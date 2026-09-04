//! gov5 block rewards.
//!
//! gov5's HotStuff `Finalize` credits `hotstuff.devBlockReward` to the coinbase
//! and to `devFaucetAddress`, lists both as the block's rewards and commits to
//! them in the withdrawals-root slot as `hash.DeriveSha(block.Rewards)`: the
//! keccak of the concatenated `RLP([address, amount])` items (the empty list
//! hashes to `keccak256("")`). The credit is part of the state root, so an
//! execution layer that does not apply it disagrees with every block.
//!
//! Inside reth a reward is carried as an EIP-4895 withdrawal: the executor
//! credits withdrawals after the transactions, exactly where gov5 credits its
//! rewards. A reward in wei becomes a withdrawal in gwei, which is exact for
//! every reward gov5 can configure (`devBlockReward` is a whole number of
//! gwei on every deployed chainspec) and rejected otherwise.

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::{Encodable, Header as RlpHeader};

/// One gov5 block reward: `block.Reward{Address, Amount}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gov5Reward {
    pub address: Address,
    pub amount: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Gov5RewardError {
    #[error("gov5 reward to {address} of {amount} wei is not a whole number of gwei")]
    NotGweiAligned { address: Address, amount: U256 },
    #[error("gov5 reward to {address} of {amount} wei exceeds the withdrawal amount range")]
    TooLarge { address: Address, amount: U256 },
}

const GWEI: u64 = 1_000_000_000;

/// The withdrawals-root gov5 writes for a block's rewards.
pub fn gov5_native_rewards_root(rewards: &[Gov5Reward]) -> B256 {
    let mut encoded = Vec::with_capacity(rewards.len() * 56);
    for reward in rewards {
        let payload_length = reward.address.length() + reward.amount.length();
        RlpHeader {
            list: true,
            payload_length,
        }
        .encode(&mut encoded);
        reward.address.encode(&mut encoded);
        reward.amount.encode(&mut encoded);
    }
    keccak256(encoded)
}

/// Rewards as the withdrawals reth's executor credits after the transactions.
/// The index is the reward's position; gov5 has no validator index.
pub fn gov5_rewards_to_withdrawals(
    rewards: &[Gov5Reward],
) -> Result<Vec<Withdrawal>, Gov5RewardError> {
    rewards
        .iter()
        .enumerate()
        .map(|(index, reward)| {
            let gwei = U256::from(GWEI);
            if reward.amount % gwei != U256::ZERO {
                return Err(Gov5RewardError::NotGweiAligned {
                    address: reward.address,
                    amount: reward.amount,
                });
            }
            let amount =
                u64::try_from(reward.amount / gwei).map_err(|_| Gov5RewardError::TooLarge {
                    address: reward.address,
                    amount: reward.amount,
                })?;
            Ok(Withdrawal {
                index: index as u64,
                validator_index: 0,
                address: reward.address,
                amount,
            })
        })
        .collect()
}

/// The inverse of [`gov5_rewards_to_withdrawals`].
pub fn gov5_withdrawals_to_rewards(withdrawals: &[Withdrawal]) -> Vec<Gov5Reward> {
    withdrawals
        .iter()
        .map(|withdrawal| Gov5Reward {
            address: withdrawal.address,
            amount: U256::from(withdrawal.amount) * U256::from(GWEI),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const ONE_ETH: u128 = 1_000_000_000_000_000_000;

    /// Chain 94 block 13,540,000: coinbase and faucet each receive 1 ETH.
    fn chain94_rewards() -> Vec<Gov5Reward> {
        vec![
            Gov5Reward {
                address: address!("301631ce4b15f1a0d62a35d6c421dd5a845e555e"),
                amount: U256::from(ONE_ETH),
            },
            Gov5Reward {
                address: address!("42e9819036f61bf665d5f727e8c03121f12f586e"),
                amount: U256::from(ONE_ETH),
            },
        ]
    }

    #[test]
    fn matches_the_live_chain94_withdrawals_root() {
        assert_eq!(
            gov5_native_rewards_root(&chain94_rewards()),
            "0x29c0690c4c8ecb051f4e54d1f2c59b491aa71147e7011bf1531d686dcc5cb53b"
                .parse::<B256>()
                .unwrap()
        );
    }

    #[test]
    fn empty_reward_list_is_keccak_of_nothing() {
        assert_eq!(gov5_native_rewards_root(&[]), keccak256([]));
    }

    #[test]
    fn rewards_round_trip_through_withdrawals() {
        let rewards = chain94_rewards();
        let withdrawals = gov5_rewards_to_withdrawals(&rewards).unwrap();
        assert_eq!(withdrawals.len(), 2);
        assert_eq!(withdrawals[0].index, 0);
        assert_eq!(withdrawals[1].index, 1);
        assert_eq!(withdrawals[0].amount, 1_000_000_000);
        assert_eq!(withdrawals[1].address, rewards[1].address);
        assert_eq!(gov5_withdrawals_to_rewards(&withdrawals), rewards);
    }

    #[test]
    fn sub_gwei_rewards_are_rejected() {
        let reward = Gov5Reward {
            address: Address::ZERO,
            amount: U256::from(1),
        };
        assert!(matches!(
            gov5_rewards_to_withdrawals(std::slice::from_ref(&reward)),
            Err(Gov5RewardError::NotGweiAligned { .. })
        ));
    }
}
