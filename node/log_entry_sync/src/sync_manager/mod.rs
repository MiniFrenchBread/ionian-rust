use crate::sync_manager::config::LogSyncConfig;
use crate::sync_manager::log_entry_fetcher::{LogEntryFetcher, LogFetchProgress};
use anyhow::{bail, Result};
use ethers::prelude::Middleware;
use futures::FutureExt;
use jsonrpsee::tracing::{debug, error, trace, warn};
use shared_types::Transaction;
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
                    let mut log_sync_manager = Self {
                        config,
                        log_fetcher,
                        next_tx_seq,
                        store,
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
                // FIXME(zz): Handle reorg after restart.
                debug!("revert for chain reorg: seq={}", tx.seq);
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
                if let Err(e) = self.store.write().await.revert_to(tx.seq.wrapping_sub(1)) {
                    error!("revert_to fails: e={:?}", e);
                    return false;
                }
                self.next_tx_seq = tx.seq;
                if let Err(e) = self.store.write().await.put_tx(tx) {
                    error!("put_tx error: e={:?}", e);
                    false
                } else {
                    self.next_tx_seq += 1;
                    true
                }
            }
            Ordering::Equal => {
                debug!("log entry sync get entry: {:?}", tx);
                if let Err(e) = self.store.write().await.put_tx(tx) {
                    error!("put_tx error: e={:?}", e);
                    false
                } else {
                    self.next_tx_seq += 1;
                    true
                }
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
mod log_entry_fetcher;
