//! Block-STM scheduler: coordinates parallel execution and validation.
//!
//! Based on the Block-STM algorithm from Aptos, simplified for our use case:
//! 1. Execute transactions optimistically in parallel.
//! 2. Validate each executed transaction's read set (did its reads change?).
//! 3. Re-execute any transaction that fails validation, and — conservatively —
//!    every higher-indexed transaction (it may have read the aborted tx's writes).
//! 4. Repeat until all transactions are validated.
//!
//! Both execution and validation are **parallel and out-of-order**: workers pull
//! from a redo queue, a monotonic execute cursor, and a validation queue. Out-of-
//! order validation is sound because any abort re-marks *every* higher tx REDO, so
//! a tx validated early is re-validated whenever a lower tx re-executes. The
//! previous design validated strictly in order through a single `val_cursor`,
//! which serialized validation and left most workers spinning (the dominant cost
//! once the coinbase cascade was gone — see `docs/devlog-67`).

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

/// A work queue of tx indices guarded by an atomic length, so the empty-queue
/// hot path (the no-conflict common case) never locks the mutex.
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

    /// Pop one index, or `None` if empty (lock-free when empty).
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
    /// Next transaction index to execute (monotonically increasing).
    exec_cursor: AtomicUsize,
    /// Number of transactions currently in the VALIDATED state.
    validated_count: AtomicUsize,
    /// Tx indices marked REDO. The status array is the source of truth: entries
    /// are confirmed with a REDO->PENDING CAS on pop, so stale/duplicate entries
    /// are skipped.
    redo: WorkQueue,
    /// Tx indices that finished execution and await validation. Confirmed with a
    /// status==EXECUTED check on pop (a since-aborted entry is skipped). Lets any
    /// worker validate any executed tx in parallel.
    validate: WorkQueue,
}

impl Scheduler {
    pub fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            status: (0..num_txs)
                .map(|_| AtomicU32::new(STATUS_PENDING))
                .collect(),
            exec_cursor: AtomicUsize::new(0),
            validated_count: AtomicUsize::new(0),
            redo: WorkQueue::new(),
            validate: WorkQueue::new(),
        }
    }

    /// Pop a redo entry that is genuinely still REDO (claims it PENDING).
    fn pop_redo(&self) -> Option<TxIdx> {
        while let Some(cand) = self.redo.pop() {
            if self.status[cand]
                .compare_exchange(STATUS_REDO, STATUS_PENDING, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(cand);
            }
            // Stale/duplicate — already rescheduled through another path.
        }
        None
    }

    /// Pop a validation entry that is still EXECUTED (a since-aborted entry is
    /// skipped — it will be re-pushed when it re-executes).
    fn pop_validate(&self) -> Option<TxIdx> {
        while let Some(cand) = self.validate.pop() {
            if self.status[cand].load(Ordering::SeqCst) == STATUS_EXECUTED {
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
        // Priority 2: execute a new tx. Wait-free claim via fetch_add (a CAS
        // loop retry-stormed under contention). The cursor may overshoot num_txs.
        let idx = self.exec_cursor.fetch_add(1, Ordering::SeqCst);
        if idx < self.num_txs {
            return Some(Task::Execute(idx));
        }
        // Priority 3: validate an executed tx (parallel, any order).
        if let Some(i) = self.pop_validate() {
            return Some(Task::Validate(i));
        }
        None
    }

    /// Mark a transaction as executed and enqueue it for validation.
    pub fn finish_execution(&self, tx_idx: TxIdx) {
        self.status[tx_idx].store(STATUS_EXECUTED, Ordering::SeqCst);
        self.validate.push(tx_idx);
    }

    /// Mark a transaction as validated.
    pub fn finish_validation(&self, tx_idx: TxIdx) {
        // A lower-index abort may already have invalidated this tx while its
        // validation task was in flight. Only count it if it is still EXECUTED.
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
    /// every higher-indexed tx, which might have read this one's stale writes.
    pub fn abort_and_reschedule(&self, tx_idx: TxIdx) {
        // Swap to REDO and capture the previous status so we can keep
        // validated_count exact if it was already VALIDATED.
        let prev_self = self.status[tx_idx].swap(STATUS_REDO, Ordering::SeqCst);
        self.redo.push(tx_idx);

        let mut invalidated_validated = usize::from(prev_self == STATUS_VALIDATED);
        for i in (tx_idx + 1)..self.num_txs {
            let prev = self.status[i].load(Ordering::SeqCst);
            if prev == STATUS_EXECUTED || prev == STATUS_VALIDATED {
                self.status[i].store(STATUS_REDO, Ordering::SeqCst);
                self.redo.push(i);
                if prev == STATUS_VALIDATED {
                    invalidated_validated += 1;
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

        scheduler.abort_and_reschedule(1);

        assert_eq!(scheduler.validated_count.load(Ordering::SeqCst), 1);
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

        scheduler.abort_and_reschedule(1);
        scheduler.finish_validation(2);

        assert_eq!(scheduler.validated_count.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.status[2].load(Ordering::SeqCst), STATUS_REDO);
    }

    #[test]
    fn parallel_validation_queue_skips_aborted_entries() {
        let s = Scheduler::new(4);
        // Two txs execute -> both enqueued for validation.
        s.finish_execution(2);
        s.finish_execution(3);
        // tx1 aborts, re-marking 2 and 3 REDO.
        s.status[1].store(STATUS_EXECUTED, Ordering::SeqCst);
        s.abort_and_reschedule(1);
        // The stale validate entries (2,3 now REDO) must be skipped.
        assert!(s.pop_validate().is_none());
        // And they are available as redo work instead.
        let mut redos = vec![s.pop_redo(), s.pop_redo(), s.pop_redo()];
        redos.sort();
        assert_eq!(redos, vec![Some(1), Some(2), Some(3)]);
    }
}
