import os
import time

from config.node_config import BLOCK_SIZE_LIMIT, CONFLUX_CONFIG
from test_framework.blockchain_node import BlockChainNodeType, BlockchainNode
from utility.simple_rpc_proxy import SimpleRpcProxy
from utility.utils import (
    blockchain_p2p_port,
    blockchain_rpc_port,
    blockchain_rpc_port_core,
)

from web3.exceptions import TransactionNotFound


class ConfluxNode(BlockchainNode):
    def __init__(
        self,
        index,
        root_dir,
        binary,
        updated_config,
        contract_path,
        token_contract_path,
        mine_contract_path,
        log,
        rpc_timeout=10,
    ):
        local_conf = CONFLUX_CONFIG.copy()
        indexed_config = {
            "jsonrpc_http_eth_port": blockchain_rpc_port(index),
            "jsonrpc_local_http_port": blockchain_rpc_port_core(index),
            "port": blockchain_p2p_port(index),
        }
        # Set configs for this specific node.
        local_conf.update(indexed_config)
        # Overwrite with personalized configs.
        local_conf.update(updated_config)
        data_dir = os.path.join(root_dir, "blockchain_node" + str(index))
        rpc_url = (
            "http://"
            + local_conf["public_address"]
            + ":"
            + str(local_conf["jsonrpc_http_eth_port"])
        )

        if "dev_block_interval_ms" in local_conf:
            self.auto_mining = True
        else:
            self.auto_mining = False
            core_rpc_url = (
                "http://"
                + local_conf["public_address"]
                + ":"
                + str(local_conf["jsonrpc_local_http_port"])
            )

            self.core_rpc = SimpleRpcProxy(core_rpc_url, timeout=rpc_timeout)

        super().__init__(
            index,
            data_dir,
            rpc_url,
            binary,
            local_conf,
            contract_path,
            token_contract_path,
            mine_contract_path,
            log,
            BlockChainNodeType.Conflux,
            rpc_timeout,
        )

    def wait_for_transaction_receipt(self, w3, tx_hash, timeout=120):
        if self.auto_mining:
            return super().wait_for_transaction_receipt(w3, tx_hash, timeout)
        else:
            time_end = time.time() + timeout
            while time.time() < time_end:
                try:
                    tx_receipt = w3.eth.get_transaction_receipt(tx_hash)
                except TransactionNotFound:
                    tx_receipt = None
                    self.core_rpc.generateoneblock([1, BLOCK_SIZE_LIMIT])

                if tx_receipt is not None:
                    return tx_receipt

            raise TransactionNotFound
