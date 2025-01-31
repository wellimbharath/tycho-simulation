use alloy_primitives::{I256, U256};
use tycho_core::Bytes;

pub mod liquidity_math;
mod solidity_math;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_list;
pub mod tick_math;

#[derive(Debug)]
pub struct SwapState {
    pub amount_remaining: I256,
    pub amount_calculated: I256,
    pub sqrt_price: U256,
    pub tick: i32,
    pub liquidity: u128,
}

#[derive(Debug)]
pub struct StepComputation {
    pub sqrt_price_start: U256,
    pub tick_next: i32,
    pub initialized: bool,
    pub sqrt_price_next: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

#[derive(Debug)]
pub struct SwapResults {
    pub amount_calculated: I256,
    pub sqrt_price: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub gas_used: U256,
}

/// Converts a slice of bytes representing a big-endian 24-bit signed integer
/// to a 32-bit signed integer.
///
/// # Arguments
/// * `val` - A reference to a `Bytes` type, which should contain at most three bytes.
///
/// # Returns
/// * The 32-bit signed integer representation of the input bytes.
pub fn i24_be_bytes_to_i32(val: &Bytes) -> i32 {
    let bytes_slice = val.as_ref();
    let bytes_len = bytes_slice.len();
    let mut result = 0i32;

    for (i, &byte) in bytes_slice.iter().enumerate() {
        result |= (byte as i32) << (8 * (bytes_len - 1 - i));
    }

    // If the first byte (most significant byte) has its most significant bit set (0x80),
    // perform sign extension for negative numbers.
    if bytes_len > 0 && bytes_slice[0] & 0x80 != 0 {
        result |= -1i32 << (8 * bytes_len);
    }
    result
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_core::Bytes;

    use crate::evm::protocol::utils::uniswap::i24_be_bytes_to_i32;

    #[test]
    fn test_i24_be_bytes_to_i32() {
        let val = Bytes::from_str("0xfeafc6").unwrap();
        let converted = i24_be_bytes_to_i32(&val);
        assert_eq!(converted, -86074);
        let val = Bytes::from_str("0x02dd").unwrap();
        let converted = i24_be_bytes_to_i32(&val);
        assert_eq!(converted, 733);
        let val = Bytes::from_str("0xe2bb").unwrap();
        let converted = i24_be_bytes_to_i32(&val);
        assert_eq!(converted, -7493);
    }
}
