use crate::types::{FileInfo, Segment, SegmentWithProof, Status};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use shared_types::DataRoot;

#[rpc(server, client, namespace = "ionian")]
pub trait Rpc {
    #[method(name = "getStatus")]
    async fn get_status(&self) -> RpcResult<Status>;

    #[method(name = "uploadSegment")]
    async fn upload_segment(&self, segment: SegmentWithProof) -> RpcResult<()>;

    #[method(name = "downloadSegment")]
    async fn download_segment(
        &self,
        data_root: DataRoot,
        start_index: usize,
        end_index: usize,
    ) -> RpcResult<Option<Segment>>;

    #[method(name = "downloadSegmentWithProof")]
    async fn download_segment_with_proof(
        &self,
        data_root: DataRoot,
        index: usize,
    ) -> RpcResult<Option<SegmentWithProof>>;

    #[method(name = "getFileInfo")]
    async fn get_file_info(&self, data_root: DataRoot) -> RpcResult<Option<FileInfo>>;

    #[method(name = "getFileInfoByTxSeq")]
    async fn get_file_info_by_tx_seq(&self, tx_seq: u64) -> RpcResult<Option<FileInfo>>;
}
