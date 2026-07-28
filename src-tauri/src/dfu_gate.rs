//! Process-wide exclusion for destructive firmware-update backends.

use std::sync::atomic::{AtomicU8, Ordering};

const NONE: u8 = 0;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum DfuBackend {
    HpmUsb = 1,
    Stm32Can = 2,
}

#[derive(Default)]
pub struct DfuMutationGate {
    owner: AtomicU8,
}

impl DfuMutationGate {
    pub fn try_acquire(&self, backend: DfuBackend) -> Result<DfuMutationPermit<'_>, &'static str> {
        self.owner
            .compare_exchange(NONE, backend as u8, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "another firmware update backend is already active")?;
        Ok(DfuMutationPermit {
            gate: self,
            owner: backend as u8,
        })
    }

    pub fn is_active(&self) -> bool {
        self.owner.load(Ordering::Acquire) != NONE
    }
}

pub struct DfuMutationPermit<'a> {
    gate: &'a DfuMutationGate,
    owner: u8,
}

impl Drop for DfuMutationPermit<'_> {
    fn drop(&mut self) {
        let released =
            self.gate
                .owner
                .compare_exchange(self.owner, NONE, Ordering::AcqRel, Ordering::Acquire);
        debug_assert!(
            released.is_ok(),
            "DFU mutation gate owner changed unexpectedly"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_backend_can_hold_the_mutation_gate() {
        let gate = DfuMutationGate::default();
        let first = gate.try_acquire(DfuBackend::HpmUsb).unwrap();
        assert!(gate.is_active());
        assert!(gate.try_acquire(DfuBackend::Stm32Can).is_err());
        drop(first);
        assert!(!gate.is_active());
        assert!(gate.try_acquire(DfuBackend::Stm32Can).is_ok());
    }
}
