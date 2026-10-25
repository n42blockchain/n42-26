//! Successful SSTORE observations for Gov5 dirty-write reconstruction.
//! Recorded per transaction on the execution thread; the standard instruction
//! still performs the write and charges its original gas.
use alloy_evm::{
    Database,
    eth::{EthEvm, EthEvmContext},
    precompiles::PrecompilesMap,
};
use alloy_primitives::{Address, U256};
use revm::{
    Inspector,
    bytecode::opcode,
    interpreter::{
        Host, Instruction, InstructionContext, InstructionExecResult,
        instructions::host,
        interpreter::EthInterpreter,
        interpreter_types::InputsTr,
    },
};
use std::cell::RefCell;
thread_local! { static STORES: RefCell<Option<Vec<(Address, U256, U256)>>> = const { RefCell::new(None) }; }
// Disabled outside the block executor: prewarming and raw EVM callers must not
// accumulate sightings. The guard also clears them on errors and unwinding.
pub(crate) struct Capture(std::marker::PhantomData<std::rc::Rc<()>>);
impl Capture {
    pub(crate) fn start() -> Self {
        STORES.with(|stores| *stores.borrow_mut() = Some(Vec::new()));
        Self(std::marker::PhantomData)
    }
    pub(crate) fn finish(self) -> Vec<(Address, U256, U256)> {
        STORES.with(|stores| stores.borrow_mut().take().unwrap_or_default())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        STORES.with(|stores| {
            stores.borrow_mut().take();
        });
    }
}
fn note(store: (Address, U256, U256)) {
    STORES.with(|stores| {
        if let Some(stores) = stores.borrow_mut().as_mut() {
            stores.push(store);
        }
    });
}
fn record<H: Host + ?Sized>(
    ctx: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    let store = match ctx.interpreter.stack.data().as_slice() {
        [.., value, key] => Some((ctx.interpreter.input.target_address(), *key, *value)),
        _ => None,
    };
    let result = host::sstore(ctx);
    if result.is_ok()
        && let Some(store) = store
    {
        note(store);
    }
    result
}
pub(crate) fn watch<DB: Database, I: Inspector<EthEvmContext<DB>>>(
    evm: EthEvm<DB, I, PrecompilesMap>,
    inspect: bool,
) -> EthEvm<DB, I, PrecompilesMap> {
    let mut inner = evm.into_inner();
    let gas = inner.instruction.gas_table()[opcode::SSTORE as usize];
    inner
        .instruction
        .insert_instruction(opcode::SSTORE, Instruction::new(record), gas);
    EthEvm::new(inner, inspect)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_execution_does_not_collect_and_unwinding_clears_capture() {
        let store = (Address::ZERO, U256::ZERO, U256::from(1));
        note(store);
        assert!(STORES.with(|stores| stores.borrow().is_none()));
        let capture = Capture::start();
        note(store);
        assert_eq!(capture.finish(), vec![store]);
        let _ = std::panic::catch_unwind(|| {
            let _capture = Capture::start();
            note(store);
            panic!("abort execution");
        });
        assert!(STORES.with(|stores| stores.borrow().is_none()));
        assert!(Capture::start().finish().is_empty());
    }
}
