// Copyright 2024 Google LLC
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

use crate::arch::psci::{PSCI_VERSION_1_1, PsciFunc};
use crate::arch::reg::{EsrEl2DataAbort, EsrEl2Ec, Reg};
use crate::hv::hvf::bindings::{HvReg, HvVcpuExitException, hv_vcpu_get_reg};
use crate::hv::hvf::check_ret;
use crate::hv::hvf::vcpu::HvfVcpu;
use crate::hv::{Result, Vcpu, VmExit, error};

impl HvfVcpu {
    // https://esr.arm64.dev/
    pub fn handle_exception(&mut self, exception: &HvVcpuExitException) -> Result<bool> {
        let esr = exception.syndrome;
        match esr.ec() {
            EsrEl2Ec::DATA_ABORT_LOWER => {
                self.decode_data_abort(EsrEl2DataAbort(esr.iss()), exception.physical_address)
            }
            EsrEl2Ec::HVC_64 => self.handle_hvc(),
            _ => error::VmExit {
                msg: "Unhandled exception".to_owned(),
            }
            .fail(),
        }
    }

    pub fn handle_hvc(&mut self) -> Result<bool> {
        log::info!(
            "hvc: x0 = {:x}, x1 = {:x}, x2 = {:x}, x3 = {:x}, x4 = {:x}",
            self.get_reg(Reg::X0).unwrap(),
            self.get_reg(Reg::X1).unwrap(),
            self.get_reg(Reg::X2).unwrap(),
            self.get_reg(Reg::X3).unwrap(),
            self.get_reg(Reg::X4).unwrap(),
        );
        let func = self.get_reg(Reg::X0).unwrap() as u32;
        match PsciFunc::from(func) {
            PsciFunc::PSCI_VERSION => {
                log::info!("PSCI_VERSION");
                self.set_regs(&[(Reg::X0, PSCI_VERSION_1_1 as u64)])
                    .unwrap();
                Ok(false)
            }
            f => error::VmExit {
                msg: format!("HVC: {f:x?}"),
            }
            .fail(),
        }
    }

    pub fn decode_data_abort(&mut self, iss: EsrEl2DataAbort, gpa: u64) -> Result<bool> {
        if !iss.isv() {
            return error::VmExit {
                msg: "Data abort: Instruction Syndrome Valid = false".to_owned(),
            }
            .fail();
        }
        let reg = HvReg::from(iss.srt());
        let write = if iss.wnr() {
            let mut value = 0;
            let ret = unsafe { hv_vcpu_get_reg(self.vcpu_id, reg, &mut value) };
            check_ret(ret).unwrap();
            Some(value)
        } else {
            None
        };
        self.exit_reg = Some(reg);
        self.vmexit = VmExit::Mmio {
            addr: gpa as _,
            write,
            size: 1 << iss.sas(),
        };
        self.advance_pc = true;
        Ok(true)
    }
}
