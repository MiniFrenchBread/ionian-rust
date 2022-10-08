use crate::error::Error;
use crate::log_store::log_manager::{
    data_to_merkle_leaves, sub_merkle_tree, COL_MISC, COL_TX, COL_TX_COMPLETED,
    COL_TX_DATA_ROOT_INDEX, ENTRY_SIZE, PORA_CHUNK_SIZE,
};
use crate::{try_option, IonianKeyValueDB, LogManager};
use anyhow::{anyhow, Result};
use append_merkle::{AppendMerkleTree, MerkleTreeRead, Sha3Algorithm};
use ethereum_types::H256;
use merkle_light::merkle::log2_pow2;
use shared_types::{DataRoot, Transaction};
use ssz::{Decode, Encode};
use std::cmp;
use std::sync::Arc;
use tracing::instrument;

const LOG_SYNC_PROGRESS_KEY: &str = "log_sync_progress";

pub struct TransactionStore {
    kvdb: Arc<dyn IonianKeyValueDB>,
}

impl TransactionStore {
    pub fn new(kvdb: Arc<dyn IonianKeyValueDB>) -> Self {
        Self { kvdb }
    }

    #[instrument(skip(self))]
    pub fn put_tx(&self, mut tx: Transaction) -> Result<()> {
        let mut db_tx = self.kvdb.transaction();

        if !tx.data.is_empty() {
            tx.size = tx.data.len() as u64;
            let mut padded_data = tx.data.clone();
            let extra = tx.data.len() % ENTRY_SIZE;
            if extra != 0 {
                padded_data.append(&mut vec![0u8; ENTRY_SIZE - extra]);
            }
            let data_root = sub_merkle_tree(&padded_data)?.root();
            tx.data_merkle_root = data_root.into();
        }

        db_tx.put(COL_TX, &tx.seq.to_be_bytes(), &tx.as_ssz_bytes());
        if self
            .get_tx_seq_by_data_root(&tx.data_merkle_root)?
            .is_none()
        {
            db_tx.put(
                COL_TX_DATA_ROOT_INDEX,
                tx.data_merkle_root.as_bytes(),
                &tx.seq.to_be_bytes(),
            );
        }

        self.kvdb.write(db_tx)?;
        Ok(())
    }

    pub fn get_tx_by_seq_number(&self, seq: u64) -> Result<Option<Transaction>> {
        let value = try_option!(self.kvdb.get(COL_TX, &seq.to_be_bytes())?);
        let tx = Transaction::from_ssz_bytes(&value).map_err(Error::from)?;
        Ok(Some(tx))
    }

    pub fn remove_tx_by_seq_number(&self, seq: u64) -> Result<Option<Transaction>> {
        let tx = try_option!(self.get_tx_by_seq_number(seq)?);
        let mut db_tx = self.kvdb.transaction();
        db_tx.delete(COL_TX, &seq.to_be_bytes());
        db_tx.delete(COL_TX_COMPLETED, &seq.to_be_bytes());
        // We only remove tx when the blockchain reorgs.
        // The data root is always mapped to the first tx with the data. If it's reverted, all
        // data after it will also be reverted, so we should remove this index.
        if try_option!(self.get_tx_seq_by_data_root(&tx.data_merkle_root)?) == seq {
            db_tx.delete(COL_TX_DATA_ROOT_INDEX, tx.data_merkle_root.as_bytes());
        }
        self.kvdb.write(db_tx)?;
        Ok(Some(tx))
    }

    pub fn get_tx_seq_by_data_root(&self, data_root: &DataRoot) -> Result<Option<u64>> {
        let value = try_option!(self
            .kvdb
            .get(COL_TX_DATA_ROOT_INDEX, data_root.as_bytes())?);
        Ok(Some(decode_tx_seq(&value)?))
    }

    #[instrument(skip(self))]
    pub fn finalize_tx(&self, tx_seq: u64) -> Result<()> {
        Ok(self
            .kvdb
            .put(COL_TX_COMPLETED, &tx_seq.to_be_bytes(), &[0])?)
    }

    pub fn check_tx_completed(&self, tx_seq: u64) -> Result<bool> {
        Ok(self.kvdb.has_key(COL_TX_COMPLETED, &tx_seq.to_be_bytes())?)
    }

    pub fn next_tx_seq(&self) -> Result<u64> {
        // TODO: `kvdb` and `kvdb-rocksdb` does not support `seek_to_last` yet.
        // We'll need to fork it or use another wrapper for a better performance in this.
        self.kvdb
            .iter(COL_TX)
            .last()
            .map(|(k, _)| decode_tx_seq(k.as_ref()).map(|seq| seq + 1))
            .unwrap_or(Ok(0))
    }

    #[instrument(skip(self))]
    pub fn put_progress(&self, progress: (u64, H256)) -> Result<()> {
        Ok(self.kvdb.put(
            COL_MISC,
            LOG_SYNC_PROGRESS_KEY.as_bytes(),
            &progress.as_ssz_bytes(),
        )?)
    }

    #[instrument(skip(self))]
    pub fn get_progress(&self) -> Result<Option<(u64, H256)>> {
        Ok(Some(
            <(u64, H256)>::from_ssz_bytes(&try_option!(self
                .kvdb
                .get(COL_MISC, LOG_SYNC_PROGRESS_KEY.as_bytes())?))
            .map_err(Error::from)?,
        ))
    }

    /// Build the merkle tree at `pora_chunk_index` with the data before (including) `tx_seq`.
    /// This first rebuild the tree with the tx root nodes lists by repeatedly checking previous
    /// until we reach the start of this chunk.
    ///
    /// Note that this can only be called with the last chunk after some transaction is committed,
    /// otherwise the start of this chunk might be within some tx subtree and this will panic.
    // TODO(zz): Fill the last chunk with data.
    pub fn rebuild_last_chunk_merkle(
        &self,
        pora_chunk_index: usize,
        mut tx_seq: u64,
    ) -> Result<AppendMerkleTree<H256, Sha3Algorithm>> {
        let last_chunk_start_index = pora_chunk_index as u64 * PORA_CHUNK_SIZE as u64;
        let mut tx_list = Vec::new();
        // Find the first tx within the last chunk.
        loop {
            let tx = self.get_tx_by_seq_number(tx_seq)?.expect("tx not removed");
            match tx.start_entry_index.cmp(&last_chunk_start_index) {
                cmp::Ordering::Greater => {
                    tx_list.push((tx_seq, tx.merkle_nodes));
                }
                cmp::Ordering::Equal => {
                    tx_list.push((tx_seq, tx.merkle_nodes));
                    break;
                }
                cmp::Ordering::Less => {
                    // The transaction data crosses a chunk, so we need to find the subtrees
                    // within the last chunk.
                    let mut start_index = tx.start_entry_index;
                    let mut first_index = None;
                    for (i, (depth, _)) in tx.merkle_nodes.iter().enumerate() {
                        start_index += 1 << (depth - 1);
                        if start_index == last_chunk_start_index {
                            first_index = Some(i + 1);
                            break;
                        }
                    }
                    // Some means some subtree ends at the chunk boundary.
                    // None means there are padding data between the tx data and the boundary,
                    // so no data belongs to the last chunk.
                    if let Some(first_index) = first_index {
                        if first_index != tx.merkle_nodes.len() {
                            tx_list.push((tx_seq, tx.merkle_nodes[first_index..].to_vec()));
                        } else {
                            // If the last subtree ends at the chunk boundary, we also do not need
                            // to add data of this tx to the last chunk.
                            // This is only possible if the last chunk is empty, because otherwise
                            // we should have entered the `Equal` condition before and
                            // have broken the loop.
                            assert!(tx_list.is_empty());
                        }
                    }
                    break;
                }
            }
            if tx_seq == 0 {
                break;
            } else {
                tx_seq -= 1;
            }
        }
        let mut merkle = if last_chunk_start_index == 0 {
            // The first entry hash is initialized as zero.
            AppendMerkleTree::<H256, Sha3Algorithm>::new_with_depth(
                vec![H256::zero()],
                log2_pow2(PORA_CHUNK_SIZE) + 1,
                None,
            )
        } else {
            AppendMerkleTree::<H256, Sha3Algorithm>::new_with_depth(
                vec![],
                log2_pow2(PORA_CHUNK_SIZE) + 1,
                None,
            )
        };
        for (tx_seq, subtree_list) in tx_list.into_iter().rev() {
            // Pad the tx. After the first subtree is padded, other subtrees should be aligned.
            let first_subtree = 1 << (subtree_list[0].0 - 1);
            if merkle.leaves() % first_subtree != 0 {
                let pad_len =
                    cmp::min(first_subtree, PORA_CHUNK_SIZE) - (merkle.leaves() % first_subtree);
                merkle.append_list(data_to_merkle_leaves(&LogManager::padding(pad_len))?);
            }
            // Since we are building the last merkle with a given last tx_seq, it's ensured
            // that appending subtrees will not go beyond the max size.
            merkle.append_subtree_list(subtree_list)?;
            merkle.commit(Some(tx_seq));
        }
        Ok(merkle)
    }
}

fn decode_tx_seq(data: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(
        data.try_into().map_err(|e| anyhow!("{:?}", e))?,
    ))
}
