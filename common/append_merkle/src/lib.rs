mod merkle_tree;
mod proof;
mod sha3;

use anyhow::{anyhow, bail, Result};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Debug;
use std::marker::PhantomData;

pub use crate::merkle_tree::{Algorithm, HashElement, MerkleTreeRead};
pub use proof::{Proof, RangeProof};
pub use sha3::Sha3Algorithm;

pub struct AppendMerkleTree<E: HashElement, A: Algorithm<E>> {
    /// Keep all the nodes in the latest version. `layers[0]` is the layer of leaves.
    layers: Vec<Vec<E>>,
    /// Keep the delta nodes that can be used to construct a history tree.
    /// The key is the root node of that version.
    delta_nodes_map: HashMap<u64, DeltaNodes<E>>,
    root_to_tx_seq_map: HashMap<E, u64>,

    /// For `last_chunk_merkle` after the first chunk, this is set to `Some(10)` so that
    /// `revert_to` can reset the state correctly when needed.
    min_depth: Option<usize>,
    /// Used to compute the correct padding hash.
    /// 0 for `pora_chunk_merkle` and 10 for not-first `last_chunk_merkle`.
    leaf_height: usize,
    _a: PhantomData<A>,
}

impl<E: HashElement, A: Algorithm<E>> AppendMerkleTree<E, A> {
    pub fn new(leaves: Vec<E>, leaf_height: usize, start_tx_seq: Option<u64>) -> Self {
        let mut merkle = Self {
            layers: vec![leaves],
            delta_nodes_map: HashMap::new(),
            root_to_tx_seq_map: HashMap::new(),
            min_depth: None,
            leaf_height,
            _a: Default::default(),
        };
        if merkle.leaves() == 0 {
            if let Some(seq) = start_tx_seq {
                merkle.delta_nodes_map.insert(
                    seq,
                    DeltaNodes {
                        right_most_nodes: vec![],
                    },
                );
            }
            return merkle;
        }
        // Reconstruct the whole tree.
        merkle.recompute(0, None);
        // Commit the first version in memory.
        // TODO(zz): Check when the roots become available.
        merkle.commit(start_tx_seq);
        merkle
    }

    pub fn new_with_subtrees(
        subtree_root_list: Vec<(usize, E)>,
        leaf_height: usize,
        start_tx_seq: Option<u64>,
    ) -> Result<Self> {
        let mut merkle = Self {
            layers: vec![vec![]],
            delta_nodes_map: HashMap::new(),
            root_to_tx_seq_map: HashMap::new(),
            min_depth: None,
            leaf_height,
            _a: Default::default(),
        };
        if subtree_root_list.is_empty() {
            if let Some(seq) = start_tx_seq {
                merkle.delta_nodes_map.insert(
                    seq,
                    DeltaNodes {
                        right_most_nodes: vec![],
                    },
                );
            }
            return Ok(merkle);
        }
        merkle.append_subtree_list(subtree_root_list)?;
        merkle.commit(start_tx_seq);
        Ok(merkle)
    }

    /// This is only used for the last chunk, so `leaf_height` is always 0 so far.
    pub fn new_with_depth(leaves: Vec<E>, depth: usize, start_tx_seq: Option<u64>) -> Self {
        if leaves.is_empty() {
            // Create an empty merkle tree with `depth`.
            let mut merkle = Self {
                layers: vec![vec![]; depth],
                delta_nodes_map: HashMap::new(),
                root_to_tx_seq_map: HashMap::new(),
                min_depth: Some(depth),
                leaf_height: 0,
                _a: Default::default(),
            };
            if let Some(seq) = start_tx_seq {
                merkle.delta_nodes_map.insert(
                    seq,
                    DeltaNodes {
                        right_most_nodes: vec![],
                    },
                );
            }
            merkle
        } else {
            let mut layers = vec![vec![]; depth];
            layers[0] = leaves;
            let mut merkle = Self {
                layers,
                delta_nodes_map: HashMap::new(),
                root_to_tx_seq_map: HashMap::new(),
                min_depth: Some(depth),
                leaf_height: 0,
                _a: Default::default(),
            };
            // Reconstruct the whole tree.
            merkle.recompute(0, None);
            // Commit the first version in memory.
            merkle.commit(start_tx_seq);
            merkle
        }
    }

    /// Return the new merkle root.
    pub fn append(&mut self, new_leaf: E) {
        self.layers[0].push(new_leaf);
        self.recompute_after_append(self.leaves() - 1);
    }

    pub fn append_list(&mut self, mut leaf_list: Vec<E>) {
        let start_index = self.leaves();
        self.layers[0].append(&mut leaf_list);
        self.recompute_after_append(start_index);
    }

    /// Append a leaf list by providing their intermediate node hash.
    /// The appended subtree must be aligned. And it's up to the caller to
    /// append the padding nodes for alignment.
    /// Other nodes in the subtree will be set to `null` nodes.
    /// TODO: Optimize to avoid storing the `null` nodes?
    pub fn append_subtree(&mut self, subtree_depth: usize, subtree_root: E) -> Result<()> {
        let start_index = self.leaves();
        self.append_subtree_inner(subtree_depth, subtree_root)?;
        self.recompute_after_append(start_index);
        Ok(())
    }

    pub fn append_subtree_list(&mut self, subtree_list: Vec<(usize, E)>) -> Result<()> {
        let start_index = self.leaves();
        for (subtree_depth, subtree_root) in subtree_list {
            self.append_subtree_inner(subtree_depth, subtree_root)?;
        }
        self.recompute_after_append(start_index);
        Ok(())
    }

    /// Change the value of the last leaf and return the new merkle root.
    /// This is needed if our merkle-tree in memory only keeps intermediate nodes instead of real leaves.
    pub fn update_last(&mut self, updated_leaf: E) {
        if self.layers[0].is_empty() {
            // Special case for the first data.
            self.layers[0].push(updated_leaf);
        } else {
            *self.layers[0].last_mut().unwrap() = updated_leaf;
        }
        self.recompute_after_append(self.leaves() - 1);
    }

    /// Fill an unknown `null` leaf with its real value.
    /// Panics if the leaf changes the merkle root or the index is out of range.
    /// TODO: Batch computing intermediate nodes.
    pub fn fill_leaf(&mut self, index: usize, leaf: E) {
        if self.layers[0][index] == E::null() {
            self.layers[0][index] = leaf;
            self.recompute_after_fill_leaves(index, index + 1);
        } else if self.layers[0][index] != leaf {
            panic!("Fill with invalid leaf")
        }
    }

    pub fn gen_range_proof(&self, start_index: usize, end_index: usize) -> Result<RangeProof<E>> {
        if end_index <= start_index {
            bail!(
                "invalid proof range: start={} end={}",
                start_index,
                end_index
            );
        }
        // TODO(zz): Optimize range proof.
        let left_proof = self.gen_proof(start_index)?;
        let right_proof = self.gen_proof(end_index - 1)?;
        Ok(RangeProof {
            left_proof,
            right_proof,
        })
    }

    pub fn check_root(&self, root: &E) -> bool {
        self.root_to_tx_seq_map.contains_key(root)
    }

    pub fn leaf_at(&self, position: usize) -> Result<Option<E>> {
        if position >= self.leaves() {
            bail!("Out of bound: position={} end={}", position, self.leaves());
        }
        if self.layers[0][position] != E::null() {
            Ok(Some(self.layers[0][position].clone()))
        } else {
            // The leaf hash is unknown.
            Ok(None)
        }
    }
}

impl<E: HashElement, A: Algorithm<E>> AppendMerkleTree<E, A> {
    pub fn commit(&mut self, tx_seq: Option<u64>) {
        if let Some(tx_seq) = tx_seq {
            if self.leaves() == 0 {
                // The state is empty, so we just save the root as `null`.
                // Note that this root should not be used.
                self.delta_nodes_map.insert(
                    tx_seq,
                    DeltaNodes {
                        right_most_nodes: vec![],
                    },
                );
                return;
            }
            let mut right_most_nodes = Vec::new();
            for layer in &self.layers {
                right_most_nodes.push((layer.len() - 1, layer.last().unwrap().clone()));
            }
            let root = self.root().clone();
            assert_eq!(root, right_most_nodes.last().unwrap().1);
            self.delta_nodes_map
                .insert(tx_seq, DeltaNodes::new(right_most_nodes));
            self.root_to_tx_seq_map.insert(root, tx_seq);
        }
    }

    fn before_extend_layer(&mut self, height: usize) {
        if height == self.layers.len() {
            self.layers.push(Vec::new());
        }
    }

    fn recompute_after_append(&mut self, start_index: usize) {
        self.recompute(start_index, None)
    }

    fn recompute_after_fill_leaves(&mut self, start_index: usize, end_index: usize) {
        self.recompute(start_index, Some(end_index))
    }

    /// Given a range of changed leaf nodes and recompute the tree.
    /// Since this tree is append-only, we always compute to the end.
    fn recompute(&mut self, mut start_index: usize, mut maybe_end_index: Option<usize>) {
        let mut height = 0;
        // Loop until we compute the new root and reach `tree_depth`.
        while self.layers[height].len() > 1 || height < self.layers.len() - 1 {
            let next_layer_start_index = start_index >> 1;
            if start_index % 2 == 1 {
                start_index -= 1;
            }

            let mut end_index = maybe_end_index.unwrap_or(self.layers[height].len());
            if end_index % 2 == 1 && end_index != self.layers[height].len() {
                end_index += 1;
            }
            let mut i = 0;
            let mut iter = self.layers[height][start_index..end_index].chunks_exact(2);
            // We cannot modify the parent layer while iterating the child layer,
            // so just keep the changes and update them later.
            let mut parent_update = Vec::new();
            while let Some([left, right]) = iter.next() {
                // If either left or right is null (unknown), we cannot compute the parent hash.
                // Note that if we are recompute a range of an existing tree,
                // we do not need to keep these possibly null parent. This is only saved
                // for the case of constructing a new tree from the leaves.
                let parent = if *left == E::null() || *right == E::null() {
                    E::null()
                } else {
                    A::parent(left, right)
                };
                parent_update.push((next_layer_start_index + i, parent));
                i += 1;
            }
            if let [r] = iter.remainder() {
                // Same as above.
                let parent = if *r == E::null() {
                    E::null()
                } else {
                    A::parent_single(r, height + self.leaf_height)
                };
                parent_update.push((next_layer_start_index + i, parent));
            }
            if !parent_update.is_empty() {
                self.before_extend_layer(height + 1);
            }
            // `parent_update` is in increasing order by `parent_index`, so
            // we can just overwrite `last_changed_parent_index` with new values.
            let mut last_changed_parent_index = None;
            for (parent_index, parent) in parent_update {
                match parent_index.cmp(&self.layers[height + 1].len()) {
                    Ordering::Less => {
                        // We do not overwrite with null.
                        if parent != E::null() {
                            if self.layers[height + 1][parent_index] != E::null()
                                && self.layers[height + 1][parent_index] != parent
                                && parent_index != self.layers[height + 1].len() - 1
                            {
                                // Recompute changes a node in the middle. This should be impossible
                                // if the inputs are valid.
                                panic!("Invalid append merkle tree!")
                            }
                            self.layers[height + 1][parent_index] = parent;
                            last_changed_parent_index = Some(parent_index);
                        }
                    }
                    Ordering::Equal => {
                        self.layers[height + 1].push(parent);
                        last_changed_parent_index = Some(parent_index);
                    }
                    Ordering::Greater => {
                        unreachable!("depth={}, parent_index={}", height, parent_index);
                    }
                }
            }
            // TODO(zz): Possible break if the tree remains the same from this layer.
            // We cannot break here just for the case of `append_subtree` where meaningful
            // nodes are inserted in the middle layer.
            maybe_end_index = last_changed_parent_index.map(|i| i + 1);

            height += 1;
            start_index = next_layer_start_index;
        }
    }

    fn append_subtree_inner(&mut self, subtree_depth: usize, subtree_root: E) -> Result<()> {
        if subtree_depth == 0 {
            bail!("Subtree depth should not be zero!");
        }
        if self.leaves() % (1 << (subtree_depth - 1)) != 0 {
            bail!(
                "The current leaves count is aligned with the merged subtree, leaves={}",
                self.leaves()
            );
        }
        for height in 0..(subtree_depth - 1) {
            self.before_extend_layer(height);
            let subtree_layer_size = 1 << (subtree_depth - 1 - height);
            self.layers[height].append(&mut vec![E::null(); subtree_layer_size]);
        }
        self.before_extend_layer(subtree_depth - 1);
        self.layers[subtree_depth - 1].push(subtree_root);
        Ok(())
    }

    #[cfg(test)]
    pub fn validate(&self, proof: &Proof<E>, leaf: &E, position: usize) -> Result<bool> {
        proof.validate::<A>(leaf, position)?;
        Ok(self.root_to_tx_seq_map.contains_key(&proof.root()))
    }

    pub fn revert_to(&mut self, tx_seq: u64) -> Result<()> {
        if self.layers[0].is_empty() {
            // Any previous state of an empty tree is always empty.
            return Ok(());
        }
        let delta_nodes = self
            .delta_nodes_map
            .get(&tx_seq)
            .ok_or_else(|| anyhow!("tx_seq unavailable, root={:?}", tx_seq))?;
        // Dropping the upper layers that are not in the old merkle tree.
        self.layers.truncate(delta_nodes.right_most_nodes.len());
        for (height, (last_index, right_most_node)) in
            delta_nodes.right_most_nodes.iter().enumerate()
        {
            self.layers[height].truncate(*last_index + 1);
            self.layers[height][*last_index] = right_most_node.clone();
        }
        self.clear_after(tx_seq);
        Ok(())
    }

    pub fn at_root_version(&self, root_hash: &E) -> Result<HistoryTree<E>> {
        let tx_seq = self
            .root_to_tx_seq_map
            .get(root_hash)
            .ok_or_else(|| anyhow!("old root unavailable, root={:?}", root_hash))?;
        let delta_nodes = self
            .delta_nodes_map
            .get(tx_seq)
            .ok_or_else(|| anyhow!("tx_seq unavailable, tx_seq={:?}", tx_seq))?;
        if delta_nodes.height() == 0 {
            bail!("empty tree");
        }
        Ok(HistoryTree {
            layers: &self.layers,
            delta_nodes,
            leaf_height: self.leaf_height,
        })
    }

    pub fn reset(&mut self) {
        self.layers = match self.min_depth {
            None => vec![vec![]],
            Some(depth) => vec![vec![]; depth],
        };
    }

    fn clear_after(&mut self, tx_seq: u64) {
        let mut tx_seq = tx_seq + 1;
        while self.delta_nodes_map.contains_key(&tx_seq) {
            if let Some(nodes) = self.delta_nodes_map.remove(&tx_seq) {
                if nodes.height() != 0 {
                    self.root_to_tx_seq_map.remove(nodes.root());
                }
            }
            tx_seq += 1;
        }
    }
}

#[derive(Clone, Debug)]
struct DeltaNodes<E: HashElement> {
    /// The right most nodes in a layer and its position.
    right_most_nodes: Vec<(usize, E)>,
}

impl<E: HashElement> DeltaNodes<E> {
    fn new(right_most_nodes: Vec<(usize, E)>) -> Self {
        Self { right_most_nodes }
    }

    fn get(&self, height: usize, position: usize) -> Result<Option<&E>> {
        if height >= self.right_most_nodes.len() || position > self.right_most_nodes[height].0 {
            Err(anyhow!("position out of tree range"))
        } else if position == self.right_most_nodes[height].0 {
            Ok(Some(&self.right_most_nodes[height].1))
        } else {
            Ok(None)
        }
    }

    fn layer_len(&self, height: usize) -> usize {
        self.right_most_nodes[height].0 + 1
    }

    fn height(&self) -> usize {
        self.right_most_nodes.len()
    }

    fn root(&self) -> &E {
        &self.right_most_nodes.last().unwrap().1
    }
}

pub struct HistoryTree<'m, E: HashElement> {
    /// A reference to the global tree nodes.
    layers: &'m Vec<Vec<E>>,
    /// The delta nodes that are difference from `layers`.
    /// This could be a reference, we just take ownership for convenience.
    delta_nodes: &'m DeltaNodes<E>,

    leaf_height: usize,
}

impl<E: HashElement, A: Algorithm<E>> MerkleTreeRead for AppendMerkleTree<E, A> {
    type E = E;

    fn node(&self, layer: usize, index: usize) -> &Self::E {
        &self.layers[layer][index]
    }

    fn height(&self) -> usize {
        self.layers.len()
    }

    fn layer_len(&self, layer_height: usize) -> usize {
        self.layers[layer_height].len()
    }

    fn padding_node(&self, height: usize) -> Self::E {
        E::end_pad(height + self.leaf_height)
    }
}

impl<'a, E: HashElement> MerkleTreeRead for HistoryTree<'a, E> {
    type E = E;
    fn node(&self, layer: usize, index: usize) -> &Self::E {
        match self.delta_nodes.get(layer, index).expect("range checked") {
            Some(node) => node,
            None => &self.layers[layer][index],
        }
    }

    fn height(&self) -> usize {
        self.delta_nodes.height()
    }

    fn layer_len(&self, layer_height: usize) -> usize {
        self.delta_nodes.layer_len(layer_height)
    }

    fn padding_node(&self, height: usize) -> Self::E {
        E::end_pad(height + self.leaf_height)
    }
}

#[macro_export]
macro_rules! ensure_eq {
    ($given:expr, $expected:expr) => {
        ensure!(
            $given == $expected,
            format!(
                "equal check fails! {}:{}: {}={:?}, {}={:?}",
                file!(),
                line!(),
                stringify!($given),
                $given,
                stringify!($expected),
                $expected,
            )
        );
    };
}

#[cfg(test)]
mod tests {
    use crate::merkle_tree::MerkleTreeRead;
    use crate::sha3::Sha3Algorithm;
    use crate::AppendMerkleTree;
    use ethereum_types::H256;

    #[test]
    fn test_proof() {
        let n = [1, 2, 6, 1025];
        for entry_len in n {
            let mut data = Vec::new();
            for _ in 0..entry_len {
                data.push(H256::random());
            }
            let mut merkle =
                AppendMerkleTree::<H256, Sha3Algorithm>::new(vec![H256::zero()], 0, None);
            merkle.append_list(data.clone());
            merkle.commit(Some(0));
            verify(&data, &merkle);

            data.push(H256::random());
            merkle.append(*data.last().unwrap());
            merkle.commit(Some(1));
            verify(&data, &merkle);

            for _ in 0..6 {
                data.push(H256::random());
            }
            merkle.append_list(data[data.len() - 6..].to_vec());
            merkle.commit(Some(2));
            verify(&data, &merkle);
        }
    }

    fn verify(data: &Vec<H256>, merkle: &AppendMerkleTree<H256, Sha3Algorithm>) {
        for i in 0..data.len() {
            let proof = merkle.gen_proof(i + 1).unwrap();
            let r = merkle.validate(&proof, &data[i], i + 1);
            assert!(matches!(r, Ok(true)), "{:?}", r);
        }
        for i in (0..data.len()).step_by(6) {
            let end = std::cmp::min(i + 3, data.len());
            let range_proof = merkle.gen_range_proof(i + 1, end + 1).unwrap();
            let r = range_proof.validate::<Sha3Algorithm>(&data[i..end], i + 1);
            assert!(r.is_ok(), "{:?}", r);
        }
    }
}
