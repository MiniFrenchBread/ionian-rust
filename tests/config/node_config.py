from web3 import Web3

IONIAN_CONFIG = dict(log_config_file="log_config")

BSC_CONFIG = dict(
    NetworkId=1000,
    HTTPPort=8545,
    HTTPHost="127.0.0.1",
    Etherbase="0x7df9a875a174b3bc565e6424a0050ebc1b2d1d82",
    DataDir="test/local_ethereum_blockchain/node1",
    Port=30303,
    Verbosity=5,
)

GENESIS_PRIV_KEY = "46b9e861b63d3509c88b7817275a30d22d62c8cd8fa6486ddee35ef0d8e0495f"
MINER_ID = "308a6e102a5829ba35e4ba1da0473c3e8bd45f5d3ffb91e31adb43f25463dddb"
GENESIS_ACCOUNT = Web3().eth.account.from_key(GENESIS_PRIV_KEY)
TX_PARAMS = {"gasPrice": 10_000_000_000, "from": GENESIS_ACCOUNT.address}

NO_SEAL_FLAG = 0x1
NO_MERKLE_PROOF_FLAG = 0x2
