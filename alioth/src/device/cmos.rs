// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::atomic::{AtomicU8, Ordering};

use chrono::{Datelike, Timelike, Utc};

use crate::device::{MmioDev, Pause};
use crate::mem::Result;
use crate::mem::emulated::{Action, Mmio};

pub const PORT_CMOS_REG: u16 = 0x70;
pub const PORT_CMOS_DATA: u16 = 0x71;

pub const CMOS_REG_MASK: u8 = 0b111_1111;

// http://www.walshcomptech.com/ohlandl/config/cmos_ram.html#Hex_00F
// https://stanislavs.org/helppc/cmos_ram.html
#[derive(Debug, Default)]
pub struct Cmos {
    reg: AtomicU8,
}

impl Mmio for Cmos {
    fn size(&self) -> u64 {
        2
    }

    fn read(&self, offset: u64, _size: u8) -> Result<u64> {
        let reg = self.reg.load(Ordering::Acquire);
        if offset == 0 {
            Ok(reg as u64)
        } else {
            let ret = match reg & CMOS_REG_MASK {
                0x00 => Utc::now().naive_local().second(),
                0x02 => Utc::now().naive_local().minute(),
                0x04 => Utc::now().naive_local().hour(),
                0x06 => Utc::now().naive_local().weekday().number_from_sunday(),
                0x07 => Utc::now().naive_local().day(),
                0x08 => Utc::now().naive_local().month(),
                0x09 => Utc::now().naive_local().year() as u32 - 2000,
                0x0a => {
                    //http://faydoc.tripod.com/structures/04/0406.htm
                    // let local = Utc::now().naive_local().nanosecond();
                    // 0x26 | (((local < 244140) as u32) << 7)
                    0x26
                }
                0x0b => 0b110, // 24hour, http://faydoc.tripod.com/structures/04/0407.htm
                0x0d => 1 << 7, // has power
                _ => {
                    log::error!("return 0 for unknown rtc register {:#x}", reg);
                    0
                }
            };
            Ok(ret as u64)
        }
    }

    fn write(&self, offset: u64, _size: u8, val: u64) -> Result<Action> {
        if offset == 0 {
            self.reg.store(val as u8, Ordering::Release);
        } else {
            let reg = self.reg.load(Ordering::Acquire);
            if reg == 0x8f && val == 0x0 {
                // arch/x86/kernel/reboot.c
                log::warn!("write {val:#x} to CMOS reg {reg:#x?}: reboot");
                return Ok(Action::Reset);
            }
            log::warn!("write {val:#x} to CMOS reg {reg:#x?} ignored");
        }
        Ok(Action::None)
    }
}

impl Pause for Cmos {
    fn pause(&self) -> super::Result<()> {
        Ok(())
    }

    fn resume(&self) -> super::Result<()> {
        Ok(())
    }
}

impl MmioDev for Cmos {}
