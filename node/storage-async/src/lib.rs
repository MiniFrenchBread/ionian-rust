#[macro_use]
extern crate tracing;

use anyhow::bail;
use shared_types::{Chunk, ChunkArray, ChunkArrayWithProof, DataRoot, Transaction};
use std::sync::Arc;
use storage::{error, error::Result, log_store::Store as LogStore};
use task_executor::TaskExecutor;
use tokio::sync::{oneshot, RwLock};

/// The name of the worker tokio tasks.
const WORKER_TASK_NAME: &str = "async_storage_worker";

macro_rules! delegate {
    (fn $name:tt($($v:ident: $t:ty),*)) => {
        delegate!($name($($v: $t),*) -> ());
    };

    (fn $name:tt($($v:ident: $t:ty),*) -> $ret:ty) => {
        pub async fn $name(&self, $($v: $t),*) -> $ret {
            self.spawn(move |store| store.$name($($v),*)).await
        }
    };
}

#[derive(Clone)]
pub struct Store {
    /// Log and transaction storage.
    store: Arc<RwLock<dyn LogStore>>,

    /// Tokio executor for spawning worker tasks.
    executor: TaskExecutor,
}

impl Store {
    pub fn new(store: Arc<RwLock<dyn LogStore>>, executor: TaskExecutor) -> Self {
        Store { store, executor }
    }

    delegate!(fn check_tx_completed(tx_seq: u64) -> Result<bool>);
    delegate!(fn get_chunk_by_tx_and_index(tx_seq: u64, index: usize) -> Result<Option<Chunk>>);
    delegate!(fn get_chunks_by_tx_and_index_range(tx_seq: u64, index_start: usize, index_end: usize) -> Result<Option<ChunkArray>>);
    delegate!(fn get_chunks_with_proof_by_tx_and_index_range(tx_seq: u64, index_start: usize, index_end: usize) -> Result<Option<ChunkArrayWithProof>>);
    delegate!(fn get_tx_by_seq_number(seq: u64) -> Result<Option<Transaction>>);
    delegate!(fn put_chunks(tx_seq: u64, chunks: ChunkArray) -> Result<()>);
    delegate!(fn get_chunk_by_flow_index(index: u64, length: u64) -> Result<Option<ChunkArray>>);
    delegate!(fn finalize_tx(tx_seq: u64) -> Result<()>);

    pub async fn get_tx_seq_by_data_root(&self, data_root: &DataRoot) -> Result<Option<u64>> {
        let root = *data_root;
        self.spawn(move |store| store.get_tx_seq_by_data_root(&root))
            .await
    }

    async fn spawn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut dyn LogStore) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        let (tx, rx) = oneshot::channel();

        self.executor.spawn(
            async move {
                // FIXME(zz): Not all functions need `write`. Refactor store usage.
                let res = f(&mut *store.write().await);

                if tx.send(res).is_err() {
                    error!("Unable to complete async storage operation: the receiver dropped");
                }
            },
            WORKER_TASK_NAME,
        );

        rx.await
            .unwrap_or_else(|_| bail!(error::Error::Custom("Receiver error".to_string())))
    }

    // FIXME(zz): Refactor the lock and async call here.
    pub fn get_store(&self) -> &RwLock<dyn LogStore> {
        self.store.as_ref()
    }
}
