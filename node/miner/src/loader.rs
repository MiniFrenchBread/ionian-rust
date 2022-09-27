use crate::pora::{BYTES_PER_LOADING, BYTES_PER_SEAL, SEALS_PER_LOADING, SECTORS_PER_SEAL};
use async_trait::async_trait;
use std::sync::Arc;
use storage::log_store::Store;
use tokio::sync::RwLock;

#[async_trait]
pub trait PoraLoader: Send + Sync {
    async fn load_sealed_data(
        &self,
        index: u64,
    ) -> Option<([u8; BYTES_PER_LOADING], [bool; SEALS_PER_LOADING])>;
}

// FIXME(kevin): not an efficient implementation
#[async_trait]
impl PoraLoader for Arc<RwLock<dyn Store>> {
    async fn load_sealed_data(
        &self,
        index: u64,
    ) -> Option<([u8; BYTES_PER_LOADING], [bool; SEALS_PER_LOADING])> {
        let store = &*self.read().await;
        let mut data_chunks = [[0u8; BYTES_PER_SEAL]; SEALS_PER_LOADING];
        let mut validities = [false; SEALS_PER_LOADING];
        let flow_index = index;

        for ((idx, chunk), valid_bit) in data_chunks
            .iter_mut()
            .enumerate()
            .zip(validities.iter_mut())
        {
            let load_index = flow_index + (idx * SECTORS_PER_SEAL) as u64;
            if load_index == 0 {
                continue;
            }
            if let Ok(Some(chunk_array)) =
                store.get_chunk_by_flow_index(load_index, SECTORS_PER_SEAL as u64)
            {
                chunk.copy_from_slice(&chunk_array.data);
                *valid_bit = true;
            }
        }

        let data_chunks: [u8; BYTES_PER_LOADING] = unsafe { std::mem::transmute(data_chunks) };

        Some((data_chunks, validities))
    }
}
