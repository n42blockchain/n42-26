//! Successful SSTORE observations for Gov5 dirty-write reconstruction.
//! Recorded per transaction on the execution thread; the standard instruction
//! still performs the write and charges its original gas.
//!
//! evm2 selects instructions through a compile-time opcode table, so the
//! observing SSTORE is installed by [`execution_config`], which
//! [`crate::N42EvmFactory`] hands to every EVM it configures. The table is the
//! base table of the spec with this one instruction replaced by evm2's own
//! `sstore_with`, the body of the standard SSTORE with a hook after the write.
use alloy_primitives::{Address, U256};
use evm2::{
    BaseEvmConfig, BaseEvmTypes, EvmConfig, ExecutionConfig, OpcodeConfig, SpecId, Version,
    interpreter::{instructions::sstore_with, op},
};
use evm2_macros::instruction;
use std::cell::RefCell;

thread_local! { static STORES: RefCell<Option<Vec<(Address, U256)>>> = const { RefCell::new(None) }; }

// Disabled outside the block executor: prewarming and raw EVM callers must not
// accumulate sightings. The guard also clears them on errors and unwinding.
pub(crate) struct Capture(std::marker::PhantomData<std::rc::Rc<()>>);

impl Capture {
    pub(crate) fn start() -> Self {
        STORES.with(|stores| *stores.borrow_mut() = Some(Vec::new()));
        Self(std::marker::PhantomData)
    }

    /// The slots that a successful SSTORE of this transaction left at a value
    /// other than the one they had when the transaction began.
    pub(crate) fn finish(self) -> Vec<(Address, U256)> {
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

fn note(store: (Address, U256)) {
    STORES.with(|stores| {
        if let Some(stores) = stores.borrow_mut().as_mut() {
            stores.push(store);
        }
    });
}

/// The standard SSTORE, which also notes a successful write of a value other
/// than the slot's value at the start of the transaction.
#[instruction(dynamic_gas)]
pub(crate) fn watched_sstore(cx: _, [key, value]: [Word]) -> Result {
    let mut store = None;
    sstore_with(cx.gas, cx.state, key, value, |_, _, address, sstore| {
        if sstore.new_value != sstore.original_value {
            store = Some((*address, *key));
        }
        Ok(())
    })?;
    // Only once the instruction has succeeded: a write whose gas cannot be
    // paid halts the frame and is rolled back.
    if let Some(store) = store {
        note(store);
    }
    Ok(())
}

/// The base opcode table of a spec with the observing SSTORE.
#[derive(Debug)]
pub(crate) struct WatchedConfig<const BASE_SPEC_ID: u32>(());

impl<const BASE_SPEC_ID: u32> EvmConfig<BaseEvmTypes> for WatchedConfig<BASE_SPEC_ID> {
    const BASE_SPEC_ID: SpecId = SpecId::try_from_u32(BASE_SPEC_ID).expect("invalid spec id");
    const OPCODE_CONFIG: &'static OpcodeConfig<BaseEvmTypes> =
        &watched_opcode_config::<BASE_SPEC_ID>();
}

const fn watched_opcode_config<const BASE_SPEC_ID: u32>() -> OpcodeConfig<BaseEvmTypes> {
    let mut config = OpcodeConfig::<BaseEvmTypes>::base::<BaseEvmConfig<BASE_SPEC_ID>>();
    // Same static gas as the instruction it replaces.
    let gas = config.static_gas(op::SSTORE);
    config.set_instruction::<watched_sstore<BaseEvmTypes>>(op::SSTORE, gas);
    config
}

/// The execution configuration of `spec` whose SSTORE is observed.
pub(crate) fn execution_config(spec: SpecId, version: Version) -> ExecutionConfig<BaseEvmTypes> {
    let config = evm2::spec_to_generic!(spec, |SPEC| {
        ExecutionConfig::<BaseEvmTypes>::for_config::<WatchedConfig<SPEC>>()
    });
    config.with_version(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_execution_does_not_collect_and_unwinding_clears_capture() {
        let store = (Address::ZERO, U256::ZERO);
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

    #[test]
    fn every_spec_keeps_its_base_table_but_for_sstore() {
        for spec in [
            SpecId::FRONTIER,
            SpecId::SHANGHAI,
            SpecId::OSAKA,
            SpecId::AMSTERDAM,
        ] {
            let watched = execution_config(spec, Version::new(spec));
            let base =
                ExecutionConfig::<BaseEvmTypes>::for_spec_and_version(spec, Version::new(spec));
            assert_eq!(watched.base_spec_id(), spec);
            for opcode in 0..=u8::MAX {
                if opcode == op::INVALID {
                    continue;
                }
                assert_eq!(
                    watched.opcode_config().static_gas(opcode),
                    base.opcode_config().static_gas(opcode),
                    "{spec:?} opcode {opcode:#x} static gas"
                );
                assert_eq!(
                    watched
                        .opcode_config()
                        .is_overridden(base.opcode_config(), opcode),
                    opcode == op::SSTORE,
                    "{spec:?} opcode {opcode:#x}"
                );
            }
        }
    }
}
