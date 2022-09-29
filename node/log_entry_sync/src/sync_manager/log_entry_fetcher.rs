use crate::rpc_proxy::ContractAddress;
use crate::sync_manager::{repeat_run_and_log, RETRY_WAIT_MS};
use anyhow::{anyhow, Result};
use append_merkle::{Algorithm, Sha3Algorithm};
use contract_interface::{IonianFlow, SubmissionFilter};
use ethers::abi::RawLog;
use ethers::prelude::{BlockNumber, EthLogDecode, Http, Log, Middleware, Provider, U256};
use ethers::providers::FilterKind;
use ethers::types::H256;
use futures::StreamExt;
use jsonrpsee::tracing::{debug, error, info};
use shared_types::{DataRoot, Transaction};
use std::sync::Arc;
use std::time::Duration;
use task_executor::TaskExecutor;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const LOG_PAGE_SIZE: u64 = 1000;

pub struct LogEntryFetcher {
    contract_address: ContractAddress,
    provider: Arc<Provider<Http>>,
}

impl LogEntryFetcher {
    pub async fn new(url: &str, contract_address: ContractAddress) -> Result<Self> {
        let provider = Arc::new(Provider::try_from(url)?);
        // TODO: `error` types are removed from the ABI json file.
        Ok(Self {
            contract_address,
            provider,
        })
    }

    pub fn start_recover(
        &self,
        start_block_number: u64,
        end_block_number: u64,
        executor: &TaskExecutor,
    ) -> UnboundedReceiver<LogFetchProgress> {
        let provider = self.provider.clone();
        let (recover_tx, recover_rx) = tokio::sync::mpsc::unbounded_channel();
        let contract = IonianFlow::new(self.contract_address, provider.clone());

        executor.spawn(
            async move {
                let mut progress = start_block_number;
                let mut filter = contract
                    .submission_filter()
                    .from_block(progress)
                    .to_block(end_block_number)
                    .filter;
                let mut stream = provider.get_logs_paginated(&filter, LOG_PAGE_SIZE);
                debug!("start_recover starts, start={}", start_block_number);
                while let Some(maybe_log) = stream.next().await {
                    match maybe_log {
                        Ok(log) => {
                            let sync_progress =
                                if log.block_hash.is_some() && log.block_number.is_some() {
                                    let synced_block = LogFetchProgress::SyncedBlock((
                                        log.block_number.unwrap().as_u64(),
                                        log.block_hash.unwrap(),
                                    ));
                                    progress = log.block_number.unwrap().as_u64();
                                    Some(synced_block)
                                } else {
                                    None
                                };

                            match SubmissionFilter::decode_log(&RawLog {
                                topics: log.topics,
                                data: log.data.to_vec(),
                            }) {
                                Ok(event) => {
                                    if let Err(e) = recover_tx
                                        .send(submission_event_to_transaction(event))
                                        .and_then(|_| match sync_progress {
                                            Some(b) => recover_tx.send(b),
                                            None => Ok(()),
                                        })
                                    {
                                        error!("send error: e={:?}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("log decode error: e={:?}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("log query error: e={:?}", e);
                            filter = filter.from_block(progress);
                            stream = provider.get_logs_paginated(&filter, LOG_PAGE_SIZE);
                            tokio::time::sleep(Duration::from_millis(RETRY_WAIT_MS)).await;
                        }
                    }
                }
            },
            "log recover",
        );
        recover_rx
    }

    pub fn start_watch(
        &self,
        start_block_number: u64,
        executor: &TaskExecutor,
    ) -> UnboundedReceiver<LogFetchProgress> {
        let (watch_tx, watch_rx) = tokio::sync::mpsc::unbounded_channel();
        let contract = IonianFlow::new(self.contract_address, self.provider.clone());
        let provider = self.provider.clone();
        executor.spawn(
            async move {
                let mut filter = contract
                    .submission_filter()
                    .from_block(start_block_number)
                    .filter;
                debug!("start_watch starts, start={}", start_block_number);
                let mut filter_id =
                    repeat_run_and_log(|| provider.new_filter(FilterKind::Logs(&filter))).await;
                let mut progress = start_block_number;

                loop {
                    match Self::watch_loop(provider.as_ref(), filter_id, &watch_tx).await {
                        Err(e) => {
                            error!("log sync watch error: e={:?}", e);
                            filter = filter.from_block(progress);
                            filter_id = repeat_run_and_log(|| {
                                provider.new_filter(FilterKind::Logs(&filter))
                            })
                            .await;
                        }
                        Ok(Some(p)) => {
                            progress = p;
                            info!("log sync to block number {:?}", progress);
                        }
                        Ok(None) => {
                            error!(
                                "log sync gets entries without progress? old_progress={}",
                                progress
                            )
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(RETRY_WAIT_MS)).await;
                }
            },
            "log watch",
        );
        watch_rx
    }

    async fn watch_loop(
        provider: &Provider<Http>,
        filter_id: U256,
        watch_tx: &UnboundedSender<LogFetchProgress>,
    ) -> Result<Option<u64>> {
        let latest_block = provider
            .get_block(BlockNumber::Latest)
            .await?
            .ok_or_else(|| anyhow!("None for latest block"))?;
        let logs: Vec<Log> = provider.get_filter_changes(filter_id).await?;
        for log in logs {
            // Reverted log should not be processed. Since we revert back to a previous tx_seq
            // directly, we also do not need to notify with reverted logs.
            if !log.removed.unwrap_or(false) {
                // TODO(zz): Log parse error means logs might be lost here.
                let tx = SubmissionFilter::decode_log(&RawLog {
                    topics: log.topics,
                    data: log.data.to_vec(),
                })?;
                watch_tx.send(submission_event_to_transaction(tx))?;
            }
        }
        let progress = if latest_block.hash.is_some() && latest_block.number.is_some() {
            Some((
                latest_block.number.unwrap().as_u64(),
                latest_block.hash.unwrap(),
            ))
        } else {
            None
        };
        if let Some(p) = &progress {
            watch_tx.send(LogFetchProgress::SyncedBlock(*p))?;
        }
        Ok(progress.map(|p| p.0))
    }

    pub fn provider(&self) -> &Provider<Http> {
        self.provider.as_ref()
    }
}

#[derive(Debug)]
pub enum LogFetchProgress {
    SyncedBlock((u64, H256)),
    Transaction(Transaction),
}

fn submission_event_to_transaction(e: SubmissionFilter) -> LogFetchProgress {
    LogFetchProgress::Transaction(Transaction {
        stream_ids: vec![],
        data: vec![],
        data_merkle_root: nodes_to_root(&e.submission.2),
        merkle_nodes: e
            .submission
            .2
            .iter()
            // the submission height is the height of the root node starting from height 0.
            .map(|(root, height)| (height.as_usize() + 1, root.into()))
            .collect(),
        start_entry_index: e.start_pos.as_u64(),
        size: e.submission.0.as_u64(),
        seq: e.submission_index.as_u64(),
    })
}

fn nodes_to_root(node_list: &Vec<([u8; 32], U256)>) -> DataRoot {
    let mut root: DataRoot = node_list.last().expect("not empty").0.into();
    for (next_node, _) in node_list[..node_list.len() - 1].iter().rev() {
        root = Sha3Algorithm::parent(&next_node.into(), &root);
    }
    root
}
