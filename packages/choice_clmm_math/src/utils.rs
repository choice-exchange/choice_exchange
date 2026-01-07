#![allow(clippy::manual_div_ceil)]

use cosmwasm_std::Uint256;
use uint::construct_uint;

// Temporary helper to allow compilation until full TickMath is ported
pub fn mock_tick_to_price(_tick: i32) -> Uint256 {
    // Return 1.0 (Q96) for now
    Uint256::from_u128(1) << 96
}

// Construct a U256 type from the uint crate
construct_uint! {
    pub struct U256(4);
}

/// Convert cosmwasm_std::Uint256 to internal U256
pub fn to_u256(n: Uint256) -> U256 {
    let bytes = n.to_be_bytes();
    U256::from_big_endian(&bytes)
}

/// Convert internal U256 back to cosmwasm_std::Uint256
pub fn from_u256(n: U256) -> Uint256 {
    // Manually construct the big-endian byte array from the internal [u64; 4] words.
    // U256 stores data as little-endian u64s: words[0] is least significant, words[3] is most significant.
    let words = n.0;

    let mut bytes = [0u8; 32];

    // Copy words into bytes in Big-Endian order
    bytes[0..8].copy_from_slice(&words[3].to_be_bytes());
    bytes[8..16].copy_from_slice(&words[2].to_be_bytes());
    bytes[16..24].copy_from_slice(&words[1].to_be_bytes());
    bytes[24..32].copy_from_slice(&words[0].to_be_bytes());

    Uint256::from_be_bytes(bytes)
}
