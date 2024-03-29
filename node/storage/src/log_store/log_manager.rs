use crate::log_store::flow_store::{batch_iter, FlowConfig, FlowStore};
use crate::log_store::tx_store::TransactionStore;
use crate::log_store::{
    FlowRead, FlowWrite, LogStoreChunkRead, LogStoreChunkWrite, LogStoreRead, LogStoreWrite,
};
use crate::{try_option, IonianKeyValueDB};
use anyhow::{anyhow, bail, Result};
use append_merkle::{Algorithm, AppendMerkleTree, MerkleTreeRead, Sha3Algorithm};
use ethereum_types::H256;
use kvdb_rocksdb::{Database, DatabaseConfig};
use merkle_light::merkle::{log2_pow2, MerkleTree};
use merkle_tree::RawLeafSha3Algorithm;
use rayon::iter::ParallelIterator;
use rayon::prelude::ParallelSlice;
use shared_types::{
    bytes_to_chunks, compute_padded_chunk_size, compute_segment_size, Chunk, ChunkArray,
    ChunkArrayWithProof, ChunkWithProof, DataRoot, FlowProof, FlowRangeProof, Transaction,
};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, error, instrument, trace};

use super::LogStoreInner;

/// 256 Bytes
pub const ENTRY_SIZE: usize = 256;
/// 1024 Entries.
pub const PORA_CHUNK_SIZE: usize = 1024;

pub const COL_TX: u32 = 0;
pub const COL_ENTRY_BATCH: u32 = 1;
pub const COL_TX_DATA_ROOT_INDEX: u32 = 2;
pub const COL_ENTRY_BATCH_ROOT: u32 = 3;
pub const COL_TX_COMPLETED: u32 = 4;
pub const COL_MISC: u32 = 5;
pub const COL_SEAL_CONTEXT: u32 = 6;
pub const COL_NUM: u32 = 7;

type Merkle = AppendMerkleTree<H256, Sha3Algorithm>;

pub struct LogManager {
    pub(crate) db: Arc<dyn IonianKeyValueDB>,
    tx_store: TransactionStore,
    flow_store: FlowStore,
    // TODO(zz): Refactor the in-memory merkle and in-disk storage together.
    pora_chunks_merkle: Merkle,
    /// The in-memory structure of the sub merkle tree of the last chunk.
    /// The size is always less than `PORA_CHUNK_SIZE`.
    last_chunk_merkle: Merkle,
}

#[derive(Clone, Default)]
pub struct LogConfig {
    pub flow: FlowConfig,
}

impl LogStoreInner for LogManager {
    fn flow(&self) -> &dyn super::Flow {
        &self.flow_store
    }

    fn flow_mut(&mut self) -> &mut dyn super::Flow {
        &mut self.flow_store
    }
}

impl LogStoreChunkWrite for LogManager {
    fn put_chunks(&mut self, tx_seq: u64, chunks: ChunkArray) -> Result<()> {
        let tx = self
            .tx_store
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("put chunks with missing tx: tx_seq={}", tx_seq))?;
        let (chunks_for_proof, _) = compute_padded_chunk_size(tx.size as usize);
        if chunks.start_index.saturating_mul(ENTRY_SIZE as u64) + chunks.data.len() as u64
            > (chunks_for_proof * ENTRY_SIZE) as u64
        {
            bail!(
                "put chunks with data out of tx range: tx_seq={} start_index={} data_len={}",
                tx_seq,
                chunks.start_index,
                chunks.data.len()
            );
        }
        // TODO: Use another struct to avoid confusion.
        let mut flow_entry_array = chunks;
        flow_entry_array.start_index += tx.start_entry_index;
        self.append_entries(flow_entry_array)?;
        Ok(())
    }

    fn put_chunks_with_tx_hash(
        &mut self,
        tx_seq: u64,
        tx_hash: H256,
        chunks: ChunkArray,
    ) -> Result<bool> {
        let tx = self
            .tx_store
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("put chunks with missing tx: tx_seq={}", tx_seq))?;
        if tx.hash() != tx_hash {
            return Ok(false);
        }
        let (chunks_for_proof, _) = compute_padded_chunk_size(tx.size as usize);
        if chunks.start_index.saturating_mul(ENTRY_SIZE as u64) + chunks.data.len() as u64
            > (chunks_for_proof * ENTRY_SIZE) as u64
        {
            bail!(
                "put chunks with data out of tx range: tx_seq={} start_index={} data_len={}",
                tx_seq,
                chunks.start_index,
                chunks.data.len()
            );
        }
        // TODO: Use another struct to avoid confusion.
        let mut flow_entry_array = chunks;
        flow_entry_array.start_index += tx.start_entry_index;
        self.append_entries(flow_entry_array)?;
        Ok(true)
    }

    fn remove_all_chunks(&self, _tx_seq: u64) -> crate::error::Result<()> {
        todo!()
    }
}

impl LogStoreWrite for LogManager {
    #[instrument(skip(self))]
    fn put_tx(&mut self, tx: Transaction) -> Result<()> {
        debug!("put_tx: tx={:?}", tx);
        // TODO(zz): Should we validate received tx?
        self.append_subtree_list(tx.merkle_nodes.clone())?;
        // TODO(zz): tx_store and the merkle tree are not updated atomically.
        self.commit(tx.seq)?;

        if let Some(old_tx_seq) = self.tx_store.put_tx(tx.clone())?.first() {
            if self.check_tx_completed(*old_tx_seq)? {
                self.copy_tx_data(*old_tx_seq, vec![tx.seq])?;
                self.tx_store.finalize_tx(tx.seq)?;
            }
        }
        Ok(())
    }

    fn finalize_tx(&mut self, tx_seq: u64) -> Result<()> {
        let tx = self
            .tx_store
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("finalize_tx with tx missing: tx_seq={}", tx_seq))?;

        self.padding_rear_data(&tx, tx_seq)?;

        let tx_end_index = tx.start_entry_index + bytes_to_entries(tx.size);
        // TODO: Check completeness without loading all data in memory.
        // TODO: Should we double check the tx merkle root?
        if self
            .flow_store
            .get_entries(tx.start_entry_index, tx_end_index)?
            .is_some()
        {
            self.tx_store.finalize_tx(tx_seq)?;
            let same_root_seq_list = self
                .tx_store
                .get_tx_seq_list_by_data_root(&tx.data_merkle_root)?;
            // Check if there are other same-root transaction not finalized.
            if same_root_seq_list.first() == Some(&tx_seq) {
                self.copy_tx_data(tx_seq, same_root_seq_list[1..].to_vec())?;
            }
            Ok(())
        } else {
            bail!("finalize tx with data missing: tx_seq={}", tx_seq)
        }
    }

    fn finalize_tx_with_hash(&mut self, tx_seq: u64, tx_hash: H256) -> crate::error::Result<bool> {
        let tx = self
            .tx_store
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("finalize_tx with tx missing: tx_seq={}", tx_seq))?;
        if tx.hash() != tx_hash {
            return Ok(false);
        }

        self.padding_rear_data(&tx, tx_seq)?;

        let tx_end_index = tx.start_entry_index + bytes_to_entries(tx.size);
        // TODO: Check completeness without loading all data in memory.
        // TODO: Should we double check the tx merkle root?
        if self
            .flow_store
            .get_entries(tx.start_entry_index, tx_end_index)?
            .is_some()
        {
            self.tx_store.finalize_tx(tx_seq)?;
            Ok(true)
        } else {
            bail!("finalize tx with data missing: tx_seq={}", tx_seq)
        }
    }

    fn put_sync_progress(&self, progress: (u64, H256)) -> Result<()> {
        self.tx_store.put_progress(progress)
    }

    /// Return the reverted Transactions in order.
    fn revert_to(&mut self, tx_seq: u64) -> Result<Vec<Transaction>> {
        self.revert_merkle_tree(tx_seq)?;
        let start_index = self.last_chunk_start_index() * PORA_CHUNK_SIZE as u64
            + self.last_chunk_merkle.leaves() as u64;
        self.flow_store.truncate(start_index)?;
        let mut transactions = Vec::new();
        for seq in (tx_seq + 1)..self.next_tx_seq()? {
            match self.tx_store.remove_tx_by_seq_number(seq)? {
                None => {
                    // All transactions are supposed to exist before we revert them.
                    error!("reverted transactions missing after tx_seq={}", seq);
                    break;
                }
                Some(tx) => transactions.push(tx),
            }
        }
        Ok(transactions)
    }
}

impl LogStoreChunkRead for LogManager {
    fn get_chunk_by_tx_and_index(
        &self,
        tx_seq: u64,
        index: usize,
    ) -> crate::error::Result<Option<Chunk>> {
        // TODO(zz): This is not needed?
        let single_chunk_array =
            try_option!(self.get_chunks_by_tx_and_index_range(tx_seq, index, index + 1)?);
        Ok(Some(Chunk(single_chunk_array.data.as_slice().try_into()?)))
    }

    fn get_chunks_by_tx_and_index_range(
        &self,
        tx_seq: u64,
        index_start: usize,
        index_end: usize,
    ) -> crate::error::Result<Option<ChunkArray>> {
        let tx = try_option!(self.get_tx_by_seq_number(tx_seq)?);
        let start_flow_index = tx.start_entry_index + index_start as u64;
        let end_flow_index = tx.start_entry_index + index_end as u64;
        // TODO: Use another struct.
        // Set returned chunk start index as the offset in the tx data.
        let mut tx_chunk = try_option!(self
            .flow_store
            .get_entries(start_flow_index, end_flow_index)?);
        tx_chunk.start_index -= tx.start_entry_index;
        Ok(Some(tx_chunk))
    }

    fn get_chunk_by_data_root_and_index(
        &self,
        _data_root: &DataRoot,
        _index: usize,
    ) -> crate::error::Result<Option<Chunk>> {
        todo!()
    }

    fn get_chunks_by_data_root_and_index_range(
        &self,
        data_root: &DataRoot,
        index_start: usize,
        index_end: usize,
    ) -> crate::error::Result<Option<ChunkArray>> {
        let tx_seq = try_option!(self.get_tx_seq_by_data_root(data_root)?);
        self.get_chunks_by_tx_and_index_range(tx_seq, index_start, index_end)
    }

    fn get_chunk_index_list(&self, _tx_seq: u64) -> crate::error::Result<Vec<usize>> {
        todo!()
    }

    fn get_chunk_by_flow_index(
        &self,
        index: u64,
        length: u64,
    ) -> crate::error::Result<Option<ChunkArray>> {
        let start_flow_index = index;
        let end_flow_index = index + length;
        self.flow_store
            .get_entries(start_flow_index, end_flow_index)
    }
}

impl LogStoreRead for LogManager {
    fn get_tx_by_seq_number(&self, seq: u64) -> crate::error::Result<Option<Transaction>> {
        self.tx_store.get_tx_by_seq_number(seq)
    }

    fn get_tx_seq_by_data_root(&self, data_root: &DataRoot) -> crate::error::Result<Option<u64>> {
        self.tx_store.get_first_tx_seq_by_data_root(data_root)
    }

    fn get_chunk_with_proof_by_tx_and_index(
        &self,
        tx_seq: u64,
        index: usize,
    ) -> crate::error::Result<Option<ChunkWithProof>> {
        // TODO(zz): Optimize for mining.
        let single_chunk_array = try_option!(self.get_chunks_with_proof_by_tx_and_index_range(
            tx_seq,
            index,
            index + 1
        )?);
        Ok(Some(ChunkWithProof {
            chunk: Chunk(single_chunk_array.chunks.data.as_slice().try_into()?),
            proof: single_chunk_array.proof.left_proof,
        }))
    }

    fn get_chunks_with_proof_by_tx_and_index_range(
        &self,
        tx_seq: u64,
        index_start: usize,
        index_end: usize,
    ) -> crate::error::Result<Option<ChunkArrayWithProof>> {
        let tx = try_option!(self.tx_store.get_tx_by_seq_number(tx_seq)?);
        let chunks =
            try_option!(self.get_chunks_by_tx_and_index_range(tx_seq, index_start, index_end)?);
        let left_proof = self.gen_proof(tx.start_entry_index + index_start as u64, None)?;
        let right_proof = self.gen_proof(tx.start_entry_index + index_end as u64 - 1, None)?;
        Ok(Some(ChunkArrayWithProof {
            chunks,
            proof: FlowRangeProof {
                left_proof,
                right_proof,
            },
        }))
    }

    fn check_tx_completed(&self, tx_seq: u64) -> crate::error::Result<bool> {
        self.tx_store.check_tx_completed(tx_seq)
    }

    fn validate_range_proof(&self, tx_seq: u64, data: &ChunkArrayWithProof) -> Result<bool> {
        let tx = self
            .get_tx_by_seq_number(tx_seq)?
            .ok_or_else(|| anyhow!("tx missing"))?;
        let leaves = data_to_merkle_leaves(&data.chunks.data)?;
        data.proof.validate::<Sha3Algorithm>(
            &leaves,
            (data.chunks.start_index + tx.start_entry_index) as usize,
        )?;
        Ok(self.pora_chunks_merkle.check_root(&data.proof.root()))
    }

    fn get_sync_progress(&self) -> Result<Option<(u64, H256)>> {
        self.tx_store.get_progress()
    }

    fn next_tx_seq(&self) -> Result<u64> {
        self.tx_store.next_tx_seq()
    }

    fn get_proof_at_root(
        &self,
        root: &DataRoot,
        index: u64,
        length: u64,
    ) -> crate::error::Result<FlowRangeProof> {
        let left_proof = self.gen_proof(index, Some(*root))?;
        let right_proof = self.gen_proof(index + length - 1, Some(*root))?;
        Ok(FlowRangeProof {
            left_proof,
            right_proof,
        })
    }

    fn get_context(&self) -> crate::error::Result<(DataRoot, u64)> {
        Ok((
            *self.pora_chunks_merkle.root(),
            self.last_chunk_start_index() + self.last_chunk_merkle.leaves() as u64,
        ))
    }
}

impl LogManager {
    pub fn rocksdb(config: LogConfig, path: impl AsRef<Path>) -> Result<Self> {
        let mut db_config = DatabaseConfig::with_columns(COL_NUM);
        db_config.enable_statistics = true;
        let db = Arc::new(Database::open(&db_config, path)?);
        Self::new(db, config)
    }

    pub fn memorydb(config: LogConfig) -> Result<Self> {
        let db = Arc::new(kvdb_memorydb::create(COL_NUM));
        Self::new(db, config)
    }

    fn new(db: Arc<dyn IonianKeyValueDB>, config: LogConfig) -> Result<Self> {
        let tx_store = TransactionStore::new(db.clone());
        let flow_store = FlowStore::new(db.clone(), config.flow);
        let chunk_roots = flow_store.get_chunk_root_list()?;
        let next_tx_seq = tx_store.next_tx_seq()?;
        let start_tx_seq = if next_tx_seq > 0 {
            Some(next_tx_seq - 1)
        } else {
            None
        };
        let mut pora_chunks_merkle =
            Merkle::new_with_subtrees(chunk_roots, log2_pow2(PORA_CHUNK_SIZE), start_tx_seq)?;
        let last_chunk_merkle = match start_tx_seq {
            Some(tx_seq) => {
                tx_store.rebuild_last_chunk_merkle(pora_chunks_merkle.leaves(), tx_seq)?
            }
            // Initialize
            None => Merkle::new_with_depth(vec![], log2_pow2(PORA_CHUNK_SIZE) + 1, None),
        };

        debug!(
            "LogManager::new() with chunk_list_len={} start_tx_seq={:?} last_chunk={}",
            pora_chunks_merkle.leaves(),
            start_tx_seq,
            last_chunk_merkle.leaves(),
        );
        if last_chunk_merkle.leaves() != 0 {
            pora_chunks_merkle.append(*last_chunk_merkle.root());
        }
        let mut log_manager = Self {
            db,
            tx_store,
            flow_store,
            pora_chunks_merkle,
            last_chunk_merkle,
        };
        log_manager.try_initialize()?;
        Ok(log_manager)
    }

    fn try_initialize(&mut self) -> Result<()> {
        if self.pora_chunks_merkle.leaves() == 0 && self.last_chunk_merkle.leaves() == 0 {
            self.last_chunk_merkle.append(H256::zero());
            self.pora_chunks_merkle
                .update_last(*self.last_chunk_merkle.root());
        } else if self.last_chunk_merkle.leaves() != 0 {
            let last_chunk_start_index = self.last_chunk_start_index();
            let last_chunk_data = self.flow_store.get_available_entries(
                last_chunk_start_index,
                last_chunk_start_index + PORA_CHUNK_SIZE as u64,
            )?;
            for e in last_chunk_data {
                let start_index = e.start_index - last_chunk_start_index;
                for i in 0..e.data.len() / ENTRY_SIZE {
                    self.last_chunk_merkle.fill_leaf(
                        i + start_index as usize,
                        Sha3Algorithm::leaf(&e.data[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE]),
                    );
                }
            }
        }
        Ok(())
    }

    fn gen_proof(&self, flow_index: u64, maybe_root: Option<DataRoot>) -> Result<FlowProof> {
        let chunk_index = flow_index / PORA_CHUNK_SIZE as u64;
        let top_proof = match maybe_root {
            None => self.pora_chunks_merkle.gen_proof(chunk_index as usize)?,
            Some(root) => self
                .pora_chunks_merkle
                .at_root_version(&root)?
                .gen_proof(chunk_index as usize)?,
        };

        // TODO(zz): Maybe we can decide that all proofs are at the PoRA chunk level, so
        // we do not need to maintain the proof at the entry level below.
        // Condition (self.last_chunk_merkle.leaves() == 0): When last chunk size is exactly PORA_CHUNK_SIZE, proof should be generated from flow data, as last_chunk_merkle.leaves() is zero at this time
        // TODO(zz): In the current use cases, `maybe_root` is only `Some` for mining
        // and `flow_index` must be within a complete PoRA chunk. For possible future usages,
        // we'll need to find the flow length at the given root and load a partial chunk
        // if `flow_index` is in the last chunk.
        let sub_proof = if chunk_index as usize != self.pora_chunks_merkle.leaves() - 1
            || self.last_chunk_merkle.leaves() == 0
        {
            // TODO(zz）: Even if the data is incomplete, given the intermediate merkle roots
            // it's still possible to generate needed proofs. These merkle roots may be stored
            // within `EntryBatch::Incomplete`.
            let pora_chunk = self
                .flow_store
                .get_entries(
                    chunk_index * PORA_CHUNK_SIZE as u64,
                    (chunk_index + 1) * PORA_CHUNK_SIZE as u64,
                )?
                .ok_or_else(|| {
                    anyhow!(
                        "data incomplete for generating proof of index {}",
                        flow_index
                    )
                })?;

            // Tempfix: for first chunk, its data is not complete, the hash of first entry is H256::zero()
            let leaves =
                if chunk_index == 0 && pora_chunk.data.len() / ENTRY_SIZE == PORA_CHUNK_SIZE - 1 {
                    let mut leaves = vec![H256::zero()];
                    leaves.append(&mut data_to_merkle_leaves(&pora_chunk.data)?);
                    leaves
                } else {
                    data_to_merkle_leaves(&pora_chunk.data)?
                };
            let chunk_merkle = Merkle::new_with_depth(leaves, log2_pow2(PORA_CHUNK_SIZE) + 1, None);
            chunk_merkle.gen_proof(flow_index as usize % PORA_CHUNK_SIZE)?
        } else {
            match maybe_root {
                None => self
                    .last_chunk_merkle
                    .gen_proof(flow_index as usize % PORA_CHUNK_SIZE)?,
                Some(root) => self
                    .last_chunk_merkle
                    .at_root_version(&root)?
                    .gen_proof(flow_index as usize % PORA_CHUNK_SIZE)?,
            }
        };
        entry_proof(&top_proof, &sub_proof)
    }

    #[instrument(skip(self))]
    fn append_subtree_list(&mut self, merkle_list: Vec<(usize, DataRoot)>) -> Result<()> {
        if merkle_list.is_empty() {
            return Ok(());
        }

        self.pad_tx(1 << (merkle_list[0].0 - 1))?;
        for (subtree_depth, subtree_root) in merkle_list {
            let subtree_size = 1 << (subtree_depth - 1);
            if self.last_chunk_merkle.leaves() == 0 && subtree_size == PORA_CHUNK_SIZE {
                self.pora_chunks_merkle.append_subtree(1, subtree_root)?;
                self.flow_store.put_batch_root(
                    (self.pora_chunks_merkle.leaves() - 1) as u64,
                    subtree_root,
                    1,
                )?;
            } else if self.last_chunk_merkle.leaves() + subtree_size <= PORA_CHUNK_SIZE {
                self.last_chunk_merkle
                    .append_subtree(subtree_depth, subtree_root)?;
                if self.last_chunk_merkle.leaves() == subtree_size {
                    // `last_chunk_merkle` was empty, so this is a new leaf in the top_tree.
                    self.pora_chunks_merkle
                        .append_subtree(1, *self.last_chunk_merkle.root())?;
                } else {
                    self.pora_chunks_merkle
                        .update_last(*self.last_chunk_merkle.root());
                }
                if self.last_chunk_merkle.leaves() == PORA_CHUNK_SIZE {
                    self.flow_store.put_batch_root(
                        (self.pora_chunks_merkle.leaves() - 1) as u64,
                        *self.last_chunk_merkle.root(),
                        1,
                    )?;
                    self.last_chunk_merkle =
                        Merkle::new_with_depth(vec![], log2_pow2(PORA_CHUNK_SIZE) + 1, None);
                }
            } else {
                // `last_chunk_merkle` has been padded here, so a subtree should not be across
                // the chunks boundary.
                assert_eq!(self.last_chunk_merkle.leaves(), 0);
                assert!(subtree_size >= PORA_CHUNK_SIZE);
                self.pora_chunks_merkle
                    .append_subtree(subtree_depth - log2_pow2(PORA_CHUNK_SIZE), subtree_root)?;
                self.flow_store.put_batch_root(
                    (self.pora_chunks_merkle.leaves() - 1) as u64,
                    subtree_root,
                    subtree_size / PORA_CHUNK_SIZE,
                )?;
            }
        }
        Ok(())
    }

    #[instrument(skip(self))]
    fn pad_tx(&mut self, first_subtree_size: u64) -> Result<()> {
        // Check if we need to pad the flow.
        let tx_start_flow_index =
            self.last_chunk_start_index() + self.last_chunk_merkle.leaves() as u64;
        let extra = tx_start_flow_index % first_subtree_size;
        trace!(
            "before pad_tx {} {}",
            self.pora_chunks_merkle.leaves(),
            self.last_chunk_merkle.leaves()
        );
        if extra != 0 {
            let pad_data = Self::padding((first_subtree_size - extra) as usize);
            let last_chunk_pad = if self.last_chunk_merkle.leaves() == 0 {
                0
            } else {
                (PORA_CHUNK_SIZE - self.last_chunk_merkle.leaves()) * ENTRY_SIZE
            };
            if pad_data.len() < last_chunk_pad {
                self.last_chunk_merkle
                    .append_list(data_to_merkle_leaves(&pad_data)?);
                self.pora_chunks_merkle
                    .update_last(*self.last_chunk_merkle.root());
                self.flow_store.append_entries(ChunkArray {
                    data: pad_data,
                    start_index: tx_start_flow_index,
                })?;
            } else {
                if last_chunk_pad != 0 {
                    // Pad the last chunk.
                    self.last_chunk_merkle
                        .append_list(data_to_merkle_leaves(&pad_data[..last_chunk_pad])?);
                    self.pora_chunks_merkle
                        .update_last(*self.last_chunk_merkle.root());
                    self.flow_store.append_entries(ChunkArray {
                        data: pad_data[..last_chunk_pad].to_vec(),
                        start_index: tx_start_flow_index as u64,
                    })?;
                    self.last_chunk_merkle =
                        Merkle::new_with_depth(vec![], log2_pow2(PORA_CHUNK_SIZE) + 1, None);
                }

                // Pad with more complete chunks.
                let mut start_index = last_chunk_pad / ENTRY_SIZE;
                while pad_data.len() >= (start_index + PORA_CHUNK_SIZE) * ENTRY_SIZE {
                    let data = pad_data
                        [start_index * ENTRY_SIZE..(start_index + PORA_CHUNK_SIZE) * ENTRY_SIZE]
                        .to_vec();
                    self.pora_chunks_merkle
                        .append(*Merkle::new(data_to_merkle_leaves(&data)?, 0, None).root());
                    self.flow_store.append_entries(ChunkArray {
                        data,
                        start_index: start_index as u64 + tx_start_flow_index,
                    })?;
                    start_index += PORA_CHUNK_SIZE;
                }
                assert_eq!(pad_data.len(), start_index * ENTRY_SIZE);
            }
        }
        trace!(
            "after pad_tx {} {}",
            self.pora_chunks_merkle.leaves(),
            self.last_chunk_merkle.leaves()
        );
        Ok(())
    }

    fn append_entries(&mut self, flow_entry_array: ChunkArray) -> Result<()> {
        let last_chunk_start_index = self.last_chunk_start_index();
        if flow_entry_array.start_index + bytes_to_chunks(flow_entry_array.data.len()) as u64
            > last_chunk_start_index
        {
            // Update `last_chunk_merkle` with real data.
            let (chunk_start_index, flow_entry_data_index) = if flow_entry_array.start_index
                >= last_chunk_start_index
            {
                // flow_entry_array only fill last chunk
                (
                    (flow_entry_array.start_index - last_chunk_start_index) as usize,
                    0,
                )
            } else {
                // flow_entry_array fill both last and last - 1 chunk
                (
                    0,
                    (last_chunk_start_index - flow_entry_array.start_index) as usize * ENTRY_SIZE,
                )
            };

            // Since we always put tx before insert its data. Here `last_chunk_merkle` must
            // have included the data range.
            for (local_index, entry) in flow_entry_array.data[flow_entry_data_index..]
                .chunks_exact(ENTRY_SIZE)
                .enumerate()
            {
                self.last_chunk_merkle
                    .fill_leaf(chunk_start_index + local_index, Sha3Algorithm::leaf(entry));
            }
        }
        let chunk_roots = self.flow_store.append_entries(flow_entry_array)?;
        for (chunk_index, chunk_root) in chunk_roots {
            if chunk_index < self.pora_chunks_merkle.leaves() as u64 {
                self.pora_chunks_merkle
                    .fill_leaf(chunk_index as usize, chunk_root);
            } else {
                // TODO(zz): This assumption may be false in the future.
                unreachable!("We always insert tx nodes before put_chunks");
            }
        }
        Ok(())
    }

    // FIXME(zz): Implement padding.
    pub fn padding(len: usize) -> Vec<u8> {
        vec![0; len * ENTRY_SIZE]
    }

    fn last_chunk_start_index(&self) -> u64 {
        if self.pora_chunks_merkle.leaves() == 0 {
            0
        } else {
            PORA_CHUNK_SIZE as u64
                * if self.last_chunk_merkle.leaves() == 0 {
                    // The last chunk is empty and its root hash is not in `pora_chunk_merkle`,
                    // so all chunks in `pora_chunk_merkle` is complete.
                    self.pora_chunks_merkle.leaves()
                } else {
                    // The last chunk has data, so we need to exclude it from `pora_chunks_merkle`.
                    self.pora_chunks_merkle.leaves() - 1
                } as u64
        }
    }

    #[instrument(skip(self))]
    fn commit(&mut self, tx_seq: u64) -> Result<()> {
        self.pora_chunks_merkle.commit(Some(tx_seq));
        self.last_chunk_merkle.commit(Some(tx_seq));
        Ok(())
    }

    fn revert_merkle_tree(&mut self, tx_seq: u64) -> Result<()> {
        // Special case for reverting tx_seq == 0
        if tx_seq == u64::MAX {
            self.pora_chunks_merkle.reset();
            self.last_chunk_merkle.reset();
            self.try_initialize()?;
            return Ok(());
        }
        let old_leaves = self.pora_chunks_merkle.leaves();
        self.pora_chunks_merkle.revert_to(tx_seq)?;
        if old_leaves == self.pora_chunks_merkle.leaves() {
            self.last_chunk_merkle.revert_to(tx_seq)?;
        } else {
            // We are reverting to a position before the current last_chunk.
            self.last_chunk_merkle = self
                .tx_store
                .rebuild_last_chunk_merkle(self.pora_chunks_merkle.leaves() - 1, tx_seq)?;
            self.try_initialize()?;
            assert_eq!(
                Some(*self.last_chunk_merkle.root()),
                self.pora_chunks_merkle
                    .leaf_at(self.pora_chunks_merkle.leaves() - 1)?
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn flow_store(&self) -> &FlowStore {
        &self.flow_store
    }

    fn padding_rear_data(&mut self, tx: &Transaction, tx_seq: u64) -> Result<()> {
        let (chunks, _) = compute_padded_chunk_size(tx.size as usize);
        let (segments_for_proof, last_segment_size_for_proof) =
            compute_segment_size(chunks, PORA_CHUNK_SIZE);
        debug!(
            "segments_for_proof: {}, last_segment_size_for_proof: {}",
            segments_for_proof, last_segment_size_for_proof
        );

        let chunks_for_file = bytes_to_entries(tx.size) as usize;
        let (mut segments_for_file, mut last_segment_size_for_file) =
            compute_segment_size(chunks_for_file, PORA_CHUNK_SIZE);
        debug!(
            "segments_for_file: {}, last_segment_size_for_file: {}",
            segments_for_file, last_segment_size_for_file
        );

        while segments_for_file <= segments_for_proof {
            let padding_size = if segments_for_file == segments_for_proof {
                (last_segment_size_for_proof - last_segment_size_for_file) * ENTRY_SIZE
            } else {
                (PORA_CHUNK_SIZE - last_segment_size_for_file) * ENTRY_SIZE
            };

            debug!("Padding size: {}", padding_size);
            if padding_size > 0 {
                // This tx hash is guaranteed to be consistent.
                self.put_chunks_with_tx_hash(
                    tx_seq,
                    tx.hash(),
                    ChunkArray {
                        data: vec![0u8; padding_size],
                        start_index: ((segments_for_file - 1) * PORA_CHUNK_SIZE
                            + last_segment_size_for_file)
                            as u64,
                    },
                )?;
            }

            last_segment_size_for_file = 0;
            segments_for_file += 1;
        }

        Ok(())
    }

    fn copy_tx_data(&mut self, from_tx_seq: u64, to_tx_seq_list: Vec<u64>) -> Result<()> {
        // We have all the data need for this tx, so just copy them.
        let old_tx = self
            .get_tx_by_seq_number(from_tx_seq)?
            .ok_or_else(|| anyhow!("from tx missing"))?;
        let mut to_tx_offset_list = Vec::with_capacity(to_tx_seq_list.len());
        for seq in to_tx_seq_list {
            // No need to copy data for completed tx.
            if self.check_tx_completed(seq)? {
                continue;
            }
            let tx = self
                .get_tx_by_seq_number(seq)?
                .ok_or_else(|| anyhow!("to tx missing"))?;
            to_tx_offset_list.push((tx.seq, tx.start_entry_index - old_tx.start_entry_index));
        }
        if to_tx_offset_list.is_empty() {
            return Ok(());
        }
        // copy data in batches
        // TODO(zz): Do this asynchronously and keep atomicity.
        for (batch_start, batch_end) in batch_iter(
            old_tx.start_entry_index,
            old_tx.start_entry_index + old_tx.num_entries() as u64,
            PORA_CHUNK_SIZE,
        ) {
            let batch_data = self
                .get_chunk_by_flow_index(batch_start, batch_end - batch_start)?
                .ok_or_else(|| anyhow!("tx data missing"))?;
            for (_, offset) in &to_tx_offset_list {
                let mut data = batch_data.clone();
                data.start_index += offset;
                self.append_entries(data)?;
            }
        }
        // num_entries() includes the rear padding data, so no need for more padding.

        for (seq, _) in to_tx_offset_list {
            self.tx_store.finalize_tx(seq)?;
        }
        Ok(())
    }
}

/// This represents the subtree of a chunk or the whole data merkle tree.
pub type FileMerkleTree = MerkleTree<[u8; 32], RawLeafSha3Algorithm>;

#[macro_export]
macro_rules! try_option {
    ($r: ident) => {
        match $r {
            Some(v) => v,
            None => return Ok(None),
        }
    };
    ($e: expr) => {
        match $e {
            Some(v) => v,
            None => return Ok(None),
        }
    };
}

/// This should be called with input checked.
pub fn sub_merkle_tree(leaf_data: &[u8]) -> Result<FileMerkleTree> {
    Ok(FileMerkleTree::new(
        data_to_merkle_leaves(leaf_data)?
            .into_iter()
            .map(|h| h.0)
            .collect::<Vec<[u8; 32]>>(),
    ))
}

pub fn data_to_merkle_leaves(leaf_data: &[u8]) -> Result<Vec<H256>> {
    if leaf_data.len() % ENTRY_SIZE != 0 {
        bail!("merkle_tree: unmatch data size");
    }
    Ok(leaf_data
        .par_chunks_exact(ENTRY_SIZE)
        .map(Sha3Algorithm::leaf)
        .collect())
}

pub fn bytes_to_entries(size_bytes: u64) -> u64 {
    if size_bytes % ENTRY_SIZE as u64 == 0 {
        size_bytes / ENTRY_SIZE as u64
    } else {
        size_bytes / ENTRY_SIZE as u64 + 1
    }
}

fn entry_proof(top_proof: &FlowProof, sub_proof: &FlowProof) -> Result<FlowProof> {
    if top_proof.item() != sub_proof.root() {
        bail!(
            "top tree and sub tree mismatch: top_leaf={:?}, sub_root={:?}",
            top_proof.item(),
            sub_proof.root()
        );
    }
    let mut lemma = sub_proof.lemma().to_vec();
    let mut path = sub_proof.path().to_vec();
    assert!(lemma.pop().is_some());
    lemma.extend_from_slice(&top_proof.lemma()[1..]);
    path.extend_from_slice(top_proof.path());
    Ok(FlowProof::new(lemma, path))
}

pub fn split_nodes(data_size: usize) -> Vec<usize> {
    let (mut padded_chunks, chunks_next_pow2) = compute_padded_chunk_size(data_size);
    let mut next_chunk_size = chunks_next_pow2;

    let mut nodes = vec![];
    while padded_chunks > 0 {
        if padded_chunks >= next_chunk_size {
            padded_chunks -= next_chunk_size;
            nodes.push(next_chunk_size);
        }

        next_chunk_size >>= 1;
    }

    nodes
}

pub fn tx_subtree_root_list_padded(data: &[u8]) -> Vec<(usize, DataRoot)> {
    let mut root_list = Vec::new();
    let mut start_index = 0;
    let nodes = split_nodes(data.len());

    for &tree_size in nodes.iter() {
        let end = start_index + tree_size * ENTRY_SIZE;

        let submerkle_root = if start_index >= data.len() {
            sub_merkle_tree(&vec![0u8; tree_size * ENTRY_SIZE])
                .unwrap()
                .root()
        } else if end > data.len() {
            let mut pad_data = data[start_index..].to_vec();
            pad_data.append(&mut vec![0u8; end - data.len()]);
            sub_merkle_tree(&pad_data).unwrap().root()
        } else {
            sub_merkle_tree(&data[start_index..end]).unwrap().root()
        };

        root_list.push((log2_pow2(tree_size) + 1, submerkle_root.into()));
        start_index += end;
    }

    root_list
}
