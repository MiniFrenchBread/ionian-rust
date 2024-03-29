use crate::sha3::Sha3Algorithm;
use crate::Proof;
use anyhow::{bail, Result};
use ethereum_types::H256;
use lazy_static::lazy_static;
use ssz::{Decode, Encode};
use std::fmt::Debug;
use std::hash::Hash;
use tracing::trace;

pub trait HashElement:
    Clone + Debug + Eq + Hash + AsRef<[u8]> + AsMut<[u8]> + Decode + Encode + Send + Sync
{
    fn end_pad(height: usize) -> Self;
    fn null() -> Self;
    fn is_null(&self) -> bool {
        self == &Self::null()
    }
}

impl HashElement for H256 {
    fn end_pad(height: usize) -> Self {
        ZERO_HASHES[height]
    }

    fn null() -> Self {
        H256::repeat_byte(1)
    }
}

lazy_static! {
    static ref ZERO_HASHES: [H256; 64] = {
        let leaf_zero_hash: H256 = Sha3Algorithm::leaf(&[0u8; 256]);
        let mut list = [H256::zero(); 64];
        list[0] = leaf_zero_hash;
        for i in 1..list.len() {
            list[i] = Sha3Algorithm::parent(&list[i - 1], &list[i - 1]);
        }
        list
    };
}

pub trait Algorithm<E: HashElement> {
    fn parent(left: &E, right: &E) -> E;
    fn parent_single(r: &E, height: usize) -> E {
        Self::parent(r, &E::end_pad(height))
    }
    fn leaf(data: &[u8]) -> E;
}

pub trait MerkleTreeRead {
    type E: HashElement;
    fn node(&self, layer: usize, index: usize) -> &Self::E;
    fn height(&self) -> usize;
    fn layer_len(&self, layer_height: usize) -> usize;
    fn padding_node(&self, height: usize) -> Self::E;

    fn leaves(&self) -> usize {
        self.layer_len(0)
    }

    fn root(&self) -> &Self::E {
        self.node(self.height() - 1, 0)
    }

    fn gen_proof(&self, leaf_index: usize) -> Result<Proof<Self::E>> {
        if leaf_index >= self.leaves() {
            bail!(
                "leaf index out of bound: leaf_index={} total_leaves={}",
                leaf_index,
                self.leaves()
            );
        }
        if self.node(0, leaf_index) == &Self::E::null() {
            bail!("Not ready to generate proof for leaf_index={}", leaf_index);
        }
        if self.height() == 1 {
            return Ok(Proof::new(
                vec![self.root().clone(), self.root().clone()],
                vec![],
            ));
        }
        let mut lemma: Vec<Self::E> = Vec::with_capacity(self.height()); // path + root
        let mut path: Vec<bool> = Vec::with_capacity(self.height() - 2); // path - 1
        let mut index_in_layer = leaf_index;
        lemma.push(self.node(0, leaf_index).clone());
        for height in 0..(self.height() - 1) {
            trace!(
                "gen_proof: height={} index={} hash={:?}",
                height,
                index_in_layer,
                self.node(height, index_in_layer)
            );
            if index_in_layer % 2 == 0 {
                path.push(true);
                if index_in_layer + 1 == self.layer_len(height) {
                    // TODO: This can be skipped if the tree size is available in validation.
                    lemma.push(self.padding_node(height));
                } else {
                    lemma.push(self.node(height, index_in_layer + 1).clone());
                }
            } else {
                path.push(false);
                lemma.push(self.node(height, index_in_layer - 1).clone());
            }
            index_in_layer >>= 1;
        }
        lemma.push(self.root().clone());
        Ok(Proof::new(lemma, path))
    }
}
