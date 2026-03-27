// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title N42 On-Chain Randomness Precompile
/// @author N42 Team
/// @notice Provides verifiable randomness derived from BFT consensus (CommitQC aggregate BLS signature).
/// @dev Deployed at address 0x0000000000000000000000000000000000000302.
///      The randomness source is `keccak256(CommitQC.aggregate_signature || block_number)`,
///      which is a threshold VUF — unpredictable by any party controlling fewer than 2f+1 validators.
///
///      **Wire format:** The first byte of calldata selects the function:
///        - 0x00: getRandom()
///        - 0x01: getRandomInRange(uint256 max)
///        - 0x02: getRandomWithSeed(bytes32 seed)
///
///      Unlike standard Solidity ABI encoding, this precompile uses a 1-byte selector
///      (not 4-byte function selector). Use the helper library `N42Random` below for
///      correct low-level calls.
///
/// @custom:address 0x0000000000000000000000000000000000000302
/// @custom:chain-id 4242
interface IN42Randomness {
    /// @notice Returns a 32-byte random value for the current block.
    /// @dev Computed as `keccak256(prevrandao)` where prevrandao is derived from
    ///      the CommitQC aggregate BLS signature of the previous block.
    ///      Same value for all calls within the same block.
    /// @return Random 32-byte value (deterministic per block).
    /// @custom:gas 100
    /// @custom:selector 0x00
    function getRandom() external view returns (bytes32);

    /// @notice Returns a random uint256 in [0, max).
    /// @dev Computed as `keccak256(prevrandao) % max`. Reverts if max == 0.
    /// @param max Upper bound (exclusive). Must be > 0.
    /// @return Random value in range [0, max).
    /// @custom:gas 150
    /// @custom:selector 0x01
    function getRandomInRange(uint256 max) external view returns (uint256);

    /// @notice Returns a 32-byte random value seeded with caller-provided data.
    /// @dev Computed as `keccak256(prevrandao || seed)`. Different seeds produce
    ///      different values within the same block (useful for multiple independent
    ///      random draws in one transaction).
    /// @param seed Caller-provided 32-byte seed for domain separation.
    /// @return Random 32-byte value unique to this (block, seed) pair.
    /// @custom:gas 100
    /// @custom:selector 0x02
    function getRandomWithSeed(bytes32 seed) external view returns (bytes32);
}

/// @title N42 Randomness Helper Library
/// @notice Safe wrapper for calling the N42 randomness precompile.
/// @dev Handles the 1-byte selector wire format and low-level staticcall.
///
/// Usage:
/// ```solidity
/// import {N42Random} from "./IN42Randomness.sol";
///
/// contract MyLottery {
///     function draw() external view returns (uint256 winner) {
///         winner = N42Random.randomInRange(100);  // 0..99
///     }
///
///     function drawMultiple(uint8 count) external view returns (uint256[] memory) {
///         uint256[] memory results = new uint256[](count);
///         for (uint8 i = 0; i < count; i++) {
///             results[i] = N42Random.randomWithSeed(bytes32(uint256(i)));
///         }
///         return results;
///     }
/// }
/// ```
library N42Random {
    address internal constant PRECOMPILE = 0x0000000000000000000000000000000000000302;

    /// @notice Get a 32-byte random value for this block.
    /// @return result Random bytes32 value.
    function random() internal view returns (bytes32 result) {
        (bool ok, bytes memory out) = PRECOMPILE.staticcall(hex"00");
        require(ok && out.length == 32, "N42Random: getRandom failed");
        result = bytes32(out);
    }

    /// @notice Get a random uint256 in [0, max).
    /// @param max Upper bound (exclusive), must be > 0.
    /// @return result Random value in [0, max).
    function randomInRange(uint256 max) internal view returns (uint256 result) {
        (bool ok, bytes memory out) = PRECOMPILE.staticcall(abi.encodePacked(hex"01", max));
        require(ok && out.length == 32, "N42Random: getRandomInRange failed");
        result = abi.decode(out, (uint256));
    }

    /// @notice Get a seeded random bytes32 value.
    /// @param seed Domain separation seed (different seeds → different values in same block).
    /// @return result Random bytes32 value unique to (block, seed).
    function randomWithSeed(bytes32 seed) internal view returns (bytes32 result) {
        (bool ok, bytes memory out) = PRECOMPILE.staticcall(abi.encodePacked(hex"02", seed));
        require(ok && out.length == 32, "N42Random: getRandomWithSeed failed");
        result = bytes32(out);
    }
}
