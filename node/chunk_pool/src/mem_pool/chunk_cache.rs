use super::chunk_pool_inner::{file_size_to_chunk_num, SegmentInfo};
use anyhow::{anyhow, bail, Result};
use hashlink::LinkedHashMap;
use shared_types::{ChunkArray, DataRoot, Transaction, CHUNK_SIZE};
use std::collections::HashMap;
use std::ops::Add;
use std::time::{Duration, Instant};

/// Used to cache chunks in memory pool and persist into db once log entry
/// retrieved from blockchain.
pub struct MemoryCachedFile {
    /// Window to control the cache of each file
    pub segments: HashMap<usize, ChunkArray>,
    /// Total number of chunks for the cache file, which is updated from log entry.
    pub total_chunks: usize,
    /// Transaction seq that used to write chunks into store.
    pub tx_seq: u64,
    /// Used for garbage collection.
    expired_at: Instant,
    /// Number of chunks that's currently cached for this file
    pub cached_chunk_num: usize,
}

impl MemoryCachedFile {
    fn new(timeout: Duration) -> Self {
        MemoryCachedFile {
            segments: HashMap::default(),
            total_chunks: 0,
            tx_seq: 0,
            expired_at: Instant::now().add(timeout),
            cached_chunk_num: 0,
        }
    }

    /// Updates file with transaction once log entry retrieved from blockchain.
    /// So that, write memory cached segments into database.
    pub fn update_with_tx(&mut self, tx: &Transaction) {
        self.total_chunks = file_size_to_chunk_num(tx.size as usize);
        self.tx_seq = tx.seq;
    }
}

pub struct ChunkPoolCache {
    expiration_timeout: Duration,
    /// All cached files.
    files: LinkedHashMap<DataRoot, MemoryCachedFile>,
    /// Total number of chunks that cached in the memory pool.
    pub total_chunks: usize,
}

impl ChunkPoolCache {
    pub fn new(expiration_timeout: Duration) -> Self {
        ChunkPoolCache {
            expiration_timeout,
            files: LinkedHashMap::default(),
            total_chunks: 0,
        }
    }

    pub fn update_expiration_time(&mut self, root: &DataRoot) {
        if let Some(file) = self.files.to_back(root) {
            file.expired_at = Instant::now().add(self.expiration_timeout);
        }
    }

    pub fn garbage_collect(&mut self) {
        while let Some((_, file)) = self.files.front() {
            if file.expired_at > Instant::now() {
                return;
            }

            if let Some((r, f)) = self.files.pop_front() {
                self.update_total_chunks_when_remove_file(&f);
                debug!("Garbage collected for file {}", r);
            }
        }
    }

    pub fn update_total_chunks_when_remove_file(&mut self, file: &MemoryCachedFile) {
        assert!(self.total_chunks >= file.cached_chunk_num);
        self.total_chunks -= file.cached_chunk_num;
    }

    pub fn cache_segment(
        &mut self,
        seg_info: SegmentInfo,
        max_cached_chunks_all: usize,
    ) -> Result<bool> {
        let file = self
            .files
            .entry(seg_info.root)
            .or_insert_with(|| MemoryCachedFile::new(self.expiration_timeout));

        let mut file_complete = false;

        //Segment already cached in memory. Directly return OK
        if file.segments.contains_key(&seg_info.seg_index) {
            return Ok(file_complete);
        }

        // Otherwise, just cache segment in memory
        let num_chunks = seg_info.seg_data.len() / CHUNK_SIZE;

        // Limits the cached chunks in the memory pool.
        if self.total_chunks + num_chunks > max_cached_chunks_all {
            bail!(anyhow!(
                "exceeds the maximum cached chunks of whole pool: {}",
                max_cached_chunks_all
            ));
        }

        // Cache segment and update the counter for cached chunks.
        self.total_chunks += num_chunks;
        file.cached_chunk_num += num_chunks;
        file.segments.insert(
            seg_info.seg_index,
            ChunkArray {
                data: seg_info.seg_data,
                start_index: (seg_info.seg_index * seg_info.chunks_per_segment) as u64,
            },
        );

        if file.total_chunks > 0 {
            file_complete = file.cached_chunk_num >= file.total_chunks;
        }

        Ok(file_complete)
    }

    pub fn has_cache(&self, root: &DataRoot) -> bool {
        self.files.contains_key(root)
    }

    pub fn get_file_mut(&mut self, root: &DataRoot) -> Option<&mut MemoryCachedFile> {
        self.files.get_mut(root)
    }

    pub fn remove_file(&mut self, root: &DataRoot) -> Option<MemoryCachedFile> {
        self.files.remove(root)
    }
}
