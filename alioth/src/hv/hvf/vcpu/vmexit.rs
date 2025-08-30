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

use snafu::ResultExt;

use crate::arch::psci::{PSCI_VERSION_1_1, PsciFunc, PsciMigrateInfo};
use crate::arch::reg::{EsrEl2DataAbort, EsrEl2Ec, EsrEl2SysRegTrap, Reg, SReg, encode};
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
            EsrEl2Ec::SYS_REG_TRAP => self.handle_sys_reg_trap(EsrEl2SysRegTrap(esr.iss())),
            _ => error::VmExit {
                msg: format!("unhandled esr: {esr:x?}"),
            }
            .fail(),
        }
    }

    pub fn handle_sys_reg_trap(&mut self, iss: EsrEl2SysRegTrap) -> Result<bool> {
        let rt = HvReg::from(iss.rt());
        if iss.is_read() {
            return error::VmExit {
                msg: format!("unhandled iss: {iss:x?}"),
            }
            .fail();
        } else {
            let mut val = 0;
            let ret = unsafe { hv_vcpu_get_reg(self.vcpu_id, rt, &mut val) };
            check_ret(ret).context(error::VcpuReg)?;
            let sreg = SReg::from(encode(
                iss.op0(),
                iss.op1(),
                iss.crn(),
                iss.crm(),
                iss.op2(),
            ));
            if sreg == SReg::OSDLR_EL1 || sreg == SReg::OSLAR_EL1 {
                log::info!("sreg = {sreg:x?}, val = {val:x?}, rt = {rt:?} {iss:x?}");
                self.advance_pc = true;
                return Ok(true);
            }
            error::VmExit {
                msg: format!("sreg = {sreg:x?}, val = {val:x?}, rt = {rt:?} {iss:x?}"),
            }
            .fail()
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
        let ret = match PsciFunc::from(func) {
            PsciFunc::PSCI_VERSION => {
                log::info!("PSCI_VERSION");
                PSCI_VERSION_1_1 as u64
            }
            PsciFunc::MIGRATE_INFO_TYPE => PsciMigrateInfo::NOT_REQUIRED.raw() as u64,
            PsciFunc::PSCI_FEATURES => {
                let f = self.get_reg(Reg::X1).unwrap() as u32;
                match PsciFunc::from(f) {
                    PsciFunc::CPU_SUSPEND_64
                    | PsciFunc::SYSTEM_OFF2_64
                    | PsciFunc::SYSTEM_RESET2_64 => 0,
                    _ => u64::MAX,
                }
            }
            PsciFunc::SYSTEM_OFF | PsciFunc::SYSTEM_OFF2_32 | PsciFunc::SYSTEM_OFF2_64 => {
                log::info!("SYSTEM_OFF");
                self.vmexit = VmExit::Shutdown;
                return Ok(true);
            }
            f => {
                return error::VmExit {
                    msg: format!("HVC: {f:x?}"),
                }
                .fail();
            }
        };
        self.set_regs(&[(Reg::X0, ret)]).unwrap();
        Ok(false)
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
