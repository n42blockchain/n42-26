//! Block-STM scheduler: coordinates parallel execution and validation.
//!
//! Based on the Block-STM algorithm from Aptos, simplified for our use case:
//! 1. Execute all transactions optimistically in parallel.
//! 2. Validate each transaction's read set (did its reads change?).
//! 3. Re-execute any transaction that fails validation.
//! 4. Repeat until all transactions are validated.

use crate::types::TxIdx;
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
    /// Number of active workers (for termination detection).
    active_workers: AtomicUsize,
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
            active_workers: AtomicUsize::new(0),
        }
    }

    /// Get the next task for a worker. Returns `None` when all work is done.
    pub fn next_task(&self) -> Option<Task> {
        self.active_workers.fetch_add(1, Ordering::SeqCst);

        let result = self.try_get_task();

        if result.is_none() {
            self.active_workers.fetch_sub(1, Ordering::SeqCst);
        }

        result
    }

    fn try_get_task(&self) -> Option<Task> {
        // Priority 1: Re-execute transactions marked for redo.
        for i in 0..self.num_txs {
            if self.status[i]
                .compare_exchange(
                    STATUS_REDO,
                    STATUS_PENDING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return Some(Task::Execute(i));
            }
        }

        // Priority 2: Execute new transactions.
        loop {
            let idx = self.exec_cursor.load(Ordering::SeqCst);
            if idx >= self.num_txs {
                break;
            }
            if self
                .exec_cursor
                .compare_exchange(idx, idx + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(Task::Execute(idx));
            }
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
        for i in 0..self.num_txs {
            if self.status[i]
                .compare_exchange(
                    STATUS_REDO,
                    STATUS_PENDING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return Some(Task::Execute(i));
            }
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
        self.active_workers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Mark a transaction as validated.
    pub fn finish_validation(&self, tx_idx: TxIdx) {
        self.status[tx_idx].store(STATUS_VALIDATED, Ordering::SeqCst);
        self.validated_count.fetch_add(1, Ordering::SeqCst);
        self.active_workers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Mark a transaction for re-execution (validation failed).
    /// Also invalidates all higher-indexed transactions that might have read from this one.
    pub fn abort_and_reschedule(&self, tx_idx: TxIdx) {
        self.status[tx_idx].store(STATUS_REDO, Ordering::SeqCst);

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
                // Decrement validated_count for txs that need re-validation.
                let revalidate = current - tx_idx;
                self.validated_count.fetch_sub(
                    revalidate.min(self.validated_count.load(Ordering::SeqCst)),
                    Ordering::SeqCst,
                );
                break;
            }
        }

        // Mark all higher-indexed executed txs as needing redo too,
        // since they might have read stale data from the aborted tx.
        for i in (tx_idx + 1)..self.num_txs {
            let prev = self.status[i].load(Ordering::SeqCst);
            if prev == STATUS_EXECUTED || prev == STATUS_VALIDATED {
                if prev == STATUS_VALIDATED {
                    self.validated_count.fetch_sub(1, Ordering::SeqCst);
                }
                self.status[i].store(STATUS_REDO, Ordering::SeqCst);
            }
        }

        self.active_workers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Check if all transactions have been validated.
    pub fn all_done(&self) -> bool {
        self.validated_count.load(Ordering::SeqCst) >= self.num_txs
    }
}
