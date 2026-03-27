# N42 Precompiled Contracts

## Randomness Precompile (0x0302)

| Property | Value |
|----------|-------|
| Address | `0x0000000000000000000000000000000000000302` |
| Source | CommitQC aggregate BLS signature (threshold VUF, 2f+1 signers) |
| Determinism | Same value for all calls within the same block |
| Unpredictability | No single validator can predict the value |

### Functions

| Selector | Function | Gas | Input | Output |
|----------|----------|-----|-------|--------|
| `0x00` | `getRandom()` | 100 | 1 byte (selector only) | `bytes32` — `keccak256(prevrandao)` |
| `0x01` | `getRandomInRange(uint256 max)` | 150 | 1 + 32 bytes | `uint256` — `keccak256(prevrandao) % max` |
| `0x02` | `getRandomWithSeed(bytes32 seed)` | 100 | 1 + 32 bytes | `bytes32` — `keccak256(prevrandao \|\| seed)` |

### Wire Format

This precompile uses a **1-byte selector** (first byte of calldata), NOT the standard 4-byte Solidity function selector. Use the `N42Random` library for correct encoding.

### Solidity Usage

```solidity
import {N42Random} from "@n42/contracts/IN42Randomness.sol";

contract Lottery {
    function draw(uint256 participants) external view returns (uint256 winner) {
        winner = N42Random.randomInRange(participants);
    }
}
```

### Low-Level Call (ethers.js / viem)

```javascript
// getRandom()
const result = await provider.call({
  to: "0x0000000000000000000000000000000000000302",
  data: "0x00"
});
// result: 0x<64 hex chars> (32 bytes)

// getRandomInRange(100)
const max = ethers.zeroPadValue(ethers.toBeHex(100), 32);
const result = await provider.call({
  to: "0x0000000000000000000000000000000000000302",
  data: "0x01" + max.slice(2)
});

// getRandomWithSeed(seed)
const seed = ethers.id("my-domain"); // keccak256 as seed
const result = await provider.call({
  to: "0x0000000000000000000000000000000000000302",
  data: "0x02" + seed.slice(2)
});
```

### Security Properties

- **Threshold VUF**: The randomness is derived from a BLS aggregate signature requiring 2f+1 out of n validators. No coalition smaller than this threshold can predict the value.
- **Per-block determinism**: All calls within the same block return the same base randomness. Use `getRandomWithSeed()` with distinct seeds for multiple independent values.
- **Genesis block**: Returns `keccak256(B256::ZERO)` (the genesis block has no CommitQC).
- **Not manipulable by leader**: The leader proposes the block but the randomness comes from the previous block's CommitQC, which the leader cannot influence alone.

### Error Conditions

| Condition | Behavior |
|-----------|----------|
| Insufficient gas | Precompile reverts (out of gas) |
| `max == 0` for `getRandomInRange` | Precompile reverts |
| Input too short for `0x01`/`0x02` (< 33 bytes) | Precompile reverts |
| Unknown selector | Precompile reverts |

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `N42_JMT` | `0` | Enable JMT state tree (unrelated but commonly co-enabled) |
| `N42_ZK_PROOF` | `0` | Enable ZK proof generation |
| `N42_PARALLEL_EVM` | `0` | Enable Block-STM parallel execution |
