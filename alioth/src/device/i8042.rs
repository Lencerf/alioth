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

#[derive(Debug)]
pub struct I8042;

impl Mmio for I8042 {
    fn size(&self) -> u64 {
        // from 0x60 to 0x68
        8
    }

    fn read(&self, offset: u64, size: u8) -> Result<u64> {
        if offset == 1 && size == 1 {
            Ok(0x20)
        } else if offset == 0x64 && size == 1 {
            Ok(0x0)
        } else {
            Ok(0)
        }
    }

    fn write(&self, offset: u64, size: u8, val: u64) -> Result<Action> {
        if offset == 0x4 && size == 1 && val == 0xfe {
            Ok(Action::Reset)
        } else {
            log::error!("Ignored write {offset:#x}, {size:#x}, {val:#x}");
            Ok(Action::None)
        }
    }
}

impl Pause for I8042 {
    fn pause(&self) -> super::Result<()> {
        Ok(())
    }

    fn resume(&self) -> super::Result<()> {
        Ok(())
    }
}

impl MmioDev for I8042 {}
