use crate::log_store::log_manager::{
    bytes_to_entries, data_to_merkle_leaves, sub_merkle_tree, LogConfig, LogManager, ENTRY_SIZE,
    PORA_CHUNK_SIZE,
};
use crate::log_store::{LogStoreChunkRead, LogStoreChunkWrite, LogStoreRead, LogStoreWrite};
use append_merkle::{Algorithm, AppendMerkleTree, Sha3Algorithm};
use ethereum_types::H256;
use merkle_light::merkle::{log2_pow2, next_pow2};
use rand::random;
use shared_types::{ChunkArray, DataRoot, Transaction, CHUNK_SIZE};
use std::cmp;

#[test]
fn test_put_get() {
    let config = LogConfig::default();
    let mut store = LogManager::memorydb(config.clone()).unwrap();
    let chunk_count = config.flow.batch_size + config.flow.batch_size / 2 - 1;
    // Aligned with size.
    let start_offset = 1024;
    let data_size = CHUNK_SIZE * chunk_count;
    let mut data = vec![0u8; data_size];
    for i in 0..chunk_count {
        data[i * CHUNK_SIZE] = random();
    }
    let mut merkle = AppendMerkleTree::<H256, Sha3Algorithm>::new(vec![H256::zero()], None);
    merkle.append_list(data_to_merkle_leaves(&LogManager::padding(start_offset - 1)).unwrap());
    merkle.append_list(data_to_merkle_leaves(&data).unwrap());
    merkle.commit(Some(0));
    let tx_merkle = sub_merkle_tree(&data).unwrap();
    let tx = Transaction {
        stream_ids: vec![],
        size: data_size as u64,
        data_merkle_root: tx_merkle.root().into(),
        seq: 0,
        data: vec![],
        start_entry_index: start_offset as u64,
        // TODO: This can come from `tx_merkle`.
        merkle_nodes: tx_subtree_root_list(&data),
    };
    store.put_tx(tx.clone()).unwrap();
    for start_index in (0..chunk_count).step_by(PORA_CHUNK_SIZE) {
        let end = cmp::min((start_index + PORA_CHUNK_SIZE) * CHUNK_SIZE, data.len());
        let chunk_array = ChunkArray {
            data: data[start_index * CHUNK_SIZE..end].to_vec(),
            start_index: start_index as u64,
        };
        store.put_chunks(tx.seq, chunk_array.clone()).unwrap();
    }
    store.finalize_tx(tx.seq).unwrap();

    let chunk_array = ChunkArray {
        data,
        start_index: 0,
    };
    assert_eq!(store.get_tx_by_seq_number(0).unwrap().unwrap(), tx);
    for i in 0..chunk_count {
        assert_eq!(
            store.get_chunk_by_tx_and_index(tx.seq, i).unwrap().unwrap(),
            chunk_array.chunk_at(i).unwrap()
        );
    }
    assert_eq!(
        store
            .get_chunk_by_tx_and_index(tx.seq, chunk_count)
            .unwrap(),
        None
    );

    assert_eq!(
        store
            .get_chunks_by_tx_and_index_range(tx.seq, 0, chunk_count)
            .unwrap()
            .unwrap(),
        chunk_array
    );
    for i in 0..chunk_count {
        let chunk_with_proof = store
            .get_chunk_with_proof_by_tx_and_index(tx.seq, i)
            .unwrap()
            .unwrap();
        assert_eq!(chunk_with_proof.chunk, chunk_array.chunk_at(i).unwrap());
        assert_eq!(
            chunk_with_proof.proof,
            merkle.gen_proof(i + start_offset).unwrap()
        );
        let r = chunk_with_proof.proof.validate::<Sha3Algorithm>(
            &Sha3Algorithm::leaf(&chunk_with_proof.chunk.0),
            i + start_offset,
        );
        assert!(r.is_ok(), "proof={:?} \n r={:?}", chunk_with_proof.proof, r);
        assert!(merkle.check_root(&chunk_with_proof.proof.root()));
    }
    for i in (0..chunk_count).step_by(PORA_CHUNK_SIZE / 3) {
        let end = std::cmp::min(i + PORA_CHUNK_SIZE, chunk_count);
        let chunk_array_with_proof = store
            .get_chunks_with_proof_by_tx_and_index_range(tx.seq, i, end)
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk_array_with_proof.chunks,
            chunk_array.sub_array(i as u64, end as u64).unwrap()
        );
        assert!(chunk_array_with_proof
            .proof
            .validate::<Sha3Algorithm>(
                &data_to_merkle_leaves(&chunk_array_with_proof.chunks.data).unwrap(),
                i + start_offset
            )
            .is_ok());
    }
}

#[test]
fn test_root() {
    for depth in 0..12 {
        let n_chunk = 1 << depth;
        let mut data = vec![0; n_chunk * CHUNK_SIZE];
        for i in 0..n_chunk {
            data[i * CHUNK_SIZE] = random();
        }
        let mt = sub_merkle_tree(&data).unwrap();
        println!("{:?} {}", mt.root(), hex::encode(&mt.root()));
        let append_mt = AppendMerkleTree::<H256, Sha3Algorithm>::new(
            data_to_merkle_leaves(&data).unwrap(),
            None,
        );
        assert_eq!(mt.root(), append_mt.root().0);
    }
}

#[test]
fn test_multi_tx() {
    let mut store = create_store();
    put_tx(&mut store, 3, 0, 2);
    put_tx(&mut store, 3, 1, 6);
    put_tx(&mut store, 5, 2, 12);
}

#[test]
fn test_revert() {
    let mut store = create_store();
    put_tx(&mut store, 1, 0, 1);
    store.revert_to(0u64.wrapping_sub(1)).unwrap();
    put_tx(&mut store, 1, 0, 1);
    put_tx(&mut store, 1, 1, 2);
    store.revert_to(0).unwrap();
    put_tx(&mut store, 1, 1, 2);
}

fn tx_subtree_root_list(data: &[u8]) -> Vec<(usize, DataRoot)> {
    let mut root_list = Vec::new();
    let mut start_index = 0;
    let data_entry_len = bytes_to_entries(data.len() as u64) as usize;
    while start_index != data_entry_len {
        let next = next_subtree_size(data_entry_len - start_index);
        let end = cmp::min((start_index + next) * ENTRY_SIZE, data.len());
        let submerkle_root = sub_merkle_tree(&data[start_index * ENTRY_SIZE..end])
            .unwrap()
            .root();
        root_list.push((log2_pow2(next) + 1, submerkle_root.into()));
        start_index = start_index + next;
    }
    root_list
}

fn next_subtree_size(tree_size: usize) -> usize {
    let next = next_pow2(tree_size);
    if next == tree_size {
        tree_size
    } else {
        next >> 1
    }
}

fn create_store() -> LogManager {
    let config = LogConfig::default();
    let store = LogManager::memorydb(config).unwrap();
    store
}

fn put_tx(store: &mut LogManager, chunk_count: usize, seq: u64, start_entry_index: u64) {
    let data_size = CHUNK_SIZE * chunk_count;
    let mut data = vec![0u8; data_size];
    for i in 0..chunk_count {
        data[i * CHUNK_SIZE] = random();
    }
    let tx_merkle = sub_merkle_tree(&data).unwrap();
    let tx = Transaction {
        stream_ids: vec![],
        size: data_size as u64,
        data_merkle_root: tx_merkle.root().into(),
        seq,
        data: vec![],
        start_entry_index,
        // TODO: This can come from `tx_merkle`.
        merkle_nodes: tx_subtree_root_list(&data),
    };
    store.put_tx(tx.clone()).unwrap();
    for start_index in (0..chunk_count).step_by(PORA_CHUNK_SIZE) {
        let end = cmp::min((start_index + PORA_CHUNK_SIZE) * CHUNK_SIZE, data.len());
        let chunk_array = ChunkArray {
            data: data[start_index * CHUNK_SIZE..end].to_vec(),
            start_index: start_index as u64,
        };
        store.put_chunks(tx.seq, chunk_array.clone()).unwrap();
    }
    store.finalize_tx(tx.seq).unwrap();
}
