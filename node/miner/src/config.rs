use ethereum_types::{Address, H256};
use ethers::core::k256::SecretKey;
use ethers::middleware::SignerMiddleware;
use ethers::providers::Http;
use ethers::providers::Middleware;
use ethers::providers::Provider;
use ethers::signers::LocalWallet;
use ethers::signers::Signer;

pub struct MinerConfig {
    pub(crate) miner_id: H256,
    pub(crate) miner_key: H256,
    pub(crate) rpc_endpoint_url: String,
    pub(crate) mine_address: Address,
    pub(crate) flow_address: Address,
}

pub type MineServiceMiddleware = SignerMiddleware<Provider<Http>, LocalWallet>;

impl MinerConfig {
    pub fn new(
        miner_id: Option<H256>,
        miner_key: Option<H256>,
        rpc_endpoint_url: String,
        mine_address: Address,
        flow_address: Address,
    ) -> Option<MinerConfig> {
        match (miner_id, miner_key) {
            (Some(miner_id), Some(miner_key)) => Some(MinerConfig {
                miner_id,
                miner_key,
                rpc_endpoint_url,
                mine_address,
                flow_address,
            }),
            _ => None,
        }
    }

    pub(crate) async fn make_provider(&self) -> Result<MineServiceMiddleware, String> {
        let provider = Provider::<Http>::try_from(&self.rpc_endpoint_url)
            .map_err(|e| format!("Can not parse blockchain endpoint: {:?}", e))?;
        let chain_id = provider
            .get_chainid()
            .await
            .map_err(|e| format!("Unable to get chain_id: {:?}", e))?;
        let secret_key = SecretKey::from_be_bytes(self.miner_key.as_ref())
            .map_err(|e| format!("Cannot parse private key: {:?}", e))?;
        let signer = LocalWallet::from(secret_key).with_chain_id(chain_id.as_u64());
        let middleware = SignerMiddleware::new(provider, signer);
        Ok(middleware)
    }
}
