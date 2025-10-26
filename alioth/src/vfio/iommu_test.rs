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

use std::os::fd::AsRawFd;

use libmock::add_mocks;

use crate::vfio::iommu::Iommu;

// use crate::vfio::iommu::Iommu;

unsafe extern "C" fn mocked_open64(
    skip: *mut bool,
    _path: *const libc::c_char,
    _flags: libc::c_int,
    _mode: libc::c_int,
) -> libc::c_int {
    println!("mocked_open64 called");
    unsafe {
        *skip = true;
    }
    -1
}

#[test]
fn test_iommu() {
    // let m = Rc::new(Mock::Open64(Box::new(
    //     |_: *const c_char, _: c_int, _: c_int| -> c_int { 0 },
    // )));
    // let m = null_mut();
    // unsafe {
    add_mocks(c"open64", mocked_open64 as *mut _);
    // }

    let iommu_path = "/dev/iommu";
    let _iommu = Iommu::new(iommu_path).unwrap();
    println!("iommu fd = {}", _iommu.fd.as_raw_fd());
}
