use contract_interface::{EpochRangeWithContextDigest, IonianFlow};
use ethereum_types::H256;
use ionian_spec::SECTORS_PER_SEAL;
use std::collections::BTreeMap;
use std::sync::Arc;
use storage::error::Result;
use storage::log_store::{SealAnswer, SealTask, Store};
use task_executor::TaskExecutor;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};

use crate::config::{MineServiceMiddleware, MinerConfig};

pub struct Sealer {
    flow_contract: IonianFlow<MineServiceMiddleware>,
    store: Arc<RwLock<dyn Store>>,
    context_cache: BTreeMap<u128, EpochRangeWithContextDigest>,
    last_context_flow_length: u64,
    miner_id: H256,
}

impl Sealer {
    pub fn spawn(
        executor: TaskExecutor,
        provider: Arc<MineServiceMiddleware>,
        store: Arc<RwLock<dyn Store>>,
        config: &MinerConfig,
    ) {
        let flow_contract = IonianFlow::new(config.flow_address, provider);
        let sealer = Sealer {
            flow_contract,
            store,
            context_cache: Default::default(),
            last_context_flow_length: 0,
            miner_id: config.miner_id,
        };

        executor.spawn(async move { Box::pin(sealer.start()).await }, "data_sealer");
    }

    async fn start(mut self) {
        let db_checker_throttle = sleep(Duration::from_secs(0));
        tokio::pin!(db_checker_throttle);

        let contract_checker_throttle = sleep(Duration::from_secs(0));
        tokio::pin!(contract_checker_throttle);

        loop {
            tokio::select! {
                biased;

                () = &mut contract_checker_throttle, if !contract_checker_throttle.is_elapsed() => {
                }

                () = &mut db_checker_throttle, if !db_checker_throttle.is_elapsed() => {
                }

                _ = async {}, if contract_checker_throttle.is_elapsed() => {
                    if let Err(err) = self.update_flow_length().await{
                        warn!("Fetch onchain context failed {:?}", err);
                    }
                    contract_checker_throttle.as_mut().reset(Instant::now() + Duration::from_secs(5));
                }

                _ = async {}, if db_checker_throttle.is_elapsed() => {
                    match self.seal_iteration().await {
                        Ok(true) => {},
                        Ok(false) => {db_checker_throttle.as_mut().reset(Instant::now() + Duration::from_secs(1));}
                        Err(err) => {
                            warn!("Seal iteration failed {:?}", err);
                            db_checker_throttle.as_mut().reset(Instant::now() + Duration::from_secs(5));
                        }
                    }
                }
            }
        }
    }

    async fn update_flow_length(&mut self) -> Result<()> {
        let recent_context = self.flow_contract.make_context_with_result().call().await?;
        debug!("Recent context is {:?}", recent_context);
        let recent_flow_length = recent_context.flow_length.as_u64();
        if self.last_context_flow_length < recent_flow_length {
            let epoch_range = self
                .flow_contract
                .get_epoch_range(recent_context.digest)
                .call()
                .await?;
            self.context_cache.insert(
                epoch_range.start,
                EpochRangeWithContextDigest {
                    start: epoch_range.start,
                    end: epoch_range.end,
                    digest: recent_context.digest,
                },
            );
            self.last_context_flow_length = recent_flow_length;
            info!("Update sealable flow length: {}", recent_flow_length)
        }
        Ok(())
    }

    async fn fetch_context(&mut self, seal_index: u64) -> Result<Option<(H256, u64)>> {
        let last_entry = ((seal_index as usize + 1) * SECTORS_PER_SEAL - 1) as u128;
        if self.last_context_flow_length <= last_entry as u64 {
            return Ok(None);
        }

        if let Some((_, context)) = self.context_cache.range(..=last_entry).rev().next() {
            if context.start <= last_entry && context.end > last_entry {
                return Ok(Some((
                    H256(context.digest),
                    context.end as u64 / SECTORS_PER_SEAL as u64,
                )));
            }
        }

        let context = match self
            .flow_contract
            .query_context_at_position(last_entry)
            .call()
            .await
        {
            Ok(context) => context,
            Err(err) => {
                info!("Error when fetch entries {:?}", err);
                return Ok(None);
            }
        };
        info!(
            "Fetch new context: range {} -> {}",
            context.start, context.end
        );
        self.context_cache.insert(context.start, context.clone());

        Ok(Some((
            H256(context.digest),
            context.end as u64 / SECTORS_PER_SEAL as u64,
        )))
    }

    async fn fetch_task(&self) -> Result<Option<Vec<SealTask>>> {
        let seal_index_max = self.last_context_flow_length as usize / SECTORS_PER_SEAL;
        self.store
            .read()
            .await
            .flow()
            .pull_seal_chunk(seal_index_max)
    }

    async fn submit_answer(&self, answers: Vec<SealAnswer>) -> Result<()> {
        self.store
            .write()
            .await
            .flow_mut()
            .submit_seal_result(answers)
    }

    async fn seal_iteration(&mut self) -> Result<bool> {
        let tasks = match self.fetch_task().await? {
            Some(tasks) if !tasks.is_empty() => tasks,
            _ => {
                return Ok(false);
            }
        };

        debug!(
            "Get seal tasks at seal index {:?}",
            tasks.iter().map(|x| x.seal_index).collect::<Vec<u64>>()
        );

        let mut answers = Vec::with_capacity(tasks.len());

        for task in tasks {
            let (context_digest, end_seal) =
                if let Some(context) = self.fetch_context(task.seal_index).await? {
                    context
                } else {
                    debug!("Index {} is not ready for seal", task.seal_index);
                    continue;
                };
            let mut data = task.non_sealed_data;
            ionian_seal::seal(
                &mut data,
                &self.miner_id,
                &context_digest,
                task.seal_index * SECTORS_PER_SEAL as u64,
            );
            answers.push(SealAnswer {
                seal_index: task.seal_index,
                version: task.version,
                sealed_data: data,
                miner_id: self.miner_id,
                seal_context: context_digest,
                context_end_seal: end_seal,
            });
        }

        self.submit_answer(answers).await?;

        Ok(true)
    }
}