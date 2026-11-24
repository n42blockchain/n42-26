//! E12: the deferred-execution safety halt of a Profile-1 node.
//!
//! A node halts (stops importing, voting and producing) when it holds proof
//! that the committed chain and its own execution disagree:
//!
//! - **carried fields**: a committed block (consensus committed it here, or
//!   it carries a CommitQC) whose carried execution fields differ from this
//!   node's result for its parent (A10). Either this node's execution or a
//!   quorum's differs from the standard; continuing would build on a state
//!   this node cannot vouch for.
//! - **conflicting commit**: two committed blocks at one height with
//!   different hashes, or a committed block that does not extend this node's
//!   committed chain (SYNC §3.3 "conflict" and "equivocation").
//!
//! The evidence is written durably to `profile1_halt.json` in the consensus
//! data directory before anything else happens, and the gauge
//! `n42_profile1_halted` is set to 1. A node that starts with that file
//! present starts halted. There is no automatic resume.
//!
//! **Operator resume.** Investigate with the evidence file (the block, its
//! parent, the field that differs, both values, the commit QC). Once the
//! cause is fixed (a corrected binary, or a resynced datadir), stop the node,
//! move `profile1_halt.json` out of the consensus data directory (keep it for
//! the record) and start the node again.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// The file name of the halt evidence in the consensus data directory.
pub const HALT_FILE: &str = "profile1_halt.json";

/// What a halt is based on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HaltReason {
    /// A committed block carries fields that are not this node's result for
    /// its parent.
    CarriedFields {
        /// The committed block.
        block: B256,
        /// Its parent.
        parent: B256,
        /// The first field that differs.
        field: String,
        /// The carried value of every deferred field.
        carried: serde_json::Value,
        /// This node's result for the parent.
        own: serde_json::Value,
    },
    /// Two committed blocks at one height, or a committed block off this
    /// node's committed chain.
    ConflictingCommit {
        /// Block height.
        number: u64,
        /// The block this node committed first.
        committed: B256,
        /// The conflicting block.
        conflicting: B256,
    },
}

/// The evidence of a halt, as written to [`HALT_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaltEvidence {
    /// What was proven.
    pub reason: HaltReason,
    /// The consensus view of the committed block.
    pub view: u64,
    /// Its commit QC, when there is one.
    pub commit_qc: Option<serde_json::Value>,
    /// Wall-clock time of the halt (ms since the Unix epoch).
    pub at_unix_ms: u64,
}

/// The halt latch: once tripped it stays tripped for the process, and on
/// disk until an operator removes the evidence.
#[derive(Debug)]
pub struct Halt {
    path: Option<PathBuf>,
    evidence: Mutex<Option<HaltEvidence>>,
}

impl Halt {
    /// A latch whose evidence goes to `dir/`[`HALT_FILE`]; halted at once if
    /// the file is there. An unreadable file also halts: evidence is never
    /// discarded because it cannot be parsed.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join(HALT_FILE);
        let evidence = match std::fs::read(&path) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                HaltEvidence {
                    reason: HaltReason::ConflictingCommit {
                        number: 0,
                        committed: B256::ZERO,
                        conflicting: B256::ZERO,
                    },
                    view: 0,
                    commit_qc: Some(serde_json::Value::String(format!(
                        "unreadable halt evidence ({error}); see {}",
                        path.display()
                    ))),
                    at_unix_ms: 0,
                }
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let halt = Self {
            path: Some(path),
            evidence: Mutex::new(evidence),
        };
        halt.publish();
        Ok(halt)
    }

    /// A latch that keeps its evidence in memory only (tests).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            evidence: Mutex::new(None),
        }
    }

    /// The evidence, if halted.
    pub fn evidence(&self) -> Option<HaltEvidence> {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether this node is halted.
    pub fn is_halted(&self) -> bool {
        self.evidence().is_some()
    }

    /// Halts with `evidence`; the first evidence is kept. The file is written
    /// and synced before this returns; a write failure is logged and the
    /// node halts anyway.
    pub fn trip(&self, evidence: HaltEvidence) {
        let mut held = self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.is_some() {
            return;
        }
        tracing::error!(
            target: "n42::profile1",
            evidence = ?evidence,
            file = ?self.path,
            "HALTED (E12): import, voting and producing are stopped until an operator removes the halt evidence"
        );
        if let Some(path) = &self.path
            && let Err(error) = write_synced(path, &evidence)
        {
            tracing::error!(target: "n42::profile1", %error, file = %path.display(), "could not write the halt evidence; halting anyway");
        }
        *held = Some(evidence);
        drop(held);
        self.publish();
    }

    fn publish(&self) {
        metrics::gauge!("n42_profile1_halted").set(if self.is_halted() { 1.0 } else { 0.0 });
    }
}

fn write_synced(path: &Path, evidence: &HaltEvidence) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(evidence).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Committed blocks by height, to recognise a conflicting commit.
#[derive(Debug, Default)]
pub struct CommittedHeights {
    by_number: std::collections::BTreeMap<u64, B256>,
}

/// How many committed heights are remembered.
const HEIGHTS_KEPT: usize = 4096;

impl CommittedHeights {
    /// Records `hash` committed at `number` with parent `parent`. Returns the
    /// conflicting block this node committed before: another hash at the
    /// same height, or a parent height whose committed block is not
    /// `parent`.
    pub fn commit(&mut self, number: u64, hash: B256, parent: B256) -> Option<(u64, B256)> {
        if let Some(known) = self.by_number.get(&number)
            && *known != hash
        {
            return Some((number, *known));
        }
        if let Some(parent_number) = number.checked_sub(1)
            && let Some(known) = self.by_number.get(&parent_number)
            && *known != parent
        {
            return Some((parent_number, *known));
        }
        self.by_number.insert(number, hash);
        while self.by_number.len() > HEIGHTS_KEPT {
            self.by_number.pop_first();
        }
        None
    }
}

/// The E12 verdict on a committed block: `Some` reason to halt, else `None`.
///
/// `carried` is what the block carries, `own_parent` this node's result for
/// its parent (`None` if this node has not executed the parent yet: the
/// import's own check decides then), `refused` the mismatch the import
/// recorded for the block, if any.
pub fn committed_block_verdict(
    block: B256,
    parent: B256,
    carried: Option<n42_consensus::ExecutedFields>,
    own_parent: Option<n42_consensus::ExecutedFields>,
    refused: Option<n42_consensus::CarriedMismatch>,
) -> Option<HaltReason> {
    let to_json = |fields: &n42_consensus::ExecutedFields| {
        serde_json::json!({
            "stateRoot": fields.state_root,
            "receiptsRoot": fields.receipts_root,
            "logsBloom": fields.logs_bloom,
            "gasUsed": fields.gas_used,
            "requestsHash": fields.requests_hash,
        })
    };
    if let Some(refused) = refused {
        return Some(HaltReason::CarriedFields {
            block,
            parent: refused.parent,
            field: refused.field.to_owned(),
            carried: to_json(&refused.carried),
            own: to_json(&refused.expected),
        });
    }
    let (carried, own) = (carried?, own_parent?);
    let field = carried.first_difference(&own)?;
    Some(HaltReason::CarriedFields {
        block,
        parent,
        field: field.to_owned(),
        carried: to_json(&carried),
        own: to_json(&own),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use n42_consensus::ExecutedFields;

    fn fields(byte: u8) -> ExecutedFields {
        ExecutedFields {
            state_root: B256::repeat_byte(byte),
            receipts_root: B256::repeat_byte(byte ^ 0x10),
            logs_bloom: Default::default(),
            gas_used: u64::from(byte),
            requests_hash: B256::repeat_byte(0x55),
        }
    }

    fn evidence(byte: u8) -> HaltEvidence {
        HaltEvidence {
            reason: HaltReason::ConflictingCommit {
                number: 7,
                committed: B256::repeat_byte(byte),
                conflicting: B256::repeat_byte(byte + 1),
            },
            view: 9,
            commit_qc: None,
            at_unix_ms: 1,
        }
    }

    #[test]
    fn a_trip_is_durable_and_only_an_operator_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let halt = Halt::open(dir.path()).unwrap();
        assert!(!halt.is_halted());
        halt.trip(evidence(1));
        // The first evidence is kept.
        halt.trip(evidence(5));
        assert_eq!(halt.evidence(), Some(evidence(1)));
        // A restart starts halted with the same evidence.
        let reopened = Halt::open(dir.path()).unwrap();
        assert_eq!(reopened.evidence(), Some(evidence(1)));
        // The operator moves the file away: the next start is not halted.
        std::fs::rename(dir.path().join(HALT_FILE), dir.path().join("kept.json")).unwrap();
        assert!(!Halt::open(dir.path()).unwrap().is_halted());
    }

    #[test]
    fn unreadable_evidence_still_halts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(HALT_FILE), b"{not json").unwrap();
        assert!(Halt::open(dir.path()).unwrap().is_halted());
    }

    #[test]
    fn committed_block_verdicts() {
        let (block, parent) = (B256::repeat_byte(0xb), B256::repeat_byte(0xa));
        // Carried = own result: no halt.
        assert_eq!(committed_block_verdict(block, parent, Some(fields(1)), Some(fields(1)), None), None);
        // Parent not executed here yet: the import decides.
        assert_eq!(committed_block_verdict(block, parent, Some(fields(1)), None, None), None);
        // Any field differs: halt with both values.
        let mut wrong = fields(1);
        wrong.gas_used += 1;
        match committed_block_verdict(block, parent, Some(wrong), Some(fields(1)), None) {
            Some(HaltReason::CarriedFields { field, .. }) => assert_eq!(field, "gasUsed"),
            other => panic!("{other:?}"),
        }
        // The import refused it: halt with the import's evidence.
        let refused = n42_consensus::CarriedMismatch {
            parent,
            field: "stateRoot",
            carried: fields(2),
            expected: fields(1),
        };
        match committed_block_verdict(block, parent, None, None, Some(refused)) {
            Some(HaltReason::CarriedFields { field, parent: p, .. }) => {
                assert_eq!((field.as_str(), p), ("stateRoot", parent))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn conflicting_commits() {
        let h = |b: u8| B256::repeat_byte(b);
        let mut heights = CommittedHeights::default();
        assert_eq!(heights.commit(1, h(1), h(0)), None);
        assert_eq!(heights.commit(2, h(2), h(1)), None);
        // The same commit again is not a conflict.
        assert_eq!(heights.commit(2, h(2), h(1)), None);
        // Another block at height 2.
        assert_eq!(heights.commit(2, h(9), h(1)), Some((2, h(2))));
        // A block at 3 whose parent is not the committed 2.
        assert_eq!(heights.commit(3, h(3), h(8)), Some((2, h(2))));
        assert_eq!(heights.commit(3, h(3), h(2)), None);
    }
}
