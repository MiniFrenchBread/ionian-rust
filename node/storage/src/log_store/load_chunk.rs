use crate::log_store::log_manager::{
    data_to_merkle_leaves, sub_merkle_tree, ENTRY_SIZE, PORA_CHUNK_SIZE,
};

use anyhow::{bail, Result};
use append_merkle::{AppendMerkleTree, MerkleTreeRead, Sha3Algorithm};
use ethereum_types::H256;
use shared_types::{bytes_to_chunks, ChunkArray};
use ssz::{Decode, DecodeError, Encode};
use ssz_derive::{Decode as DeriveDecode, Encode as DeriveEncode};
use std::fmt::{Debug, Formatter};
use std::mem;
use tracing::trace;

#[derive(DeriveEncode, DeriveDecode)]
pub struct EntryBatch {
    // a bitmap specify which sealing chunks have been sealed
    seal_status: u64,
    // the inner data
    data: EntryBatchData,
}

impl EntryBatch {
    pub fn new_with_chunk_array(
        chunk: ChunkArray,
        start_offset: usize,
        is_full_chunk: bool,
    ) -> Self {
        let data = if is_full_chunk {
            EntryBatchData::Complete(chunk.data)
        } else {
            EntryBatchData::Incomplete(vec![PartialBatch {
                start_offset,
                data: chunk.data,
            }])
        };
        Self {
            seal_status: 0,
            data,
        }
    }
}

enum EntryBatchData {
    Complete(Vec<u8>),
    /// All `PartialBatch`s are ordered based on `start_index`.
    Incomplete(Vec<PartialBatch>),
}

const COMPLETE_BATCH_TYPE: u8 = 0;
const INCOMPLETE_BATCH_TYPE: u8 = 1;

impl Encode for EntryBatchData {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        match &self {
            EntryBatchData::Complete(data) => {
                buf.extend_from_slice(&[COMPLETE_BATCH_TYPE]);
                buf.extend_from_slice(data.as_slice());
            }
            EntryBatchData::Incomplete(data_list) => {
                buf.extend_from_slice(&[INCOMPLETE_BATCH_TYPE]);
                buf.extend_from_slice(&data_list.as_ssz_bytes());
            }
        }
    }

    fn ssz_bytes_len(&self) -> usize {
        match &self {
            EntryBatchData::Complete(data) => 1 + data.len(),
            EntryBatchData::Incomplete(batch_list) => 1 + batch_list.ssz_bytes_len(),
        }
    }
}

impl Decode for EntryBatchData {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> std::result::Result<Self, DecodeError> {
        match *bytes.first().ok_or(DecodeError::ZeroLengthItem)? {
            COMPLETE_BATCH_TYPE => Ok(EntryBatchData::Complete(bytes[1..].to_vec())),
            INCOMPLETE_BATCH_TYPE => Ok(EntryBatchData::Incomplete(
                <Vec<PartialBatch> as Decode>::from_ssz_bytes(&bytes[1..])?,
            )),
            unknown => Err(DecodeError::BytesInvalid(format!(
                "Unrecognized EntryBatchData indentifier {}",
                unknown
            ))),
        }
    }
}

struct PartialBatch {
    /// Offset in this batch.
    start_offset: usize,
    data: Vec<u8>,
}

impl Debug for PartialBatch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PartialBatch: start_offset={} data_len={}",
            self.start_offset,
            self.data.len()
        )
    }
}

impl Encode for PartialBatch {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.start_offset.to_be_bytes());
        buf.extend_from_slice(&self.data);
    }

    fn ssz_bytes_len(&self) -> usize {
        1 + self.data.len()
    }
}

impl Decode for PartialBatch {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> std::result::Result<Self, DecodeError> {
        Ok(Self {
            start_offset: usize::from_be_bytes(
                bytes[..mem::size_of::<usize>()].try_into().unwrap(),
            ),
            data: bytes[mem::size_of::<usize>()..].to_vec(),
        })
    }
}

impl PartialBatch {
    fn end_offset(&self) -> usize {
        self.start_offset + bytes_to_chunks(self.data.len())
    }
}

impl EntryBatch {
    /// Get data chunks in the flow from the storage.
    /// Whether the data is sealed is unknown.
    fn get_storage_data(&self, offset: usize, length: usize) -> Option<Vec<u8>> {
        match &self.data {
            EntryBatchData::Complete(data) => data
                .get(offset * ENTRY_SIZE..(offset + length) * ENTRY_SIZE)
                .map(|s| s.to_vec()),
            EntryBatchData::Incomplete(data_list) => {
                for p in data_list {
                    if offset >= p.start_offset
                        && offset + length <= p.start_offset + bytes_to_chunks(p.data.len())
                    {
                        return p
                            .data
                            .get(
                                (offset - p.start_offset) * ENTRY_SIZE
                                    ..(offset - p.start_offset + length) * ENTRY_SIZE,
                            )
                            .map(|s| s.to_vec());
                    }
                }
                None
            }
        }
    }

    /// Get unsealed data
    pub fn get_data(&self, offset: usize, length: usize) -> Option<Vec<u8>> {
        let unsealed_data = self.get_storage_data(offset, length)?;
        // TODO: unseal data
        Some(unsealed_data)
    }

    /// Return `Error` if the new data overlaps with old data.
    /// Convert `Incomplete` to `Completed` if the chunk is completed after the insertion.
    pub fn insert_data(&mut self, offset: usize, mut data: Vec<u8>) -> Result<()> {
        // Check if the entry is completed
        let list = if let EntryBatchData::Incomplete(list) = &mut self.data {
            list
        } else {
            bail!("overwriting a completed PoRA Chunk with partial data");
        };

        // Check if the entry is completed
        let position = match list.binary_search_by_key(&offset, |p| p.start_offset) {
            Ok(i) => {
                bail!(
                    "same offset with a PartialBatch at index {}: offset={}",
                    i,
                    offset
                );
            }
            Err(position) => position,
        };

        // Check if overlapped with the previous item
        if position != 0 && offset < list[position - 1].end_offset() {
            bail!(
                "Overlap with index {}: end_offset={} new_offset={}",
                position - 1,
                list[position - 1].end_offset(),
                offset
            );
        }

        let data_entry_len = bytes_to_chunks(data.len());

        // Check if overlapped with the next item
        if position != list.len() && offset + data_entry_len > list[position].start_offset {
            bail!(
                "Overlap with index{}: start_offset={} new_end_offset={}",
                position,
                list[position].start_offset,
                offset + data_entry_len
            );
        }

        let merge_prev = position != 0 && offset == list[position - 1].end_offset();
        let merge_next =
            position != list.len() && offset + data_entry_len == list[position].start_offset;

        match (merge_prev, merge_next) {
            (false, false) => {
                list.insert(
                    position,
                    PartialBatch {
                        start_offset: offset,
                        data,
                    },
                );
            }
            (true, false) => {
                list[position - 1].data.append(&mut data);
            }
            (false, true) => {
                data.append(&mut list[position].data);
                list[position] = PartialBatch {
                    start_offset: offset,
                    data,
                };
            }
            (true, true) => {
                // Merge the new data with the two around partial batches to
                // a single one.
                list[position - 1].data.append(&mut data);
                let mut next = list.remove(position);
                list[position - 1].data.append(&mut next.data);
            }
        }
        // TODO(zz): Use config here?
        if list.len() == 1
            && list[0].start_offset == 0
            && bytes_to_chunks(list[0].data.len()) == PORA_CHUNK_SIZE
        {
            // All data in this batch have been filled.
            self.data = EntryBatchData::Complete(list.remove(0).data);
        }
        Ok(())
    }

    pub fn truncate(&mut self, start_offset: usize) {
        assert!(start_offset > 0 && start_offset < PORA_CHUNK_SIZE);
        match &mut self.data {
            EntryBatchData::Complete(data) => {
                data.truncate(start_offset);
            }
            EntryBatchData::Incomplete(batch_list) => {
                let mut start_partial_batch_index = None;
                for (i, b) in batch_list.iter_mut().enumerate() {
                    if b.start_offset >= start_offset {
                        // All partial chunks after (including) i should be removed;
                        start_partial_batch_index = Some(i);
                        break;
                    } else if b.start_offset + bytes_to_chunks(b.data.len()) > start_offset {
                        start_partial_batch_index = Some(i + 1);
                        b.data
                            .truncate((start_offset - b.start_offset) * ENTRY_SIZE);
                        break;
                    }
                }
                if let Some(start_index) = start_partial_batch_index {
                    batch_list.truncate(start_index);
                }
            }
        }
    }

    pub fn build_root(&self, is_first_chunk: bool) -> Result<Option<H256>> {
        if is_first_chunk {
            return self.build_root_for_first_chunk();
        }
        Ok(if let EntryBatchData::Complete(raw_data) = &self.data {
            // TODO(zz): Check if we want to insert here.
            assert_eq!(raw_data.len(), ENTRY_SIZE * PORA_CHUNK_SIZE);
            Some(sub_merkle_tree(raw_data.as_slice())?.root().into())
        } else {
            None
        })
    }

    fn build_root_for_first_chunk(&self) -> Result<Option<H256>> {
        let partial_batch = match &self.data {
            EntryBatchData::Complete(_) => {
                bail!("Unexpected first batch");
            }
            EntryBatchData::Incomplete(p)
                if p.len() == 1
                    && p[0].start_offset == 1
                    && p[0].data.len() == ENTRY_SIZE * (PORA_CHUNK_SIZE - 1) =>
            {
                &p[0]
            }
            _ => {
                return Ok(None);
            }
        };

        trace!("put first batch: {:?}", partial_batch);
        let mut leaves = vec![H256::zero()];
        leaves.append(&mut data_to_merkle_leaves(&partial_batch.data)?);
        let root = *AppendMerkleTree::<H256, Sha3Algorithm>::new(leaves, 0, None).root();

        Ok(Some(root))
    }

    pub fn into_data_list(self, start_index: u64) -> Vec<ChunkArray> {
        // TODO: unseal data
        match self.data {
            EntryBatchData::Complete(data) => {
                vec![ChunkArray { data, start_index }]
            }
            EntryBatchData::Incomplete(batch_list) => batch_list
                .into_iter()
                .map(|b| ChunkArray {
                    data: b.data,
                    start_index: start_index + b.start_offset as u64,
                })
                .collect(),
        }
    }
}
