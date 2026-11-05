//! The beacon block header a beacon API serves, and its SSZ root, which is the block's root.

use alloy::primitives::B256;
use serde::{Deserialize, Deserializer};

use crate::seed_relay::ssz::merkleize;

/// A beacon block header as the beacon API's `/eth/v1/beacon/headers` writes it, with its integers
/// as decimal strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconBlockHeader {
    #[serde(deserialize_with = "decimal")]
    pub slot: u64,
    #[serde(deserialize_with = "decimal")]
    pub proposer_index: u64,
    pub parent_root: B256,
    pub state_root: B256,
    pub body_root: B256,
}

impl BeaconBlockHeader {
    /// Its `hash_tree_root`: its five fields as the leaves of an 8-leaf tree, integers
    /// little-endian, as `SeedRelay.hashTreeRoot` computes it.
    pub fn root(&self) -> B256 {
        let mut leaves = [[0u8; 32]; 5];
        leaves[0][..8].copy_from_slice(&self.slot.to_le_bytes());
        leaves[1][..8].copy_from_slice(&self.proposer_index.to_le_bytes());
        leaves[2] = self.parent_root.0;
        leaves[3] = self.state_root.0;
        leaves[4] = self.body_root.0;
        B256::from(merkleize(leaves.to_vec(), 5).expect("five chunks fit an 8-leaf tree"))
    }
}

/// A `u64` the beacon API writes as a decimal string.
fn decimal<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mainnet's finalized block at slot 15583232, as the beacon API served it; its root is the
    /// block root the API named for it.
    #[test]
    fn a_mainnet_header_hashes_to_its_block_root() {
        let header: BeaconBlockHeader = serde_json::from_str(
            r#"{"slot":"15583232","proposer_index":"2210630",
                "parent_root":"0xbba3e3c1cae0845ac346bec948004fb19c6aa2e8e16c05e275e10ba510ceb11f",
                "state_root":"0xa4f17e670de4c5c2c6d44ac53bc34681b21b563165c68e4649584255da616c59",
                "body_root":"0x00ecd68cf44fd612d8972433dbef7141e457af406327e82bc04f06e117b45fd4"}"#,
        )
        .unwrap();
        let root: B256 = "0x59ba1e55f90b6ccf084411a2eeee75d2d9bb40947721cec6fa9c1c8e2a05358b".parse().unwrap();
        assert_eq!(header.root(), root);
        let other = BeaconBlockHeader { proposer_index: header.proposer_index + 1, ..header };
        assert_ne!(other.root(), root);
    }
}
