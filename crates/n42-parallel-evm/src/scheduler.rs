//! Block-STM scheduler: coordinates parallel execution and validation.
//!
//! Based on the Block-STM algorithm from Aptos, simplified for our use case:
//! 1. Execute all transactions optimistically in parallel.
//! 2. Validate each transaction's read set (did its reads change?).
//! 3. Re-execute any transaction that fails validation.
//! 4. Repeat until all transactions are validated.

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

/// Coordinates execution and validation of transactions across worker threads.
pub struct Scheduler {
    num_txs: usize,
    /// Per-transaction status.
    status: Vec<AtomicU32>,
    /// Next transaction index to execute (monotonically increasing).
    exec_cursor: AtomicUsize,
    /// Next transaction index to validate (monotonically increasing).
    val_cursor: AtomicUsize,
    /// Number of transactions that have been validated.
    validated_count: AtomicUsize,
    /// Queue of tx indices marked REDO. The status array remains the source of
    /// truth: entries are confirmed with a REDO->PENDING CAS on pop, so stale or
    /// duplicate entries (a tx aborted twice before being picked up) are skipped.
    /// Replaces the previous O(num_txs) status scan per task — that scan made
    /// task acquisition O(n) and the whole block O(n^2) (5000 txs measured 188s).
    redo: Mutex<VecDeque<TxIdx>>,
    /// Length of `redo`, mirrored as an atomic so the no-conflict hot path can
    /// skip locking the mutex entirely. Without this, every `try_get_task` (one
    /// per task, on every worker) locked the redo mutex even when no tx ever
    /// aborts — that lock was the dominant contention point, making the parallel
    /// path SLOWER with more threads (32 threads measured 4x slower than 2).
    redo_len: AtomicUsize,
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
            redo: Mutex::new(VecDeque::new()),
            redo_len: AtomicUsize::new(0),
        }
    }

    /// Pop redo entries until one is genuinely still REDO (or the queue is empty).
    /// Lock-free fast exit when the queue is empty (the no-conflict common case).
    fn pop_redo(&self) -> Option<TxIdx> {
        if self.redo_len.load(Ordering::Acquire) == 0 {
            return None;
        }
        loop {
            let cand = match self.redo.lock().pop_front() {
                Some(c) => {
                    self.redo_len.fetch_sub(1, Ordering::AcqRel);
                    c
                }
                None => return None,
            };
            if self.status[cand]
                .compare_exchange(STATUS_REDO, STATUS_PENDING, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(cand);
            }
            // Stale/duplicate entry — already rescheduled through another path.
        }
    }

    /// Push a REDO entry (keeps `redo_len` in sync with the queue).
    fn push_redo(&self, tx_idx: TxIdx) {
        self.redo.lock().push_back(tx_idx);
        self.redo_len.fetch_add(1, Ordering::AcqRel);
    }

    /// Get the next task for a worker. Returns `None` when all work is done.
    pub fn next_task(&self) -> Option<Task> {
        self.try_get_task()
    }

    fn try_get_task(&self) -> Option<Task> {
        // Priority 1: Re-execute transactions marked for redo (O(1) queue pop).
        if let Some(i) = self.pop_redo() {
            return Some(Task::Execute(i));
        }

        // Priority 2: Execute new transactions. Wait-free claim via fetch_add —
        // a CAS load/compare loop here caused retry storms under 32-thread
        // contention (the dominant cost on cheap txs). The cursor may overshoot
        // `num_txs`; we simply ignore claims past the end.
        let idx = self.exec_cursor.fetch_add(1, Ordering::SeqCst);
        if idx < self.num_txs {
            return Some(Task::Execute(idx));
        }

        // Priority 3: Validate executed transactions.
        loop {
            let idx = self.val_cursor.load(Ordering::SeqCst);
            if idx >= self.num_txs {
                break;
            }
            // Only validate if the tx has been executed.
            if self.status[idx].load(Ordering::SeqCst) != STATUS_EXECUTED {
                break; // Must validate in order — wait for this tx to be executed.
            }
            if self
                .val_cursor
                .compare_exchange(idx, idx + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(Task::Validate(idx));
            }
        }

        // Check if all done.
        if self.validated_count.load(Ordering::SeqCst) >= self.num_txs {
            return None;
        }

        // No task available but not done yet — spin briefly then check again.
        // This avoids busy-waiting by yielding to the thread pool.
        std::thread::yield_now();

        // After yield, try again for redo tasks.
        if let Some(i) = self.pop_redo() {
            return Some(Task::Execute(i));
        }

        // Check validation cursor again.
        let idx = self.val_cursor.load(Ordering::SeqCst);
        if idx < self.num_txs
            && self.status[idx].load(Ordering::SeqCst) == STATUS_EXECUTED
            && self
                .val_cursor
                .compare_exchange(idx, idx + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Some(Task::Validate(idx));
        }

        if self.validated_count.load(Ordering::SeqCst) >= self.num_txs {
            return None;
        }

        // Still not done — return None to let the worker exit this iteration.
        // The outer loop will call next_task() again.
        None
    }

    /// Mark a transaction as executed.
    pub fn finish_execution(&self, tx_idx: TxIdx) {
        self.status[tx_idx].store(STATUS_EXECUTED, Ordering::SeqCst);
    }

    /// Mark a transaction as validated.
    pub fn finish_validation(&self, tx_idx: TxIdx) {
        // A lower-index abort may already have invalidated this tx while its
        // validation task was still in flight. In that case, keep the REDO
        // marker instead of reviving the stale execution result.
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

    /// Mark a transaction for re-execution (validation failed).
    /// Also invalidates all higher-indexed transactions that might have read from this one.
    pub fn abort_and_reschedule(&self, tx_idx: TxIdx) {
        // Use swap to atomically set REDO and capture the previous status,
        // so we can correctly count this tx if it was already VALIDATED.
        let prev_self = self.status[tx_idx].swap(STATUS_REDO, Ordering::SeqCst);
        self.push_redo(tx_idx);

        // Reset validation cursor to re-validate from this point.
        loop {
            let current = self.val_cursor.load(Ordering::SeqCst);
            if current <= tx_idx {
                break;
            }
            if self
                .val_cursor
                .compare_exchange(current, tx_idx, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }

        // Mark all higher-indexed executed txs as needing redo too,
        // since they might have read stale data from the aborted tx.
        let mut invalidated_validated = if prev_self == STATUS_VALIDATED { 1 } else { 0 };
        for i in (tx_idx + 1)..self.num_txs {
            let prev = self.status[i].load(Ordering::SeqCst);
            if prev == STATUS_EXECUTED || prev == STATUS_VALIDATED {
                self.status[i].store(STATUS_REDO, Ordering::SeqCst);
                self.push_redo(i);
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
}
