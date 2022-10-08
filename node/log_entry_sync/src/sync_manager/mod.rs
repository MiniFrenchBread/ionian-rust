use crate::sync_manager::config::LogSyncConfig;
use crate::sync_manager::data_cache::DataCache;
use crate::sync_manager::log_entry_fetcher::{LogEntryFetcher, LogFetchProgress};
use anyhow::{bail, Result};
use ethers::prelude::Middleware;
use futures::FutureExt;
use jsonrpsee::tracing::{debug, error, trace, warn};
use shared_types::{ChunkArray, Transaction};
use std::cmp::Ordering;
use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use storage::log_store::Store;
use task_executor::{ShutdownReason, TaskExecutor};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::RwLock;

const RETRY_WAIT_MS: u64 = 500;

pub struct LogSyncManager {
    config: LogSyncConfig,
    log_fetcher: LogEntryFetcher,
    store: Arc<RwLock<dyn Store>>,
    data_cache: DataCache,

    next_tx_seq: u64,
}

impl LogSyncManager {
    pub async fn spawn(
        config: LogSyncConfig,
        executor: TaskExecutor,
        store: Arc<RwLock<dyn Store>>,
    ) -> Result<()> {
        let next_tx_seq = store.read().await.next_tx_seq()?;

        let executor_clone = executor.clone();
        let mut shutdown_sender = executor.shutdown_sender();
        // Spawn the task to sync log entries from the blockchain.
        executor.spawn(
            run_and_log(
                move || {
                    shutdown_sender
                        .try_send(ShutdownReason::Failure("log sync failure"))
                        .expect("shutdown send error")
                },
                async move {
                    let log_fetcher =
                        LogEntryFetcher::new(&config.rpc_endpoint_url, config.contract_address)
                            .await?;
                    let data_cache = DataCache::new(config.cache_config.clone());
                    let mut log_sync_manager = Self {
                        config,
                        log_fetcher,
                        next_tx_seq,
                        store,
                        data_cache,
                    };

                    // Load previous progress from db and check if chain reorg happens after restart.
                    // TODO(zz): Handle reorg instead of return.
                    let start_block_number =
                        match log_sync_manager.store.read().await.get_sync_progress()? {
                            // No previous progress, so just use config.
                            None => log_sync_manager.config.start_block_number,
                            Some((block_number, block_hash)) => {
                                match log_sync_manager
                                    .log_fetcher
                                    .provider()
                                    .get_block(block_number)
                                    .await
                                {
                                    Ok(Some(b)) => {
                                        if b.hash == Some(block_hash) {
                                            block_number
                                        } else {
                                            warn!(
                                                "log sync progress check hash fails, \
                                            block_number={:?} expect={:?} get={:?}",
                                                block_number, block_hash, b.hash
                                            );
                                            // Assume the blocks before this are not reverted.
                                            block_number.saturating_sub(
                                                log_sync_manager.config.confirmation_block_count,
                                            )
                                        }
                                    }
                                    e => {
                                        error!("log sync progress check rpc fails, e={:?}", e);
                                        bail!("log sync start error");
                                    }
                                }
                            }
                        };
                    let latest_block_number = log_sync_manager
                        .log_fetcher
                        .provider()
                        .get_block_number()
                        .await?
                        .as_u64();

                    // Start watching before recovery to ensure that no log is skipped.
                    // TODO(zz): Rate limit to avoid OOM during recovery.
                    let watch_rx = log_sync_manager
                        .log_fetcher
                        .start_watch(latest_block_number, &executor_clone);
                    let recover_rx = log_sync_manager.log_fetcher.start_recover(
                        start_block_number,
                        latest_block_number,
                        &executor_clone,
                    );
                    log_sync_manager.handle_data(recover_rx).await?;
                    // Syncing `watch_rx` is supposed to block forever.
                    log_sync_manager.handle_data(watch_rx).await?;
                    Ok(())
                },
            )
            .map(|_| ()),
            "log_sync",
        );
        Ok(())
    }

    async fn put_tx(&mut self, tx: Transaction) -> bool {
        match tx.seq.cmp(&self.next_tx_seq) {
            Ordering::Less => {
                warn!("revert for chain reorg: seq={}", tx.seq);
                match self.store.read().await.get_tx_by_seq_number(tx.seq) {
                    Ok(Some(old_tx)) => {
                        if tx == old_tx {
                            // The reverted result is the same, so just ignore.
                            return true;
                        }
                    }
                    e => {
                        error!("get old tx error: tx_seq={} e={:?}", tx.seq, e);
                        return false;
                    }
                }
                // TODO(zz): `wrapping_sub` here is a hack to handle the case of tx_seq=0.
                {
                    let store = self.store.read().await;
                    for seq in tx.seq..self.next_tx_seq {
                        if matches!(store.check_tx_completed(seq), Ok(true)) {
                            if let Ok(Some(tx)) = store.get_tx_by_seq_number(seq) {
                                // TODO(zz): Skip reading the rear padding data?
                                if let Ok(Some(data)) =
                                    store.get_chunks_by_tx_and_index_range(seq, 0, tx.num_entries())
                                {
                                    if !self.data_cache.add_data(
                                        tx.data_merkle_root,
                                        seq,
                                        data.data,
                                    ) {
                                        // TODO(zz): Data too large. Save to disk?
                                        warn!("large reverted data dropped for tx={:?}", tx);
                                    }
                                }
                            }
                        }
                    }
                }
                // TODO: Process reverted transactions.
                if let Err(e) = self.store.write().await.revert_to(tx.seq.wrapping_sub(1)) {
                    error!("revert_to fails: e={:?}", e);
                    return false;
                }
                self.next_tx_seq = tx.seq;
                self.put_tx_inner(tx).await
            }
            Ordering::Equal => {
                debug!("log entry sync get entry: {:?}", tx);
                self.put_tx_inner(tx).await
            }
            Ordering::Greater => {
                error!(
                    "Unexpected transaction skip: next={} get={}",
                    self.next_tx_seq, tx.seq
                );
                false
            }
        }
    }

    async fn handle_data(&mut self, mut rx: UnboundedReceiver<LogFetchProgress>) -> Result<()> {
        while let Some(data) = rx.recv().await {
            trace!("handle_data: data={:?}", data);
            match data {
                LogFetchProgress::SyncedBlock(progress) => {
                    self.store.write().await.put_sync_progress(progress)?;
                }
                LogFetchProgress::Transaction(tx) => {
                    if !self.put_tx(tx).await {
                        // Unexpected error.
                        error!("log sync write error");
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn put_tx_inner(&mut self, tx: Transaction) -> bool {
        if let Err(e) = self.store.write().await.put_tx(tx.clone()) {
            error!("put_tx error: e={:?}", e);
            false
        } else {
            if let Some(data) = self.data_cache.pop_data(&tx.data_merkle_root) {
                let mut store = self.store.write().await;
                if let Err(e) = store
                    .put_chunks(
                        tx.seq,
                        ChunkArray {
                            data,
                            start_index: 0,
                        },
                    )
                    .and_then(|_| store.finalize_tx(tx.seq))
                {
                    error!("put_tx data error: e={:?}", e);
                    return false;
                }
            }
            self.data_cache.garbage_collect(self.next_tx_seq);
            self.next_tx_seq += 1;
            true
        }
    }
}

async fn run_and_log<R, E>(
    mut on_error: impl FnMut(),
    f: impl Future<Output = std::result::Result<R, E>> + Send,
) -> Option<R>
where
    E: Debug,
{
    match f.await {
        Err(e) => {
            error!("log sync failure: e={:?}", e);
            on_error();
            None
        }
        Ok(r) => Some(r),
    }
}

async fn repeat_run_and_log<R, E, F>(f: impl Fn() -> F) -> R
where
    E: Debug,
    F: Future<Output = std::result::Result<R, E>> + Send,
{
    loop {
        if let Some(r) = run_and_log(|| {}, f()).await {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(RETRY_WAIT_MS)).await;
    }
}

pub(crate) mod config;
mod data_cache;
mod log_entry_fetcher;
