use super::api::RpcServer;
use crate::error;
use crate::types::{FileInfo, Segment, SegmentWithProof, Status};
use crate::Context;
use chunk_pool::SegmentInfo;
use jsonrpsee::core::async_trait;
use jsonrpsee::core::RpcResult;
use shared_types::DataRoot;
use storage::try_option;

pub struct RpcServerImpl {
    pub ctx: Context,
}

#[async_trait]
impl RpcServer for RpcServerImpl {
    #[tracing::instrument(skip(self), err)]
    async fn get_status(&self) -> RpcResult<Status> {
        info!("ionian_getStatus()");

        Ok(Status {
            connected_peers: self.ctx.network_globals.connected_peers(),
        })
    }

    async fn upload_segment(&self, segment: SegmentWithProof) -> RpcResult<()> {
        debug!("ionian_uploadSegment()");

        let _ = self.ctx.chunk_pool.validate_segment_size(&segment.data)?;

        let seq = self
            .ctx
            .log_store
            .get_tx_seq_by_data_root(&segment.root)
            .await?;

        let mut need_cache = false;

        if self
            .ctx
            .chunk_pool
            .check_already_has_cache(&segment.root)
            .await
        {
            need_cache = true;
        }

        if !need_cache {
            need_cache = self.check_need_cache(seq, segment.file_size).await?;
        }

        segment.validate(self.ctx.config.chunks_per_segment)?;

        let seg_info = SegmentInfo {
            root: segment.root,
            seg_data: segment.data,
            seg_index: segment.index as usize,
            chunks_per_segment: self.ctx.config.chunks_per_segment,
        };

        if need_cache {
            self.ctx.chunk_pool.cache_chunks(seg_info).await?;
        } else {
            self.ctx
                .chunk_pool
                .write_chunks(seg_info, seq.unwrap(), segment.file_size as usize)
                .await?;
        }

        Ok(())
    }

    async fn download_segment(
        &self,
        data_root: DataRoot,
        start_index: u32,
        end_index: u32,
    ) -> RpcResult<Option<Segment>> {
        debug!("ionian_downloadSegment()");

        if start_index >= end_index {
            return Err(error::invalid_params("end_index", "invalid chunk index"));
        }

        if end_index - start_index > self.ctx.config.chunks_per_segment as u32 {
            return Err(error::invalid_params(
                "end_index",
                format!(
                    "exceeds maximum chunks {}",
                    self.ctx.config.chunks_per_segment
                ),
            ));
        }

        let tx_seq = try_option!(
            self.ctx
                .log_store
                .get_tx_seq_by_data_root(&data_root)
                .await?
        );
        let segment = try_option!(
            self.ctx
                .log_store
                .get_chunks_by_tx_and_index_range(tx_seq, start_index as usize, end_index as usize)
                .await?
        );

        Ok(Some(Segment(segment.data)))
    }

    async fn get_file_info(&self, data_root: DataRoot) -> RpcResult<Option<FileInfo>> {
        debug!("get_file_info()");

        let tx_seq = try_option!(
            self.ctx
                .log_store
                .get_tx_seq_by_data_root(&data_root)
                .await?
        );
        let tx = try_option!(self.ctx.log_store.get_tx_by_seq_number(tx_seq).await?);

        Ok(Some(FileInfo {
            tx,
            finalized: self.ctx.log_store.check_tx_completed(tx_seq).await?,
        }))
    }
}

impl RpcServerImpl {
    async fn check_need_cache(&self, seq: Option<u64>, file_size: u64) -> RpcResult<bool> {
        let mut need_cache = false;

        if let Some(tx_seq) = seq {
            // Transaction already finalized for the specified file data root.
            if self.ctx.log_store.check_tx_completed(tx_seq).await? {
                return Err(error::invalid_params(
                    "root",
                    "already uploaded and finalized",
                ));
            }

            let tx = match self.ctx.log_store.get_tx_by_seq_number(tx_seq).await? {
                Some(tx) => tx,
                None => return Err(error::invalid_params("root", "data root not found")),
            };

            if tx.size != file_size {
                return Err(error::invalid_params(
                    "file_size",
                    "segment file size not matched with tx file size",
                ));
            }
        } else {
            //Check whether file is small enough to cache in the system
            if file_size as usize > self.ctx.config.max_cache_file_size {
                return Err(error::invalid_params(
                    "file_size",
                    "caching of large file when tx is unavailable is not supported",
                ));
            }

            need_cache = true;
        }

        Ok(need_cache)
    }
}
