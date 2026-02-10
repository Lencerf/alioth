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

use crate::device::{MmioDev, Pause};
use crate::mem::Result;
use crate::mem::emulated::{Action, Mmio};

pub const PORT_FWDBG: u16 = 0x402;

#[derive(Debug)]
pub struct FwDbg;

impl Mmio for FwDbg {
    fn size(&self) -> u64 {
        1
    }

    fn read(&self, _offset: u64, _size: u8) -> Result<u64> {
        Ok(0xe9)
    }

    fn write(&self, _offset: u64, _size: u8, val: u64) -> Result<Action> {
        print!("{}", val as u8 as char);
        Ok(Action::None)
    }
}

impl Pause for FwDbg {
    fn pause(&self) -> super::Result<()> {
        Ok(())
    }

    fn resume(&self) -> super::Result<()> {
        Ok(())
    }
}

impl MmioDev for FwDbg {}
