use super::chunk_cache::{ChunkPoolCache, MemoryCachedFile};
use super::chunk_write_control::ChunkPoolWriteCtrl;
use crate::Config;
use anyhow::{anyhow, bail, Result};
use async_lock::Mutex;
use shared_types::{ChunkArray, DataRoot, Transaction, CHUNK_SIZE};
use std::time::Duration;
use storage_async::Store;
use tokio::sync::mpsc::UnboundedSender;

// TODO(qhz): Suppose that file uploaded in sequence and following scenarios are to be resolved:
// 1) Uploaded not in sequence: costly to determine if all chunks uploaded, so as to finalize tx in store.
// 2) Upload concurrently: by one user or different users.

struct Inner {
    config: Config,
    segment_cache: ChunkPoolCache,
    write_control: ChunkPoolWriteCtrl,
}

impl Inner {
    fn new(config: Config) -> Self {
        let expiration_timeout = Duration::from_secs(config.expiration_time_secs);
        Inner {
            config,
            segment_cache: ChunkPoolCache::new(expiration_timeout),
            write_control: ChunkPoolWriteCtrl::new(),
        }
    }

    fn on_write_cache_succeeded(&mut self, root: &DataRoot, cached_segs_chunks: usize) {
        assert!(self.write_control.total_writings > 0);
        self.write_control.total_writings -= 1;
        assert!(self.segment_cache.total_chunks >= cached_segs_chunks);
        self.segment_cache.total_chunks -= cached_segs_chunks;

        let file = self.segment_cache.get_file_mut(root).unwrap();
        file.cached_chunk_num -= cached_segs_chunks;
    }

    fn on_write_cache_failed(&mut self, root: &DataRoot, cached_segs_chunks: usize) {
        assert!(self.write_control.total_writings > 0);
        self.write_control.total_writings -= 1;
        assert!(self.segment_cache.total_chunks >= cached_segs_chunks);
        self.segment_cache.total_chunks -= cached_segs_chunks;

        let file = self.segment_cache.get_file_mut(root).unwrap();
        file.cached_chunk_num -= cached_segs_chunks;
    }

    /// Return the tx seq and all segments that belong to the root.
    fn get_all_cached_segments_to_write(
        &mut self,
        root: &DataRoot,
    ) -> Result<(u64, usize, Vec<ChunkArray>)> {
        // Limits the number of writing threads.
        if self.write_control.total_writings >= self.config.max_writings {
            bail!(anyhow!(
                "too many data writing: {}",
                self.config.max_writings
            ));
        }

        let file = self.segment_cache.remove_file(root).unwrap();
        let tx_seq = file.tx_seq;
        let cached_chunk_num = file.cached_chunk_num;
        let segs = file.segments.into_iter().map(|(_k, v)| v).collect();

        self.write_control.total_writings += 1;

        Ok((tx_seq, cached_chunk_num, segs))
    }
}

pub struct SegmentInfo {
    pub root: DataRoot,
    pub seg_data: Vec<u8>,
    pub seg_index: usize,
    pub chunks_per_segment: usize,
}

/// Caches data chunks in memory before the entire file uploaded to storage node
/// and data root verified on blockchain.
pub struct MemoryChunkPool {
    inner: Mutex<Inner>,
    log_store: Store,
    sender: UnboundedSender<DataRoot>,
}

impl MemoryChunkPool {
    pub(crate) fn new(config: Config, log_store: Store, sender: UnboundedSender<DataRoot>) -> Self {
        MemoryChunkPool {
            inner: Mutex::new(Inner::new(config)),
            log_store,
            sender,
        }
    }

    pub fn validate_segment_size(&self, segment: &Vec<u8>) -> Result<()> {
        if segment.is_empty() {
            bail!(anyhow!("data is empty"));
        }

        if segment.len() % CHUNK_SIZE != 0 {
            bail!(anyhow!("invalid data length"))
        }

        Ok(())
    }

    pub async fn cache_chunks(&self, seg_info: SegmentInfo) -> Result<()> {
        self.inner.lock().await.segment_cache.garbage_collect();
        let file_complete;
        let root = seg_info.root;
        {
            let mut inner = self.inner.lock().await;
            let max_cached_chunks_all = inner.config.max_cached_chunks_all;
            file_complete = inner
                .segment_cache
                .cache_segment(seg_info, max_cached_chunks_all)?;
            inner.segment_cache.update_expiration_time(&root);
        } //inner is dropped after this code block, so the lock is released

        if file_complete {
            //Trigger writing the cached file into store and finalize when all chunks of a file are cached
            self.write_all_cached_chunks_and_finalize(root).await?;
        }

        Ok(())
    }

    pub async fn write_chunks(
        &self,
        seg_info: SegmentInfo,
        tx_seq: u64,
        file_size: usize,
    ) -> Result<()> {
        let file_total_chunk_num = file_size_to_chunk_num(file_size);

        debug!(
            "Begin to write segment, root={}, segment_size={}, segment_index={}",
            seg_info.root,
            seg_info.seg_data.len(),
            seg_info.seg_index,
        );

        {
            //Write the segment in window
            let mut inner = self.inner.lock().await;
            let write_window_size = inner.config.write_window_size;
            let max_writings = inner.config.max_writings;
            inner.write_control.write_segment(
                seg_info.root,
                tx_seq,
                seg_info.seg_index,
                file_total_chunk_num,
                write_window_size,
                max_writings,
            )?;
        } //inner is dropped after this code block, so the lock is released

        // Write memory cached segments into store.
        // TODO(qhz): error handling
        // 1. Push the failed segment back to front. (enhance store to return Err(ChunkArray))
        // 2. Put the incompleted segments back to memory pool.
        let seg = ChunkArray {
            data: seg_info.seg_data,
            start_index: (seg_info.seg_index * seg_info.chunks_per_segment) as u64,
        };

        if let Err(e) = self.log_store.put_chunks(tx_seq, seg).await {
            self.inner
                .lock()
                .await
                .write_control
                .on_write_failed(&seg_info.root, seg_info.seg_index);
            return Err(e);
        }

        let all_uploaded = self.inner.lock().await.write_control.on_write_succeeded(
            &seg_info.root,
            seg_info.seg_index,
            seg_info.chunks_per_segment,
            file_total_chunk_num,
        );

        // Notify to finalize transaction asynchronously.
        if all_uploaded {
            if let Err(e) = self.sender.send(seg_info.root) {
                // Channel receiver will not be dropped until program exit.
                bail!(anyhow!("channel send error: {}", e));
            }
            debug!("Queue to finalize transaction for file {}", seg_info.root);
        }

        Ok(())
    }

    /// Updates the cached file info when log entry retrieved from blockchain.
    pub async fn update_file_info(&self, tx: &Transaction) -> Result<bool> {
        let mut inner = self.inner.lock().await;

        // Do nothing if file not uploaded yet.
        let file = match inner.segment_cache.get_file_mut(&tx.data_merkle_root) {
            Some(f) => f,
            None => return Ok(false),
        };

        // Update the file info with transaction.
        file.update_with_tx(tx);

        // File partially uploaded and it's up to user thread
        // to write chunks into store and finalize transaction.
        if file.cached_chunk_num < file.total_chunks {
            return Ok(true);
        }

        // Otherwise, notify to write all memory cached chunks and finalize transaction.
        if let Err(e) = self.sender.send(tx.data_merkle_root) {
            // Channel receiver will not be dropped until program exit.
            bail!(anyhow!("channel send error: {}", e));
        }

        Ok(true)
    }

    pub(crate) async fn remove_cached_file(&self, root: &DataRoot) -> Option<MemoryCachedFile> {
        let mut inner = self.inner.lock().await;

        let file = inner.segment_cache.remove_file(root)?;
        inner
            .segment_cache
            .update_total_chunks_when_remove_file(&file);

        Some(file)
    }

    pub async fn check_already_has_cache(&self, root: &DataRoot) -> bool {
        self.inner.lock().await.segment_cache.has_cache(root)
    }

    async fn write_all_cached_chunks_and_finalize(&self, root: DataRoot) -> Result<()> {
        let (tx_seq, cache_chunk_num, mut segments) = self
            .inner
            .lock()
            .await
            .get_all_cached_segments_to_write(&root)?;

        while let Some(seg) = segments.pop() {
            // TODO(qhz): error handling
            // 1. Push the failed segment back to front. (enhance store to return Err(ChunkArray))
            // 2. Put the incompleted segments back to memory pool.
            if let Err(e) = self.log_store.put_chunks(tx_seq, seg).await {
                self.inner
                    .lock()
                    .await
                    .on_write_cache_failed(&root, cache_chunk_num);
                return Err(e);
            }
        }

        self.inner
            .lock()
            .await
            .on_write_cache_succeeded(&root, cache_chunk_num);

        if let Err(e) = self.sender.send(root) {
            // Channel receiver will not be dropped until program exit.
            bail!(anyhow!("channel send error: {}", e));
        }

        Ok(())
    }

    pub async fn get_tx_seq(&self, root: &DataRoot) -> u64 {
        let mut inner = self.inner.lock().await;
        let tx_seq = if inner.segment_cache.has_cache(root) {
            inner.segment_cache.get_file_mut(root).unwrap().tx_seq
        } else {
            inner.write_control.get_tx_seq(root)
        };

        tx_seq
    }
}

pub fn file_size_to_chunk_num(file_size: usize) -> usize {
    let mut chunk_num = file_size as usize / CHUNK_SIZE;
    if file_size as usize % CHUNK_SIZE > 0 {
        chunk_num += 1;
    }

    chunk_num
}
