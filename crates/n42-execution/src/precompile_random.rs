//! N42 randomness precompile — pure logic + revm adapter.
//!
//! Three function selectors (first byte of input):
//! - `0x00`: `getRandom()` → `keccak256(prevrandao)` (32 bytes, 100 gas)
//! - `0x01`: `getRandomInRange(max)` → `keccak256(prevrandao) % max` (32 bytes, 150 gas)
//! - `0x02`: `getRandomWithSeed(seed)` → `keccak256(prevrandao || seed)` (32 bytes, 100 gas)

use alloy_primitives::{B256, Bytes, U256, keccak256};
use revm::precompile::{PrecompileHalt, PrecompileOutput, PrecompileResult};
use std::cell::Cell;

thread_local! {
    /// Block's prevrandao, injected before EVM execution.
    /// Used by the randomness precompile since revm's precompile function signature
    /// does not provide access to the block environment.
    static BLOCK_PREVRANDAO: Cell<B256> = const { Cell::new(B256::ZERO) };
}

/// Set the block's prevrandao for the randomness precompile.
/// Must be called before each block's EVM execution.
pub fn set_block_randomness(prevrandao: B256) {
    BLOCK_PREVRANDAO.set(prevrandao);
}

/// Get the current block's prevrandao.
fn get_block_randomness() -> B256 {
    BLOCK_PREVRANDAO.get()
}

/// Adapter for revm precompile registration.
/// Reads prevrandao from thread-local storage set by [`set_block_randomness()`].
///
/// The `reservoir` parameter (added in revm 38 / EIP-8037) tracks the remaining
/// state-gas reservoir for this call; precompiles that do not touch state pass
/// it through unchanged.
pub fn revm_precompile_fn(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    let prevrandao = get_block_randomness();
    match execute_randomness(input, gas_limit, prevrandao) {
        Ok(result) => Ok(PrecompileOutput::new(
            result.gas_used,
            Bytes::from(result.output),
            reservoir,
        )),
        // revm 38 split fatal vs non-fatal: input-shape / out-of-gas issues
        // become `PrecompileOutput::halt(...)` (non-fatal) instead of `Err`
        // (which would abort the entire EVM transaction).
        Err(PrecompileCallError::OutOfGas) => {
            Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir))
        }
        Err(e) => Ok(PrecompileOutput::halt(
            PrecompileHalt::other(e.to_string()),
            reservoir,
        )),
    }
}

/// N42 randomness precompile address (for future EVM registration).
pub const RANDOMNESS_ADDRESS: [u8; 20] = {
    let mut addr = [0u8; 20];
    addr[18] = 0x03;
    addr[19] = 0x02;
    addr
};

/// Gas costs.
pub const GAS_RANDOM: u64 = 100;
pub const GAS_RANDOM_RANGE: u64 = 150;

/// Result of precompile execution.
#[derive(Debug)]
pub struct PrecompileCallResult {
    pub gas_used: u64,
    pub output: Vec<u8>,
}

/// Error from precompile execution.
#[derive(Debug)]
pub enum PrecompileCallError {
    OutOfGas,
    InputTooShort,
    ZeroMax,
    UnknownSelector(u8),
}

impl std::fmt::Display for PrecompileCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfGas => write!(f, "out of gas"),
            Self::InputTooShort => write!(f, "input too short"),
            Self::ZeroMax => write!(f, "max cannot be zero"),
            Self::UnknownSelector(s) => write!(f, "unknown selector: 0x{s:02x}"),
        }
    }
}

/// Execute the N42 randomness precompile.
///
/// `prevrandao` is the block's prev_randao value (derived from CommitQC aggregate signature).
pub fn execute_randomness(
    input: &[u8],
    gas_limit: u64,
    prevrandao: B256,
) -> Result<PrecompileCallResult, PrecompileCallError> {
    let selector = input.first().copied().unwrap_or(0x00);

    match selector {
        // getRandom() → keccak256(prevrandao)
        0x00 => {
            if gas_limit < GAS_RANDOM {
                return Err(PrecompileCallError::OutOfGas);
            }
            let hash = keccak256(prevrandao);
            Ok(PrecompileCallResult {
                gas_used: GAS_RANDOM,
                output: hash.to_vec(),
            })
        }

        // getRandomInRange(max: u256) → keccak256(prevrandao) % max
        0x01 => {
            if gas_limit < GAS_RANDOM_RANGE {
                return Err(PrecompileCallError::OutOfGas);
            }
            if input.len() < 33 {
                return Err(PrecompileCallError::InputTooShort);
            }
            let max_bytes: [u8; 32] = input[1..33].try_into().expect("slice is 32 bytes");
            let max = U256::from_be_bytes(max_bytes);
            if max.is_zero() {
                return Err(PrecompileCallError::ZeroMax);
            }
            let hash = keccak256(prevrandao);
            let value = U256::from_be_bytes(hash.0) % max;
            Ok(PrecompileCallResult {
                gas_used: GAS_RANDOM_RANGE,
                output: value.to_be_bytes::<32>().to_vec(),
            })
        }

        // getRandomWithSeed(seed: bytes32) → keccak256(prevrandao || seed)
        0x02 => {
            if gas_limit < GAS_RANDOM {
                return Err(PrecompileCallError::OutOfGas);
            }
            if input.len() < 33 {
                return Err(PrecompileCallError::InputTooShort);
            }
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(prevrandao.as_ref());
            buf[32..64].copy_from_slice(&input[1..33]);
            let hash = keccak256(buf);
            Ok(PrecompileCallResult {
                gas_used: GAS_RANDOM,
                output: hash.to_vec(),
            })
        }

        _ => Err(PrecompileCallError::UnknownSelector(selector)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_prevrandao() -> B256 {
        B256::from([0xAB; 32])
    }

    #[test]
    fn test_get_random_returns_32_bytes() {
        let input = [0x00];
        let result = execute_randomness(&input, 1000, dummy_prevrandao()).unwrap();
        assert_eq!(result.output.len(), 32);
        assert_eq!(result.gas_used, GAS_RANDOM);
    }

    #[test]
    fn test_get_random_deterministic() {
        let input = [0x00];
        let pr = dummy_prevrandao();
        let r1 = execute_randomness(&input, 1000, pr).unwrap();
        let r2 = execute_randomness(&input, 1000, pr).unwrap();
        assert_eq!(r1.output, r2.output);
    }

    #[test]
    fn test_get_random_in_range() {
        let max = 100u64;
        let mut input = vec![0x01];
        input.extend_from_slice(&U256::from(max).to_be_bytes::<32>());

        let result = execute_randomness(&input, 1000, dummy_prevrandao()).unwrap();
        assert_eq!(result.output.len(), 32);
        assert_eq!(result.gas_used, GAS_RANDOM_RANGE);

        let value = U256::from_be_bytes(<[u8; 32]>::try_from(result.output.as_slice()).unwrap());
        assert!(value < U256::from(max));
    }

    #[test]
    fn test_get_random_in_range_zero_max_errors() {
        let mut input = vec![0x01];
        input.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());

        let err = execute_randomness(&input, 1000, dummy_prevrandao()).unwrap_err();
        assert!(matches!(err, PrecompileCallError::ZeroMax));
    }

    #[test]
    fn test_get_random_with_seed() {
        let seed_a = B256::from([0x01; 32]);
        let seed_b = B256::from([0x02; 32]);

        let mut input_a = vec![0x02];
        input_a.extend_from_slice(seed_a.as_ref());
        let mut input_b = vec![0x02];
        input_b.extend_from_slice(seed_b.as_ref());

        let pr = dummy_prevrandao();
        let ra = execute_randomness(&input_a, 1000, pr).unwrap();
        let rb = execute_randomness(&input_b, 1000, pr).unwrap();

        assert_eq!(ra.output.len(), 32);
        assert_eq!(ra.gas_used, GAS_RANDOM);
        assert_ne!(
            ra.output, rb.output,
            "different seeds must produce different output"
        );
    }

    #[test]
    fn test_out_of_gas() {
        // getRandom needs 100 gas
        let err = execute_randomness(&[0x00], 50, dummy_prevrandao()).unwrap_err();
        assert!(matches!(err, PrecompileCallError::OutOfGas));

        // getRandomInRange needs 150 gas
        let mut input = vec![0x01];
        input.extend_from_slice(&U256::from(10u64).to_be_bytes::<32>());
        let err = execute_randomness(&input, 100, dummy_prevrandao()).unwrap_err();
        assert!(matches!(err, PrecompileCallError::OutOfGas));

        // getRandomWithSeed needs 100 gas
        let mut input = vec![0x02];
        input.extend_from_slice(&[0x00; 32]);
        let err = execute_randomness(&input, 50, dummy_prevrandao()).unwrap_err();
        assert!(matches!(err, PrecompileCallError::OutOfGas));
    }

    #[test]
    fn test_unknown_selector() {
        let err = execute_randomness(&[0xFF], 1000, dummy_prevrandao()).unwrap_err();
        assert!(matches!(err, PrecompileCallError::UnknownSelector(0xFF)));
    }
}
