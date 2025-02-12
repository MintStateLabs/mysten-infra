// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
#![warn(
    future_incompatible,
    nonstandard_style,
    rust_2018_idioms,
    rust_2021_compatibility
)]

use eyre::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    cmp::Eq,
    collections::{HashMap, VecDeque},
    hash::Hash,
};
use tokio::sync::{
    mpsc::{channel, Sender},
    oneshot,
};

pub mod traits;
pub use traits::Map;
pub mod rocks;
#[cfg(test)]
#[path = "tests/store_tests.rs"]
pub mod store_tests;

pub type StoreError = rocks::TypedStoreError;
type StoreResult<T> = Result<T, StoreError>;

pub enum StoreCommand<Key, Value> {
    Write(Key, Value),
    WriteAll(Vec<(Key, Value)>, oneshot::Sender<StoreResult<()>>),
    Delete(Key),
    DeleteAll(Vec<Key>, oneshot::Sender<StoreResult<()>>),
    Read(Key, oneshot::Sender<StoreResult<Option<Value>>>),
    ReadAll(Vec<Key>, oneshot::Sender<StoreResult<Vec<Option<Value>>>>),
    NotifyRead(Key, oneshot::Sender<StoreResult<Option<Value>>>),
}

#[derive(Clone)]
pub struct Store<K, V> {
    channel: Sender<StoreCommand<K, V>>,
}

impl<Key, Value> Store<Key, Value>
where
    Key: Hash + Eq + Serialize + DeserializeOwned + Send + 'static,
    Value: Serialize + DeserializeOwned + Send + Clone + 'static,
{
    pub fn new(keyed_db: rocks::DBMap<Key, Value>) -> Self {
        let mut obligations = HashMap::<Key, VecDeque<oneshot::Sender<_>>>::new();
        let (tx, mut rx) = channel(100);
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    StoreCommand::Write(key, value) => {
                        let _ = keyed_db.insert(&key, &value);
                        if let Some(mut senders) = obligations.remove(&key) {
                            while let Some(s) = senders.pop_front() {
                                let _ = s.send(Ok(Some(value.clone())));
                            }
                        }
                    }
                    StoreCommand::WriteAll(key_values, sender) => {
                        let response =
                            keyed_db.multi_insert(key_values.iter().map(|(k, v)| (k, v)));

                        if response.is_ok() {
                            for (key, _) in key_values {
                                if let Some(mut senders) = obligations.remove(&key) {
                                    while let Some(s) = senders.pop_front() {
                                        let _ = s.send(Ok(None));
                                    }
                                }
                            }
                        }
                        let _ = sender.send(response);
                    }
                    StoreCommand::Delete(key) => {
                        let _ = keyed_db.remove(&key);
                        if let Some(mut senders) = obligations.remove(&key) {
                            while let Some(s) = senders.pop_front() {
                                let _ = s.send(Ok(None));
                            }
                        }
                    }
                    StoreCommand::DeleteAll(keys, sender) => {
                        let response = keyed_db.multi_remove(keys.iter());
                        // notify the obligations only when the delete was successful
                        if response.is_ok() {
                            for key in keys {
                                if let Some(mut senders) = obligations.remove(&key) {
                                    while let Some(s) = senders.pop_front() {
                                        let _ = s.send(Ok(None));
                                    }
                                }
                            }
                        }
                        let _ = sender.send(response);
                    }
                    StoreCommand::Read(key, sender) => {
                        let response = keyed_db.get(&key);
                        let _ = sender.send(response);
                    }
                    StoreCommand::ReadAll(keys, sender) => {
                        let response = keyed_db.multi_get(keys.as_slice());
                        let _ = sender.send(response);
                    }
                    StoreCommand::NotifyRead(key, sender) => {
                        let response = keyed_db.get(&key);
                        if let Ok(Some(_)) = response {
                            let _ = sender.send(response);
                        } else {
                            obligations
                                .entry(key)
                                .or_insert_with(VecDeque::new)
                                .push_back(sender)
                        }
                    }
                }
            }
        });
        Self { channel: tx }
    }
}

impl<Key, Value> Store<Key, Value>
where
    Key: Serialize + DeserializeOwned + Send,
    Value: Serialize + DeserializeOwned + Send,
{
    pub async fn write(&self, key: Key, value: Value) {
        if let Err(e) = self.channel.send(StoreCommand::Write(key, value)).await {
            panic!("Failed to send Write command to store: {e}");
        }
    }

    /// Atomically writes all the key-value pairs in storage.
    /// If the operation is successful, then the result will be a non
    /// error empty result. Otherwise the error is returned.
    pub async fn write_all(
        &self,
        key_value_pairs: impl IntoIterator<Item = (Key, Value)>,
    ) -> StoreResult<()> {
        let (sender, receiver) = oneshot::channel();
        if let Err(e) = self
            .channel
            .send(StoreCommand::WriteAll(
                key_value_pairs.into_iter().collect(),
                sender,
            ))
            .await
        {
            panic!("Failed to send WriteAll command to store: {e}");
        }
        receiver
            .await
            .expect("Failed to receive reply to WriteAll command from store")
    }

    pub async fn remove(&self, key: Key) {
        if let Err(e) = self.channel.send(StoreCommand::Delete(key)).await {
            panic!("Failed to send Delete command to store: {e}");
        }
    }

    /// Atomically removes all the data referenced by the provided keys.
    /// If the operation is successful, then the result will be a non
    /// error empty result. Otherwise the error is returned.
    pub async fn remove_all(&self, keys: impl IntoIterator<Item = Key>) -> StoreResult<()> {
        let (sender, receiver) = oneshot::channel();
        if let Err(e) = self
            .channel
            .send(StoreCommand::DeleteAll(keys.into_iter().collect(), sender))
            .await
        {
            panic!("Failed to send DeleteAll command to store: {e}");
        }
        receiver
            .await
            .expect("Failed to receive reply to RemoveAll command from store")
    }

    pub async fn read(&self, key: Key) -> StoreResult<Option<Value>> {
        let (sender, receiver) = oneshot::channel();
        if let Err(e) = self.channel.send(StoreCommand::Read(key, sender)).await {
            panic!("Failed to send Read command to store: {e}");
        }
        receiver
            .await
            .expect("Failed to receive reply to Read command from store")
    }

    /// Fetches all the values for the provided keys.
    pub async fn read_all(
        &self,
        keys: impl IntoIterator<Item = Key>,
    ) -> StoreResult<Vec<Option<Value>>> {
        let (sender, receiver) = oneshot::channel();
        if let Err(e) = self
            .channel
            .send(StoreCommand::ReadAll(keys.into_iter().collect(), sender))
            .await
        {
            panic!("Failed to send ReadAll command to store: {e}");
        }
        receiver
            .await
            .expect("Failed to receive reply to ReadAll command from store")
    }

    pub async fn notify_read(&self, key: Key) -> StoreResult<Option<Value>> {
        let (sender, receiver) = oneshot::channel();
        if let Err(e) = self
            .channel
            .send(StoreCommand::NotifyRead(key, sender))
            .await
        {
            panic!("Failed to send NotifyRead command to store: {e}");
        }
        receiver
            .await
            .expect("Failed to receive reply to NotifyRead command from store")
    }
}
