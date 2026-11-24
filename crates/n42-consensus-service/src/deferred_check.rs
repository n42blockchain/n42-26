//! Includability under deferred execution (Profile 1, A13): every
//! transaction of a proposed block must be includable against the parent's
//! post-state, the rule that keeps execution from ever failing a committed
//! block whose header only carries the parent's result.
//!
//! A port of gov5 `internal/deferred_includable.go` (wt-p1vote 0ce45012):
//! [`deferred_tx_plan`] is `deferredTxPlan` (the stateless rules: block gas,
//! no blob transactions, fee cap against the tip and the base fee, intrinsic
//! gas under the block's fork rules, grouping by sender in first-appearance
//! order with the value earlier transactions of the block pay each sender),
//! and [`check_sender_states`] is `checkSenderStates` (EIP-3607, contiguous
//! nonces from the state, worst-case cost `gas * feeCap + value` within the
//! balance plus the block's earlier credits). [`intrinsic_gas`] is gov5's
//! `IntrinsicGas`, including the EIP-2780 decomposition under Amsterdam.
//! Error texts follow gov5's so refusals read the same on both clients.
//!
//! gov5 skips the check for a block it has already applied (its post-state
//! would read the block's own nonces as used); callers do the same.

use alloy_primitives::{Address, B256, U256};

/// gov5 `params` constants used by [`intrinsic_gas`].
mod gas {
    pub const TX: u64 = 21_000;
    pub const TX_CONTRACT_CREATION: u64 = 53_000;
    pub const TX_DATA_ZERO: u64 = 4;
    pub const TX_DATA_NON_ZERO_FRONTIER: u64 = 68;
    pub const TX_DATA_NON_ZERO_EIP2028: u64 = 16;
    pub const INIT_CODE_WORD: u64 = 2;
    pub const TX_ACCESS_LIST_ADDRESS: u64 = 2_400;
    pub const TX_ACCESS_LIST_STORAGE_KEY: u64 = 1_900;
    pub const PER_EMPTY_ACCOUNT: u64 = 25_000;
    pub const TX_BASE_EIP2780: u64 = 12_000;
    pub const TX_COLD_ACCOUNT_EIP2780: u64 = 3_000;
    pub const TX_VALUE_EIP2780: u64 = 6_000;
    pub const TX_AUTH_BASE_EIP2780: u64 = 7_816;
    pub const CREATE_ACCESS_EIP8038: u64 = 12_000;
    pub const TX_ACCESS_LIST_ADDRESS_EIP8038: u64 = 2_900;
    pub const TX_ACCESS_LIST_STORAGE_KEY_EIP8038: u64 = 2_000;
    pub const ACCESS_LIST_BYTES_SURCHARGE_EIP7981: u64 = 64;
    pub const ACCESS_LIST_ADDRESS_BYTES: u64 = 20;
    pub const ACCESS_LIST_STORAGE_KEY_BYTES: u64 = 32;
}

/// The fork rules intrinsic gas depends on, at the block's number and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IntrinsicRules {
    /// Homestead (contract creation price).
    pub homestead: bool,
    /// Istanbul (EIP-2028 calldata price).
    pub istanbul: bool,
    /// Shanghai (EIP-3860 init-code words).
    pub shanghai: bool,
    /// Prague (EIP-7702 authorizations).
    pub prague: bool,
    /// Amsterdam's execution set (EIP-2780, EIP-8038, EIP-7981).
    pub glamsterdam: bool,
}

impl IntrinsicRules {
    /// Every rule on: a Profile-1 chain from genesis.
    pub const ALL: Self = Self {
        homestead: true,
        istanbul: true,
        shanghai: true,
        prague: true,
        glamsterdam: true,
    };
}

/// What [`intrinsic_gas`] reads of a transaction.
#[derive(Debug, Clone, Copy)]
pub struct IntrinsicInput<'a> {
    /// Calldata or init code.
    pub data: &'a [u8],
    /// `(addresses, storage keys)` in the access list, `None` without one.
    pub access_list: Option<(u64, u64)>,
    /// Number of EIP-7702 authorizations.
    pub authorizations: u64,
    /// The transaction creates a contract.
    pub create: bool,
    /// It transfers a non-zero value.
    pub has_value: bool,
    /// Its recipient is its sender.
    pub self_transfer: bool,
}

/// gov5 `ErrGasUintOverflow`.
pub const GAS_UINT_OVERFLOW: &str = "gas uint64 overflow";

/// gov5 `IntrinsicGas`.
pub fn intrinsic_gas(tx: IntrinsicInput<'_>, rules: IntrinsicRules) -> Result<u64, &'static str> {
    let mut gas = if rules.glamsterdam {
        let mut gas = gas::TX_BASE_EIP2780;
        if tx.create {
            gas += gas::CREATE_ACCESS_EIP8038;
        } else if !tx.self_transfer {
            gas += gas::TX_COLD_ACCOUNT_EIP2780;
            if tx.has_value {
                gas += gas::TX_VALUE_EIP2780;
            }
        }
        gas
    } else if tx.create && rules.homestead {
        gas::TX_CONTRACT_CREATION
    } else {
        gas::TX
    };
    let data_len = tx.data.len() as u64;
    if data_len > 0 {
        let non_zero = tx.data.iter().filter(|byte| **byte != 0).count() as u64;
        let non_zero_gas = if rules.istanbul || rules.glamsterdam {
            gas::TX_DATA_NON_ZERO_EIP2028
        } else {
            gas::TX_DATA_NON_ZERO_FRONTIER
        };
        if (u64::MAX - gas) / non_zero_gas < non_zero {
            return Err(GAS_UINT_OVERFLOW);
        }
        gas += non_zero * non_zero_gas;
        let zero = data_len - non_zero;
        if (u64::MAX - gas) / gas::TX_DATA_ZERO < zero {
            return Err(GAS_UINT_OVERFLOW);
        }
        gas += zero * gas::TX_DATA_ZERO;
        if tx.create && rules.shanghai {
            let words = if data_len > u64::MAX - 31 {
                u64::MAX / 32 + 1
            } else {
                data_len.div_ceil(32)
            };
            if (u64::MAX - gas) / gas::INIT_CODE_WORD < words {
                return Err(GAS_UINT_OVERFLOW);
            }
            gas += words * gas::INIT_CODE_WORD;
        }
    }
    if let Some((addresses, keys)) = tx.access_list {
        let (address_gas, key_gas) = if rules.glamsterdam {
            (
                gas::TX_ACCESS_LIST_ADDRESS_EIP8038
                    + gas::ACCESS_LIST_ADDRESS_BYTES * gas::ACCESS_LIST_BYTES_SURCHARGE_EIP7981,
                gas::TX_ACCESS_LIST_STORAGE_KEY_EIP8038
                    + gas::ACCESS_LIST_STORAGE_KEY_BYTES * gas::ACCESS_LIST_BYTES_SURCHARGE_EIP7981,
            )
        } else {
            (gas::TX_ACCESS_LIST_ADDRESS, gas::TX_ACCESS_LIST_STORAGE_KEY)
        };
        // gov5 adds these without an overflow check; wrapping keeps its
        // arithmetic (an access list that large cannot be in a block).
        gas = gas.wrapping_add(addresses.wrapping_mul(address_gas));
        gas = gas.wrapping_add(keys.wrapping_mul(key_gas));
    }
    if rules.prague && tx.authorizations > 0 {
        let per_auth = if rules.glamsterdam {
            gas::TX_AUTH_BASE_EIP2780
        } else {
            gas::PER_EMPTY_ACCOUNT
        };
        if (u64::MAX - gas) / per_auth < tx.authorizations {
            return Err(GAS_UINT_OVERFLOW);
        }
        gas += tx.authorizations * per_auth;
    }
    Ok(gas)
}

/// One transaction as the plan reads it.
#[derive(Debug, Clone)]
pub struct PlanTx {
    /// Its sender, recovered by the caller, or why it could not be.
    pub sender: Result<Address, String>,
    /// Nonce.
    pub nonce: u64,
    /// Gas limit.
    pub gas_limit: u64,
    /// Fee cap (the gas price of a legacy or EIP-2930 transaction).
    pub max_fee_per_gas: u128,
    /// Tip cap; `None` reads as the fee cap.
    pub max_priority_fee_per_gas: Option<u128>,
    /// Recipient; `None` creates a contract.
    pub to: Option<Address>,
    /// Value.
    pub value: U256,
    /// Number of blob hashes.
    pub blob_hashes: usize,
    /// Calldata or init code.
    pub input: alloy_primitives::Bytes,
    /// `(addresses, storage keys)` of the access list.
    pub access_list: Option<(u64, u64)>,
    /// Number of EIP-7702 authorizations.
    pub authorizations: u64,
}

/// What the plan reads of the header.
#[derive(Debug, Clone, Copy)]
pub struct PlanHeader {
    /// Block number.
    pub number: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Base fee.
    pub base_fee: Option<u64>,
}

/// One sender's transactions in block order (gov5 `deferredSender`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderPlan {
    /// Sender.
    pub address: Address,
    /// `(nonce, gas limit, fee cap, value)` of each transaction.
    pub txs: Vec<(u64, u64, u128, U256)>,
    /// What the block pays the sender before its first transaction.
    pub credit: U256,
}

/// A refusal; the text says which rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refusal(pub String);

/// gov5 `deferredTxPlan`.
pub fn deferred_tx_plan(
    header: PlanHeader,
    rules: IntrinsicRules,
    txs: &[PlanTx],
) -> Result<Vec<SenderPlan>, Refusal> {
    let number = header.number;
    let refuse = |message: String| Err(Refusal(message));
    if txs.is_empty() {
        return Ok(Vec::new());
    }
    // gov5 recovers all senders first and reports the lowest failing index.
    if let Some((index, error)) = txs
        .iter()
        .enumerate()
        .find_map(|(index, tx)| tx.sender.as_ref().err().map(|error| (index, error)))
    {
        return refuse(format!(
            "deferred execution: block {number} tx {index} sender: {error}"
        ));
    }
    let mut block_gas: u64 = 0;
    let mut by_sender: std::collections::HashMap<Address, usize> = Default::default();
    let mut incoming: std::collections::HashMap<Address, U256> = Default::default();
    let mut plan: Vec<SenderPlan> = Vec::new();
    for (index, tx) in txs.iter().enumerate() {
        let from = *tx.sender.as_ref().expect("checked above");
        if tx.gas_limit > header.gas_limit.wrapping_sub(block_gas) {
            return refuse(format!(
                "deferred execution: block {number} exceeds its gas limit at tx {index}"
            ));
        }
        block_gas += tx.gas_limit;
        if tx.blob_hashes > 0 {
            return refuse(format!(
                "deferred execution: block {number} tx {index}: blob transactions are not includable under deferred execution"
            ));
        }
        let fee_cap = tx.max_fee_per_gas;
        let tip = tx.max_priority_fee_per_gas.unwrap_or(fee_cap);
        if fee_cap < tip {
            return refuse(format!(
                "deferred execution: block {number} tx {index}: max priority fee per gas higher than max fee per gas: address {from}, tip: {tip}, gasFeeCap: {fee_cap}"
            ));
        }
        if let Some(base_fee) = header.base_fee
            && fee_cap < u128::from(base_fee)
        {
            return refuse(format!(
                "deferred execution: block {number} tx {index}: max fee per gas less than block base fee: address {from}, gasFeeCap: {fee_cap} baseFee: {base_fee}"
            ));
        }
        let create = tx.to.is_none();
        let has_value = !tx.value.is_zero();
        let self_transfer = tx.to == Some(from);
        let intrinsic = intrinsic_gas(
            IntrinsicInput {
                data: &tx.input,
                access_list: tx.access_list,
                authorizations: tx.authorizations,
                create,
                has_value,
                self_transfer,
            },
            rules,
        )
        .map_err(|error| Refusal(format!("deferred execution: block {number} tx {index}: {error}")))?;
        if intrinsic > tx.gas_limit {
            return refuse(format!(
                "deferred execution: block {number} tx {index}: intrinsic gas {intrinsic} exceeds gas limit {}",
                tx.gas_limit
            ));
        }
        let at = *by_sender.entry(from).or_insert_with(|| {
            plan.push(SenderPlan {
                address: from,
                txs: Vec::new(),
                credit: incoming.get(&from).copied().unwrap_or_default(),
            });
            plan.len() - 1
        });
        plan[at]
            .txs
            .push((tx.nonce, tx.gas_limit, tx.max_fee_per_gas, tx.value));
        // What this transaction pays its recipient, for senders that appear
        // later in the block. A self-transfer changes nothing, a creation
        // has no recipient.
        if let Some(to) = tx.to
            && has_value
            && !self_transfer
        {
            let credit = incoming.entry(to).or_default();
            // Unreachable with real balances; no credit rather than a claim.
            *credit = credit.checked_add(tx.value).unwrap_or_default();
        }
    }
    Ok(plan)
}

/// What the check reads of a sender in the parent's post-state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SenderState {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code hash (`None` or the empty code hash: no code).
    pub code_hash: Option<B256>,
}

/// keccak256 of empty code.
const EMPTY_CODE_HASH: B256 =
    alloy_primitives::b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");

/// gov5 `checkSenderStates`: `read` gives a sender's state in the parent's
/// post-state (`None` for an account that does not exist). A read error is
/// returned as `Err(Ok(error))`, a refusal as `Err(Err(refusal))`.
pub fn check_sender_states<E>(
    number: u64,
    plan: &[SenderPlan],
    mut read: impl FnMut(&Address) -> Result<Option<SenderState>, E>,
) -> Result<(), Result<E, Refusal>> {
    let refuse = |message: String| Err(Err(Refusal(message)));
    for sender in plan {
        let from = sender.address;
        let short = alloy_primitives::hex::encode(&from.as_slice()[..4]);
        let account = read(&from).map_err(Ok)?;
        let (mut nonce, mut balance) = (0u64, U256::ZERO);
        if let Some(account) = account {
            nonce = account.nonce;
            balance = account.balance;
            // EIP-3607: a sender with code cannot originate.
            if let Some(code_hash) = account.code_hash
                && code_hash != B256::ZERO
                && code_hash != EMPTY_CODE_HASH
            {
                return refuse(format!(
                    "deferred execution: block {number} sender {short} has code"
                ));
            }
        }
        let mut cost = U256::ZERO;
        for &(tx_nonce, gas_limit, fee_cap, value) in &sender.txs {
            if tx_nonce != nonce {
                return refuse(format!(
                    "deferred execution: block {number} sender {short} nonce {tx_nonce}, state expects {nonce}"
                ));
            }
            if nonce == u64::MAX {
                return refuse(format!(
                    "deferred execution: block {number} sender {short} nonce at its maximum"
                ));
            }
            nonce += 1;
            let Some(gas_cost) = U256::from(gas_limit).checked_mul(U256::from(fee_cap)) else {
                return refuse(format!(
                    "deferred execution: block {number} sender {short} gas cost overflows"
                ));
            };
            let Some(next) = cost.checked_add(gas_cost).and_then(|cost| cost.checked_add(value))
            else {
                return refuse(format!(
                    "deferred execution: block {number} sender {short} cost overflows"
                ));
            };
            cost = next;
        }
        if !sender.credit.is_zero() {
            balance = balance.checked_add(sender.credit).unwrap_or(U256::MAX);
        }
        if cost > balance {
            return refuse(format!(
                "deferred execution: block {number} sender {short} worst-case cost {cost} exceeds balance {balance}"
            ));
        }
    }
    Ok(())
}

/// The builder's includability filter (design 3.3): the same rules as
/// [`deferred_tx_plan`] + [`check_sender_states`], applied one transaction
/// at a time to the block being built, so that every block this node
/// proposes passes the check on gov5 and on n42-26 followers. A transaction
/// is [`IncludabilityFilter::admit`]ted (checked, nothing changed) and then,
/// once the builder has executed it, [`IncludabilityFilter::commit`]ted.
#[derive(Debug)]
pub struct IncludabilityFilter {
    header: PlanHeader,
    rules: IntrinsicRules,
    block_gas: u64,
    senders: std::collections::HashMap<Address, SenderTrack>,
    incoming: std::collections::HashMap<Address, U256>,
}

#[derive(Debug, Clone, Copy)]
struct SenderTrack {
    next_nonce: u64,
    /// Balance in the parent's post-state plus the block's earlier credits
    /// to the sender at its first transaction (saturating).
    funds: U256,
    cost: U256,
}

/// A transaction that passed [`IncludabilityFilter::admit`].
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct Admitted {
    sender: Address,
    gas_limit: u64,
    track: SenderTrack,
    credit_to: Option<(Address, U256)>,
}

impl IncludabilityFilter {
    /// A filter for the block `header` under `rules`.
    pub fn new(header: PlanHeader, rules: IntrinsicRules) -> Self {
        Self {
            header,
            rules,
            block_gas: 0,
            senders: Default::default(),
            incoming: Default::default(),
        }
    }

    /// Whether `tx` may follow the transactions committed so far. `read`
    /// gives a sender's state in the parent's post-state; it is called for a
    /// sender's first transaction only.
    pub fn admit<E>(
        &self,
        tx: &PlanTx,
        read: impl FnOnce(&Address) -> Result<Option<SenderState>, E>,
    ) -> Result<Admitted, Result<E, Refusal>> {
        let number = self.header.number;
        let index = "next";
        let refuse = |message: String| Err(Err(Refusal(message)));
        let from = match &tx.sender {
            Ok(from) => *from,
            Err(error) => {
                return refuse(format!("deferred execution: block {number} tx {index} sender: {error}"));
            }
        };
        if tx.gas_limit > self.header.gas_limit.saturating_sub(self.block_gas) {
            return refuse(format!(
                "deferred execution: block {number} exceeds its gas limit at tx {index}"
            ));
        }
        // The stateless rules, exactly as the plan applies them.
        let single = deferred_tx_plan(
            PlanHeader {
                gas_limit: tx.gas_limit,
                ..self.header
            },
            self.rules,
            std::slice::from_ref(tx),
        )
        .map_err(Err)?;
        debug_assert_eq!(single.len(), 1);
        let mut track = match self.senders.get(&from) {
            Some(track) => *track,
            None => {
                let mut track = SenderTrack {
                    next_nonce: 0,
                    funds: U256::ZERO,
                    cost: U256::ZERO,
                };
                if let Some(account) = read(&from).map_err(Ok)? {
                    if let Some(code_hash) = account.code_hash
                        && code_hash != B256::ZERO
                        && code_hash != EMPTY_CODE_HASH
                    {
                        let short = alloy_primitives::hex::encode(&from.as_slice()[..4]);
                        return refuse(format!(
                            "deferred execution: block {number} sender {short} has code"
                        ));
                    }
                    track.next_nonce = account.nonce;
                    track.funds = account.balance;
                }
                if let Some(credit) = self.incoming.get(&from) {
                    track.funds = track.funds.checked_add(*credit).unwrap_or(U256::MAX);
                }
                track
            }
        };
        let plan = SenderPlan {
            address: from,
            txs: vec![(tx.nonce, tx.gas_limit, tx.max_fee_per_gas, tx.value)],
            credit: U256::ZERO,
        };
        // The sender's earlier transactions are accounted in `track`: check
        // this one against what is left.
        let state = SenderState {
            nonce: track.next_nonce,
            balance: track.funds.saturating_sub(track.cost),
            code_hash: None,
        };
        if track.cost > track.funds {
            return refuse(format!(
                "deferred execution: block {number} sender worst-case cost exceeds balance"
            ));
        }
        check_sender_states::<std::convert::Infallible>(number, &[plan], |_| Ok(Some(state)))
            .map_err(|error| match error {
                Ok(never) => match never {},
                Err(refusal) => Err(refusal),
            })?;
        track.next_nonce += 1;
        track.cost += U256::from(tx.gas_limit) * U256::from(tx.max_fee_per_gas) + tx.value;
        let credit_to = match tx.to {
            Some(to) if !tx.value.is_zero() && to != from => Some((to, tx.value)),
            _ => None,
        };
        Ok(Admitted {
            sender: from,
            gas_limit: tx.gas_limit,
            track,
            credit_to,
        })
    }

    /// Records an admitted transaction the builder included.
    pub fn commit(&mut self, admitted: Admitted) {
        self.block_gas += admitted.gas_limit;
        self.senders.insert(admitted.sender, admitted.track);
        if let Some((to, value)) = admitted.credit_to {
            let credit = self.incoming.entry(to).or_default();
            *credit = credit.checked_add(value).unwrap_or_default();
        }
    }
}

/// The outcome of a deferred proposal check (design 3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredCheck {
    /// The carried fields are this node's result for the parent and every
    /// transaction is includable: the block may get the Round-1 vote before
    /// its import completes.
    Checked,
    /// Not decidable yet (the parent's result or post-state is not here):
    /// the vote waits for the block's import instead.
    Retry(String),
    /// The block is refused; the Round-1 vote is withheld.
    Refused(String),
    /// This execution layer cannot check (not a Profile-1 node).
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    /// A 21000-gas EIP-1559 transfer (gov5 `signTo`: tip 1, fee cap 1 gwei).
    fn transfer(from: Address, nonce: u64, to: Address, value: u64) -> PlanTx {
        PlanTx {
            sender: Ok(from),
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: Some(1),
            to: Some(to),
            value: U256::from(value),
            blob_hashes: 0,
            input: Default::default(),
            access_list: None,
            authorizations: 0,
        }
    }

    fn header(gas_limit: u64, base_fee: u64) -> PlanHeader {
        PlanHeader {
            number: 10,
            gas_limit,
            base_fee: Some(base_fee),
        }
    }

    /// gov5 `buildSignedTxs`: one transfer from each of `n` senders.
    fn many(n: usize) -> Vec<PlanTx> {
        (0..n)
            .map(|i| {
                let mut from = [0u8; 20];
                from[..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
                transfer(Address::from(from), 0, address(0xee), 1)
            })
            .collect()
    }

    /// gov5 `TestDeferredTxPlanRecoversWithoutFrom`: one entry per sender,
    /// in block order.
    #[test]
    fn plan_groups_senders_in_block_order() {
        let txs = many(3000);
        let plan = deferred_tx_plan(header(21_000 * 4000, 1_000_000_000), IntrinsicRules::ALL, &txs)
            .unwrap();
        assert_eq!(plan.len(), txs.len());
        for (sender, tx) in plan.iter().zip(&txs) {
            assert_eq!(Ok(sender.address), tx.sender);
            assert_eq!(sender.txs.len(), 1);
        }
        // Two transactions of one sender stay together, in order.
        let (a, b) = (address(1), address(2));
        let txs = [transfer(a, 0, b, 1), transfer(b, 0, a, 1), transfer(a, 1, b, 1)];
        let plan = deferred_tx_plan(header(21_000 * 10, 1), IntrinsicRules::ALL, &txs).unwrap();
        assert_eq!(plan.iter().map(|sender| sender.address).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(plan[0].txs.iter().map(|tx| tx.0).collect::<Vec<_>>(), vec![0, 1]);
    }

    /// gov5 `TestDeferredTxPlanStatelessRules` and
    /// `TestAuditDeferredPlanRejectsGasOverflow`, as a table.
    #[test]
    fn plan_stateless_rules() {
        let txs = many(3000);
        let mut bad = txs.clone();
        bad[2800].sender = Err("invalid signature".into());
        bad[100].sender = Err("invalid signature".into());
        let mut overflow = vec![transfer(address(7), 0, address(1), 0), transfer(address(7), 1, address(1), 0)];
        overflow[1].gas_limit = u64::MAX;
        let mut tip_above = vec![transfer(address(7), 0, address(1), 0)];
        tip_above[0].max_priority_fee_per_gas = Some(2_000_000_000);
        let mut blob = vec![transfer(address(7), 0, address(1), 0)];
        blob[0].blob_hashes = 1;
        let mut short_gas = vec![transfer(address(7), 0, address(1), 1)];
        short_gas[0].gas_limit = 20_999;
        let cases: Vec<(&str, PlanHeader, Vec<PlanTx>, &str)> = vec![
            ("fee cap below the base fee", header(21_000 * 4000, 2_000_000_000), txs.clone(), "less than block base fee"),
            ("block gas limit", header(21_000 * 10, 1), txs.clone(), "exceeds its gas limit at tx 10"),
            ("lowest failing sender index", header(21_000 * 4000, 1), bad, "tx 100 sender"),
            ("gas overflow", header(u64::MAX, 1_000_000_000), overflow, "gas limit"),
            ("tip above fee cap", header(21_000, 1), tip_above, "higher than max fee per gas"),
            ("blob transaction", header(21_000, 1), blob, "blob transactions are not includable"),
            ("intrinsic gas", header(21_000, 1), short_gas, "intrinsic gas 21000 exceeds gas limit 20999"),
        ];
        for (name, header, txs, expected) in cases {
            let error = deferred_tx_plan(header, IntrinsicRules::ALL, &txs).unwrap_err();
            assert!(error.0.contains(expected), "{name}: {error}");
        }
    }

    /// gov5 `TestDeferredTxPlanCreditsEarlierPaymentsInTheBlock` and
    /// `TestDeferredTxPlanIgnoresLaterPayments`.
    #[test]
    fn plan_credits_earlier_payments_only() {
        let (payer, spender, sink) = (address(0xa1), address(0xa2), address(0xcc));
        let fund = transfer(payer, 0, spender, 5_000_000);
        let spend = transfer(spender, 0, sink, 1);
        let plan = deferred_tx_plan(header(21_000 * 10, 1_000_000_000), IntrinsicRules::ALL, &[fund.clone(), spend.clone()]).unwrap();
        let credit = plan.iter().find(|sender| sender.address == spender).unwrap().credit;
        assert_eq!(credit, U256::from(5_000_000));
        let plan = deferred_tx_plan(header(21_000 * 10, 1_000_000_000), IntrinsicRules::ALL, &[spend, fund]).unwrap();
        assert!(plan.iter().find(|sender| sender.address == spender).unwrap().credit.is_zero());
        // A self-transfer and a creation credit nobody.
        let mut create = transfer(payer, 1, sink, 9);
        create.to = None;
        create.gas_limit = 100_000;
        let plan = deferred_tx_plan(
            header(1_000_000, 1),
            IntrinsicRules::ALL,
            &[transfer(sink, 0, sink, 5), create, transfer(sink, 1, payer, 1)],
        )
        .unwrap();
        assert!(plan.iter().all(|sender| sender.credit.is_zero()));
    }

    #[test]
    fn sender_states() {
        let (a, b) = (address(0xa1), address(0xb2));
        let worst = U256::from(21_000u64 * 1_000_000_000 + 1);
        let plan = |txs: Vec<PlanTx>| deferred_tx_plan(header(21_000 * 10, 1), IntrinsicRules::ALL, &txs).unwrap();
        let state = |nonce, balance: U256, code: Option<B256>| move |_: &Address| -> Result<Option<SenderState>, ()> {
            Ok(Some(SenderState { nonce, balance, code_hash: code }))
        };
        let refusal = |result: Result<(), Result<(), Refusal>>| match result {
            Err(Err(refusal)) => refusal.0,
            other => panic!("{other:?}"),
        };
        // Includable: nonce from the state, worst case covered.
        assert_eq!(check_sender_states(10, &plan(vec![transfer(a, 3, b, 1)]), state(3, worst, None)), Ok(()));
        assert_eq!(check_sender_states(10, &plan(vec![transfer(a, 3, b, 1)]), state(3, worst, Some(EMPTY_CODE_HASH))), Ok(()));
        // Nonce gap and reuse.
        assert!(refusal(check_sender_states(10, &plan(vec![transfer(a, 4, b, 1)]), state(3, worst, None))).contains("nonce 4, state expects 3"));
        assert!(refusal(check_sender_states(10, &plan(vec![transfer(a, 3, b, 1), transfer(a, 5, b, 1)]), state(3, worst * U256::from(2), None))).contains("nonce 5, state expects 4"));
        // Worst-case cost: gas * fee cap + value, summed over the sender.
        assert!(refusal(check_sender_states(10, &plan(vec![transfer(a, 3, b, 1)]), state(3, worst - U256::from(1), None))).contains("worst-case cost"));
        // EIP-3607.
        assert!(refusal(check_sender_states(10, &plan(vec![transfer(a, 3, b, 1)]), state(3, worst, Some(B256::repeat_byte(1))))).contains("has code"));
        // A sender that does not exist has nonce 0 and nothing to pay with.
        let missing = |_: &Address| -> Result<Option<SenderState>, ()> { Ok(None) };
        assert!(refusal(check_sender_states(10, &plan(vec![transfer(a, 0, b, 1)]), missing)).contains("worst-case cost"));
        // ... unless the block paid it first.
        let txs = plan(vec![transfer(b, 0, a, 1_000_000_000_000_000), transfer(a, 0, b, 1)]);
        let rich_b = |who: &Address| -> Result<Option<SenderState>, ()> {
            Ok((*who == b).then_some(SenderState { nonce: 0, balance: U256::MAX >> 1, code_hash: None }))
        };
        assert_eq!(check_sender_states(10, &txs, rich_b), Ok(()));
        // Nonce at its maximum.
        assert!(refusal(check_sender_states(10, &plan(vec![transfer(a, u64::MAX, b, 1)]), state(u64::MAX, worst, None))).contains("nonce at its maximum"));
        // A read error is not a refusal.
        let failing = |_: &Address| -> Result<Option<SenderState>, &str> { Err("io") };
        assert_eq!(check_sender_states(10, &plan(vec![transfer(a, 0, b, 1)]), failing), Err(Ok("io")));
    }

    /// The builder's filter admits exactly a set the follower's check
    /// passes: a sender's worst case summed over its transactions (where
    /// execution, charging the gas actually used, would take both), credits
    /// from earlier transactions only, the sum of gas limits, nonce order.
    /// A refused transaction leaves no trace: a later one from the same
    /// sender is judged afresh.
    #[test]
    fn builder_filter_matches_the_check() {
        let (a, b, c) = (address(0xa1), address(0xb2), address(0xc3));
        let one = U256::from(21_000u64 * 1_000_000_000 + 1);
        let header = header(21_000 * 5, 1);
        let balances = move |who: &Address| -> Result<Option<SenderState>, ()> {
            Ok(Some(SenderState {
                nonce: 0,
                balance: if *who == a { one + one - U256::from(1) } else if *who == b { one * U256::from(10) } else { U256::ZERO },
                code_hash: None,
            }))
        };
        let one_tx = 21_000u64 * 1_000_000_000 + 1;
        let candidates = [
            transfer(a, 0, b, 1),      // admitted
            transfer(a, 1, b, 1),      // a's worst case for two exceeds its balance
            transfer(c, 0, a, 1),      // c has nothing yet
            transfer(b, 0, c, one_tx), // pays c enough for one transfer
            transfer(c, 0, a, 1),      // admitted: the earlier credit counts
            transfer(b, 2, a, 1),      // nonce gap
            transfer(b, 1, a, 1),      // admitted
            transfer(b, 2, a, 1),      // admitted: block gas now 5 x 21000
            transfer(b, 3, a, 1),      // the sum of gas limits is full
        ];
        let mut filter = IncludabilityFilter::new(header, IntrinsicRules::ALL);
        let mut included = Vec::new();
        let mut verdicts = Vec::new();
        for tx in &candidates {
            match filter.admit(tx, balances) {
                Ok(admitted) => {
                    filter.commit(admitted);
                    included.push(tx.clone());
                    verdicts.push(true);
                }
                Err(Err(_)) => verdicts.push(false),
                Err(Ok(())) => unreachable!(),
            }
        }
        assert_eq!(verdicts, [true, false, false, true, true, false, true, true, false]);
        let plan = deferred_tx_plan(header, IntrinsicRules::ALL, &included).unwrap();
        assert_eq!(check_sender_states(10, &plan, balances), Ok(()), "the built block passes the check");
        // A credit before the sender's first transaction is counted.
        let mut filter = IncludabilityFilter::new(header, IntrinsicRules::ALL);
        let funded = filter.admit(&transfer(b, 0, c, one_tx), balances).unwrap();
        filter.commit(funded);
        assert!(filter.admit(&transfer(c, 0, a, 1), balances).is_ok());
    }

    /// gov5 `IntrinsicGas` under Amsterdam (EIP-2780) and before it.
    #[test]
    fn intrinsic_gas_table() {
        let call = |data: &'static [u8], value: bool, self_transfer: bool| IntrinsicInput {
            data,
            access_list: None,
            authorizations: 0,
            create: false,
            has_value: value,
            self_transfer,
        };
        let create = |data: &'static [u8]| IntrinsicInput { create: true, ..call(data, false, false) };
        let pre = IntrinsicRules { glamsterdam: false, ..IntrinsicRules::ALL };
        let all = IntrinsicRules::ALL;
        let cases: Vec<(&str, IntrinsicInput<'_>, IntrinsicRules, u64)> = vec![
            ("transfer", call(b"", true, false), all, 21_000),
            ("call without value", call(b"", false, false), all, 15_000),
            ("self-transfer", call(b"", true, true), all, 12_000),
            ("create", create(b""), all, 24_000),
            ("create with init code", create(&[1, 0, 2]), all, 24_000 + 16 * 2 + 4 + 2),
            ("data", call(&[0, 0, 7], true, false), all, 21_000 + 8 + 16),
            ("access list", IntrinsicInput { access_list: Some((1, 2)), ..call(b"", true, false) }, all, 21_000 + 2_900 + 20 * 64 + 2 * (2_000 + 32 * 64)),
            ("authorizations", IntrinsicInput { authorizations: 2, ..call(b"", false, false) }, all, 15_000 + 2 * 7_816),
            ("pre-Amsterdam transfer", call(b"", true, false), pre, 21_000),
            ("pre-Amsterdam self-transfer", call(b"", true, true), pre, 21_000),
            ("pre-Amsterdam create", create(&[1]), pre, 53_000 + 16 + 2),
            ("pre-Amsterdam access list", IntrinsicInput { access_list: Some((1, 2)), ..call(b"", true, false) }, pre, 21_000 + 2_400 + 2 * 1_900),
            ("pre-Amsterdam authorizations", IntrinsicInput { authorizations: 2, ..call(b"", false, false) }, pre, 21_000 + 2 * 25_000),
        ];
        for (name, input, rules, expected) in cases {
            assert_eq!(intrinsic_gas(input, rules), Ok(expected), "{name}");
        }
    }
}
