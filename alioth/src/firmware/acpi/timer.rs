use std::time::Instant;

use crate::mem::Result;
use crate::mem::emulated::{Action, Mmio};

#[derive(Debug)]
pub struct AcpiPmTimerDevice {
    start: Instant,
}

impl AcpiPmTimerDevice {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for AcpiPmTimerDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl Mmio for AcpiPmTimerDevice {
    fn read(&self, _offset: u64, _size: u8) -> Result<u64> {
        let now = Instant::now();
        let since = now.duration_since(self.start);
        let nanos = since.as_nanos();

        const PM_TIMER_FREQUENCY_HZ: u128 = 3_579_545;
        const NANOS_PER_SECOND: u128 = 1_000_000_000;

        let counter = (nanos * PM_TIMER_FREQUENCY_HZ) / NANOS_PER_SECOND;
        let counter: u32 = (counter & 0xffff_ffff) as u32;

        Ok(counter as u64)
    }

    fn write(&self, _offset: u64, _size: u8, _val: u64) -> Result<Action> {
        Ok(Action::None)
    }

    fn size(&self) -> u64 {
        4
    }
}
