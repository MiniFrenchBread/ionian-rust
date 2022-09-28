use ethereum_types::H256;
use shared_types::{
    Chunk, ChunkArray, ChunkArrayWithProof, ChunkWithProof, DataRoot, FlowRangeProof, Transaction,
};

use crate::error::Result;

mod flow_store;
mod load_chunk;
pub mod log_manager;
#[cfg(test)]
mod tests;
mod tx_store;

/// The trait to read the transactions already appended to the log.
///
/// Implementation Rationale:
/// If the stored chunk is large, we can store the proof together with the chunk.
pub trait LogStoreRead: LogStoreChunkRead {
    /// Get a transaction by its global log sequence number.
    fn get_tx_by_seq_number(&self, seq: u64) -> Result<Option<Transaction>>;

    /// Get a transaction by the data root of its data.
    fn get_tx_seq_by_data_root(&self, data_root: &DataRoot) -> Result<Option<u64>>;

    fn get_chunk_with_proof_by_tx_and_index(
        &self,
        tx_seq: u64,
        index: usize,
    ) -> Result<Option<ChunkWithProof>>;

    fn get_chunks_with_proof_by_tx_and_index_range(
        &self,
        tx_seq: u64,
        index_start: usize,
        index_end: usize,
    ) -> Result<Option<ChunkArrayWithProof>>;

    fn check_tx_completed(&self, tx_seq: u64) -> Result<bool>;

    fn next_tx_seq(&self) -> Result<u64>;

    fn get_sync_progress(&self) -> Result<Option<(u64, H256)>>;

    fn validate_range_proof(&self, tx_seq: u64, data: &ChunkArrayWithProof) -> Result<bool>;

    fn get_proof_for_flow_index_range(&self, index: u64, length: u64) -> Result<FlowRangeProof>;

    /// Return flow root and length.
    fn get_context(&self) -> Result<(DataRoot, u64)>;
}

pub trait LogStoreChunkRead {
    /// Get a data chunk by the transaction sequence number and the chunk offset in the transaction.
    /// Accessing a single chunk is mostly used for mining.
    fn get_chunk_by_tx_and_index(&self, tx_seq: u64, index: usize) -> Result<Option<Chunk>>;

    /// Get a list of continuous chunks by the transaction sequence number and an index range (`index_end` excluded).
    fn get_chunks_by_tx_and_index_range(
        &self,
        tx_seq: u64,
        index_start: usize,
        index_end: usize,
    ) -> Result<Option<ChunkArray>>;

    fn get_chunk_by_data_root_and_index(
        &self,
        data_root: &DataRoot,
        index: usize,
    ) -> Result<Option<Chunk>>;

    fn get_chunks_by_data_root_and_index_range(
        &self,
        data_root: &DataRoot,
        index_start: usize,
        index_end: usize,
    ) -> Result<Option<ChunkArray>>;

    fn get_chunk_index_list(&self, tx_seq: u64) -> Result<Vec<usize>>;

    /// Accessing chunks by absolute flow index
    fn get_chunk_by_flow_index(&self, index: u64, length: u64) -> Result<Option<ChunkArray>>;
}

pub trait LogStoreWrite: LogStoreChunkWrite {
    /// Store a data entry metadata.
    fn put_tx(&mut self, tx: Transaction) -> Result<()>;

    /// Finalize a transaction storage.
    /// This will compute and the merkle tree, check the data root, and persist a part of the merkle
    /// tree for future queries.
    ///
    /// This will return error if not all chunks are stored. But since this check can be expensive,
    /// the caller is supposed to track chunk statuses and call this after storing all the chunks.
    fn finalize_tx(&mut self, tx_seq: u64) -> Result<()>;

    /// Store the progress of synced block number and its hash.
    fn put_sync_progress(&self, progress: (u64, H256)) -> Result<()>;

    /// Revert the log state to a given tx seq.
    /// This is needed when transactions are reverted because of chain reorg.
    ///
    /// Note that in the current implementation this just reverts the merkle tree and relies on
    /// inserting new transactions to overwrite the old tx data.
    fn revert_to(&mut self, tx_seq: u64) -> Result<()>;
}

pub trait LogStoreChunkWrite {
    /// Store data chunks of a data entry.
    fn put_chunks(&mut self, tx_seq: u64, chunks: ChunkArray) -> Result<()>;

    /// Delete all chunks of a tx.
    fn remove_all_chunks(&self, tx_seq: u64) -> Result<()>;
}

pub trait Configurable {
    fn get_config(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn set_config(&self, key: &[u8], value: &[u8]) -> Result<()>;
}

pub trait LogChunkStore: LogStoreChunkRead + LogStoreChunkWrite + Send + Sync + 'static {}
impl<T: LogStoreChunkRead + LogStoreChunkWrite + Send + Sync + 'static> LogChunkStore for T {}

pub trait Store: LogStoreRead + LogStoreWrite + Configurable + Send + Sync + 'static {}
impl<T: LogStoreRead + LogStoreWrite + Configurable + Send + Sync + 'static> Store for T {}

pub trait FlowRead {
    fn get_entries(&self, index_start: u64, index_end: u64) -> Result<Option<ChunkArray>>;

    fn get_chunk_root_list(&self) -> Result<Vec<(usize, DataRoot)>>;
}

pub trait FlowWrite {
    /// Append data to the flow. `start_index` is included in `ChunkArray`, so
    /// it's possible to append arrays in any place.
    /// Return the list of completed chunks.
    fn append_entries(&self, data: ChunkArray) -> Result<Vec<(u64, DataRoot)>>;

    /// Remove all the entries after `start_index`.
    /// This is used to remove deprecated data in case of chain reorg.
    fn truncate(&self, start_index: u64) -> Result<()>;
}

pub trait Flow: FlowRead + FlowWrite {}
impl<T: FlowRead + FlowWrite> Flow for T {}
