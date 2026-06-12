//! Block-STM worker loop: each rayon worker pulls tasks (execute / validate) from
//! the shared scheduler until the block converges.

use crate::coinbase::DeferredCoinbase;
use crate::execution::{execute_single_tx, validate_read_set};
use crate::mv_memory::MvMemory;
use crate::output::TxOutputInternal;
use crate::scheduler::{Scheduler, Task};
use parking_lot::Mutex;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::database_interface::DatabaseRef;
use std::fmt;

/// Repeatedly fetch and run tasks from the scheduler until all are validated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn worker_loop<DB>(
    scheduler: &Scheduler,
    txs: &[TxEnv],
    base_db: &DB,
    mv: &MvMemory,
    cfg_env: &CfgEnv,
    block_env: &BlockEnv,
    outputs: &[Mutex<Option<TxOutputInternal>>],
    coinbase: Option<&DeferredCoinbase>,
) where
    DB: DatabaseRef + Send + Sync,
    DB::Error: fmt::Display + Send,
{
    let mut retries = 0;
    loop {
        let task = match scheduler.next_task() {
            Some(t) => {
                retries = 0;
                t
            }
            None => {
                if scheduler.all_done() {
                    break;
                }
                retries += 1;
                if retries > 100 {
                    break; // Avoid infinite spin; the outer round re-spawns.
                }
                std::thread::yield_now();
                continue;
            }
        };

        match task {
            Task::Execute(tx_idx) => {
                mv.clear_tx(tx_idx);

                let output = execute_single_tx(
                    tx_idx,
                    &txs[tx_idx],
                    base_db,
                    mv,
                    cfg_env,
                    block_env,
                    coinbase,
                );

                if let Some(ref out) = output {
                    for (addr, write) in &out.account_writes {
                        mv.write_account(tx_idx, *addr, write);
                    }
                    for &(addr, slot, value) in &out.storage_writes {
                        mv.write_storage(tx_idx, addr, slot, value);
                    }
                    if let Some(delta) = out.bene_delta {
                        mv.record_bene_delta(tx_idx, delta);
                    }
                }

                *outputs[tx_idx].lock() = output;
                scheduler.finish_execution(tx_idx);
            }
            Task::Validate(tx_idx) => {
                let valid = outputs[tx_idx]
                    .lock()
                    .as_ref()
                    .map(|out| validate_read_set(tx_idx, &out.read_set, mv, base_db))
                    .unwrap_or(false);

                if valid {
                    scheduler.finish_validation(tx_idx);
                } else {
                    scheduler.abort_and_reschedule(tx_idx);
                }
            }
        }
    }
}
