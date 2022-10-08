use crate::rpc_proxy::ContractAddress;

pub struct LogSyncConfig {
    pub rpc_endpoint_url: String,
    pub contract_address: ContractAddress,
    pub cache_config: CacheConfig,

    /// The block number where we start to sync data.
    /// This is usually the block number when Ionian contract is deployed.
    pub start_block_number: u64,
    /// The number of blocks needed for confirmation on the blockchain.
    /// This is used to rollback to a stable height if reorg happens during node restart.
    /// TODO(zz): Some blockchains have better confirmation/finalization mechanisms.
    pub confirmation_block_count: u64,
}

#[derive(Clone)]
pub struct CacheConfig {
    /// The data with a size larger than this will not be cached.
    /// This is reasonable because uploading
    pub max_data_size: usize,
    pub tx_seq_ttl: usize,
}

impl LogSyncConfig {
    pub fn new(
        rpc_endpoint_url: String,
        contract_address: ContractAddress,
        start_block_number: u64,
        confirmation_block_count: u64,
        cache_config: CacheConfig,
    ) -> Self {
        Self {
            rpc_endpoint_url,
            contract_address,
            cache_config,
            start_block_number,
            confirmation_block_count,
        }
    }
}
