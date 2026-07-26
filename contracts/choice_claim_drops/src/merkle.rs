use cosmwasm_std::{HexBinary, Uint128};
use sha2::{Digest, Sha256};

/// Cross-language leaf spec — any tree builder (TS keeper, trippytools FE)
/// must reproduce these bytes exactly:
///   leaf = sha256(utf8("{bech32_address}:{amount_base_units_decimal}"))
/// The address is the claimant's canonical lowercase bech32 string; the amount
/// is the LIFETIME CUMULATIVE allocation in base units, no separators.
pub fn leaf_hash(addr: &str, amount: Uint128) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(addr.as_bytes());
    h.update(b":");
    h.update(amount.to_string().as_bytes());
    h.finalize().into()
}

/// Sorted-pair (OpenZeppelin-style) verification: each parent hashes
/// concat(min(a, b), max(a, b)), so proofs carry no left/right flags and an
/// odd node at any level promotes unchanged (its proof simply omits that
/// level). An empty proof means a single-leaf tree (leaf == root).
pub fn verify(root: &[u8], mut node: [u8; 32], proof: &[HexBinary]) -> bool {
    for step in proof {
        let sib = step.as_slice();
        if sib.len() != 32 {
            return false;
        }
        let mut h = Sha256::new();
        if node.as_slice() <= sib {
            h.update(node);
            h.update(sib);
        } else {
            h.update(sib);
            h.update(node);
        }
        node = h.finalize().into();
    }
    node.as_slice() == root
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors pinning the byte-level spec for other-language builders.
    #[test]
    fn leaf_hash_golden_vector() {
        let leaf = leaf_hash(
            "inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
            Uint128::new(1_000_000),
        );
        assert_eq!(
            HexBinary::from(leaf).to_hex(),
            "82021d82ef5d5a5db1fdf2b7204e4179da4676e67104d56464b03c767e16ce48"
        );
    }

    #[test]
    fn sorted_pair_golden_vector() {
        // a = sha256("a-leaf-x"), b = sha256("b-leaf-y"); parent hashes the
        // lexicographically smaller node first.
        let a: [u8; 32] =
            HexBinary::from_hex("7b631c0212ee9c2e8715fadd136a31706728c9cefa23cb42b681e0a5ec4792c5")
                .unwrap()
                .to_array()
                .unwrap();
        let b =
            HexBinary::from_hex("079be04b5b153b7a84b4df326ad8d1f1f30fd5292cab32560dec512b5eb366c1")
                .unwrap();
        let parent =
            HexBinary::from_hex("55a8565a798073b99f9332a82ce34227e834d115b9fa6639b348f687b820953a")
                .unwrap();
        assert!(verify(parent.as_slice(), a, &[b]));
    }

    #[test]
    fn single_leaf_tree_is_its_own_root() {
        let leaf = leaf_hash("inj1abc", Uint128::new(1));
        assert!(verify(&leaf, leaf, &[]));
    }

    #[test]
    fn rejects_malformed_sibling() {
        let leaf = leaf_hash("inj1abc", Uint128::new(1));
        assert!(!verify(
            &leaf,
            leaf,
            &[HexBinary::from_hex("aabb").unwrap()]
        ));
    }
}
