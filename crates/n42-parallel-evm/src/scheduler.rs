//! Block-STM scheduler: coordinates parallel execution and in-order validation.
//!
//! 1. Execute transactions optimistically in parallel (a monotonic execute
//!    cursor + a redo queue for aborted txs).
//! 2. Validate **in transaction order** (a single `val_cursor`): tx `i` is only
//!    validated once every lower tx has been validated, so a passing validation
//!    is against a settled lower-prefix state. This in-order discipline is
//!    load-bearing for correctness — out-of-order validation lets a tx pass
//!    against an intermediate state and never get re-checked, producing wrong
//!    results on dependency chains (hot accounts). See `docs/devlog-67`.
//! 3. A failed validation aborts the tx, re-marks every higher tx REDO, and
//!    rewinds `val_cursor` to that index.
//!
//! Validation itself is value-based ([`crate::execution::validate_read_set`]): it
//! re-reads each recorded read and compares the value, so a lower tx that
//! re-executed with a different value invalidates this tx even though its writer
//! index is unchanged.

use crate::types::TxIdx;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Transaction execution status.
const STATUS_PENDING: u32 = 0;
const STATUS_EXECUTED: u32 = 1;
const STATUS_VALIDATED: u32 = 2;
const STATUS_REDO: u32 = 3;

/// A task for a worker thread.
#[derive(Debug)]
pub enum Task {
    /// Execute transaction at given index.
    Execute(TxIdx),
    /// Validate transaction at given index.
    Validate(TxIdx),
}

/// A queue of tx indices guarded by an atomic length, so the empty-queue hot path
/// (the no-conflict common case) never locks the mutex.
struct WorkQueue {
    q: Mutex<VecDeque<TxIdx>>,
    len: AtomicUsize,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            q: Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
        }
    }

    fn push(&self, tx_idx: TxIdx) {
        self.q.lock().push_back(tx_idx);
        self.len.fetch_add(1, Ordering::AcqRel);
    }

    fn pop(&self) -> Option<TxIdx> {
        if self.len.load(Ordering::Acquire) == 0 {
            return None;
        }
        let v = self.q.lock().pop_front();
        if v.is_some() {
            self.len.fetch_sub(1, Ordering::AcqRel);
        }
        v
    }
}

/// Coordinates execution and validation of transactions across worker threads.
pub struct Scheduler {
    num_txs: usize,
    /// Per-transaction status.
    status: Vec<AtomicU32>,
    /// Next transaction index to execute (monotonically increasing; may overshoot).
    exec_cursor: AtomicUsize,
    /// Next transaction index to validate (in order; rewound on abort).
    val_cursor: AtomicUsize,
    /// Number of transactions currently in the VALIDATED state.
    validated_count: AtomicUsize,
    /// Tx indices marked REDO (aborted). The status array is the source of truth;
    /// entries are confirmed with a REDO->PENDING CAS on pop, so stale/duplicate
    /// entries are skipped.
    redo: WorkQueue,
}

impl Scheduler {
    pub fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            status: (0..num_txs)
                .map(|_| AtomicU32::new(STATUS_PENDING))
                .collect(),
            exec_cursor: AtomicUsize::new(0),
            val_cursor: AtomicUsize::new(0),
            validated_count: AtomicUsize::new(0),
            redo: WorkQueue::new(),
        }
    }

    /// Pop a redo entry that is genuinely still REDO (claims it PENDING).
    fn pop_redo(&self) -> Option<TxIdx> {
        while let Some(cand) = self.redo.pop() {
            if self.status[cand]
                .compare_exchange(
                    STATUS_REDO,
                    STATUS_PENDING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return Some(cand);
            }
        }
        None
    }

    /// Get the next task for a worker, or `None` if nothing is ready right now.
    pub fn next_task(&self) -> Option<Task> {
        // Priority 1: re-execute aborted txs (keeps the cascade converging).
        if let Some(i) = self.pop_redo() {
            return Some(Task::Execute(i));
        }
        // Priority 2: execute a new tx. Wait-free claim via fetch_add (a CAS loop
        // retry-stormed under contention). The cursor may overshoot num_txs.
        let idx = self.exec_cursor.fetch_add(1, Ordering::SeqCst);
        if idx < self.num_txs {
            return Some(Task::Execute(idx));
        }
        // Priority 3: validate the next tx IN ORDER — only once it is EXECUTED
        // (lower txs are already validated, so a pass is against a settled state).
        loop {
            let v = self.val_cursor.load(Ordering::SeqCst);
            if v >= self.num_txs {
                return None;
            }
            if self.status[v].load(Ordering::SeqCst) != STATUS_EXECUTED {
                // Either not executed yet, or already validated/redone — wait.
                return None;
            }
            // `val_cursor` advances when a validation task is claimed, not when
            // it finishes. Without this predecessor gate, another worker can
            // claim tx v+1 while tx v is still validating. The higher tx may
            // then validate against an unsettled hot-account value and escape
            // revalidation under an unlucky abort race. Completion, not merely
            // claim order, is the load-bearing Block-STM invariant.
            if v > 0 && self.status[v - 1].load(Ordering::SeqCst) != STATUS_VALIDATED {
                return None;
            }
            if self
                .val_cursor
                .compare_exchange(v, v + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(Task::Validate(v));
            }
        }
    }

    /// Mark a transaction as executed.
    pub fn finish_execution(&self, tx_idx: TxIdx) {
        self.status[tx_idx].store(STATUS_EXECUTED, Ordering::SeqCst);
    }

    /// Mark a transaction as validated.
    pub fn finish_validation(&self, tx_idx: TxIdx) {
        // A lower-index abort may have re-marked this tx REDO while its validation
        // task was in flight; only count it if it is still EXECUTED.
        if self.status[tx_idx]
            .compare_exchange(
                STATUS_EXECUTED,
                STATUS_VALIDATED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.validated_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Mark a transaction for re-execution (validation failed). Also invalidates
    /// every higher-indexed tx (it may have read this one's stale writes) and
    /// rewinds the validation cursor to this index.
    pub fn abort_and_reschedule(&self, tx_idx: TxIdx) {
        let prev_self = self.status[tx_idx].swap(STATUS_REDO, Ordering::SeqCst);
        self.redo.push(tx_idx);

        // Rewind val_cursor so validation resumes from the aborted tx.
        let mut cur = self.val_cursor.load(Ordering::SeqCst);
        while cur > tx_idx {
            match self
                .val_cursor
                .compare_exchange(cur, tx_idx, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }

        let mut invalidated_validated = usize::from(prev_self == STATUS_VALIDATED);
        for i in (tx_idx + 1)..self.num_txs {
            // CAS the EXECUTED/VALIDATED → REDO transition so the observed `cur` is
            // atomic with the store. A plain load-then-store races with a concurrent
            // finish_validation CAS (EXECUTED → VALIDATED) and could leave
            // validated_count permanently too high.
            loop {
                let cur = self.status[i].load(Ordering::SeqCst);
                if cur != STATUS_EXECUTED && cur != STATUS_VALIDATED {
                    break;
                }
                if self.status[i]
                    .compare_exchange(cur, STATUS_REDO, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.redo.push(i);
                    if cur == STATUS_VALIDATED {
                        invalidated_validated += 1;
                    }
                    break;
                }
            }
        }
        self.decrement_validated_count(invalidated_validated);
    }

    /// Check if all transactions have been validated.
    pub fn all_done(&self) -> bool {
        self.validated_count.load(Ordering::SeqCst) >= self.num_txs
    }

    fn decrement_validated_count(&self, count: usize) {
        if count == 0 {
            return;
        }
        let _ = self
            .validated_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_sub(count))
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_and_reschedule_only_decrements_currently_validated_txs_once() {
        let scheduler = Scheduler::new(4);

        scheduler.status[0].store(STATUS_VALIDATED, Ordering::SeqCst);
        scheduler.status[1].store(STATUS_VALIDATED, Ordering::SeqCst);
        scheduler.status[2].store(STATUS_VALIDATED, Ordering::SeqCst);
        scheduler.validated_count.store(3, Ordering::SeqCst);
        scheduler.val_cursor.store(3, Ordering::SeqCst);

        scheduler.abort_and_reschedule(1);

        assert_eq!(scheduler.validated_count.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.val_cursor.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.status[1].load(Ordering::SeqCst), STATUS_REDO);
        assert_eq!(scheduler.status[2].load(Ordering::SeqCst), STATUS_REDO);
    }

    #[test]
    fn finish_validation_does_not_revive_a_tx_invalidated_by_lower_abort() {
        let scheduler = Scheduler::new(4);

        scheduler.status[0].store(STATUS_VALIDATED, Ordering::SeqCst);
        scheduler.status[1].store(STATUS_EXECUTED, Ordering::SeqCst);
        scheduler.status[2].store(STATUS_EXECUTED, Ordering::SeqCst);
        scheduler.validated_count.store(1, Ordering::SeqCst);
        scheduler.val_cursor.store(3, Ordering::SeqCst);

        scheduler.abort_and_reschedule(1);
        scheduler.finish_validation(2);

        assert_eq!(scheduler.validated_count.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.status[2].load(Ordering::SeqCst), STATUS_REDO);
    }

    #[test]
    fn validation_waits_for_predecessor_completion_not_just_claim() {
        let scheduler = Scheduler::new(2);
        scheduler.exec_cursor.store(2, Ordering::SeqCst);
        scheduler.status[0].store(STATUS_EXECUTED, Ordering::SeqCst);
        scheduler.status[1].store(STATUS_EXECUTED, Ordering::SeqCst);

        assert!(matches!(scheduler.next_task(), Some(Task::Validate(0))));
        assert!(
            scheduler.next_task().is_none(),
            "tx 1 must not validate while tx 0's validation is still in flight"
        );

        scheduler.finish_validation(0);
        assert!(matches!(scheduler.next_task(), Some(Task::Validate(1))));
    }
}
