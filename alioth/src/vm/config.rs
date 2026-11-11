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

#[cfg(target_os = "linux")]
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_aco::Help;

use crate::board::BoardSpec;
use crate::device::console::{ConsoleSpec, PtyConsoleSpec};
#[cfg(target_arch = "x86_64")]
use crate::device::fw_cfg::FwCfgItemSpec;
use crate::loader::PayloadSpec;
#[cfg(target_os = "linux")]
use crate::vfio::{VfioCdevSpec, VfioContainerSpec, VfioGroupSpec, VfioIoasSpec, VfioUserSpec};
use crate::virtio::dev::balloon::BalloonSpec;
use crate::virtio::dev::blk::BlkFileSpec;
use crate::virtio::dev::entropy::EntropySpec;
use crate::virtio::dev::fs::shared_dir::SharedDirSpec;
#[cfg(target_os = "macos")]
use crate::virtio::dev::net::vmnet::VmnetSpec;
use crate::virtio::dev::vsock::UdsVsockSpec;
#[cfg(target_os = "linux")]
use crate::virtio::dev::{fs::vu::VuFsSpec, net::tap::TapNetSpec, vsock::VhostVsockSpec};

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Help)]
pub struct VuSocket {
    /// Path to the vhost-user UNIX domain socket.
    pub socket: Box<Path>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Help)]
#[serde(rename_all = "snake_case")]
pub enum NetSpec {
    /// VirtIO net device backed by TUN/TAP, MacVTap, or IPVTap.
    #[cfg(target_os = "linux")]
    #[serde(alias = "tap")]
    Tap(TapNetSpec),
    /// VirtIO net device backed by vmnet framework.
    #[cfg(target_os = "macos")]
    #[serde(alias = "vmnet")]
    Vmnet(VmnetSpec),
    /// vhost-user net device over a Unix domain socket.
    #[cfg(target_os = "linux")]
    #[serde(alias = "vu")]
    Vu(VuSocket),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Help)]
#[serde(rename_all = "snake_case")]
pub enum BlkSpec {
    /// VirtIO block device backed by a disk image file.
    #[serde(alias = "file")]
    File(BlkFileSpec),
    #[cfg(target_os = "linux")]
    #[serde(alias = "vu")]
    /// vhost-user block device over a Unix domain socket.
    Vu(VuSocket),
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone, Help)]
#[serde(rename_all = "snake_case")]
pub enum FsSpec {
    /// VirtIO FS device backed by a shared directory.
    #[serde(alias = "dir")]
    Dir(SharedDirSpec),
    #[cfg(target_os = "linux")]
    /// VirtIO FS device backed by a vhost-user process, e.g. virtiofsd.
    #[serde(alias = "vu")]
    Vu(VuFsSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Help)]
#[serde(rename_all = "snake_case")]
pub enum VsockSpec {
    #[cfg(target_os = "linux")]
    /// Vsock device backed by host kernel vhost-vsock module.
    #[serde(alias = "vhost")]
    Vhost(VhostVsockSpec),
    /// Vsock device mapped to a Unix domain socket.
    #[serde(alias = "uds")]
    Uds(UdsVsockSpec),
    #[cfg(target_os = "linux")]
    /// Vsock device backed by a vhost-user process.
    #[serde(alias = "vu")]
    Vu(VuSocket),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, Help)]
pub struct VmSpec {
    /// The memory, vCPUs, and platform of the guest.
    #[serde(default)]
    pub board: BoardSpec,

    /// What the guest boots.
    pub payload: PayloadSpec,

    /// The guest console. [default: pty]
    // A deserialized spec, e.g. from an API request, defaults to a PTY
    // console, since a VMM serving many VMs has no terminal for them.
    #[serde(default = "default_console")]
    pub console: ConsoleSpec,
    /// VirtIO net devices.
    #[serde(default)]
    pub net: Vec<NetSpec>,
    /// VirtIO block devices.
    #[serde(default)]
    pub blk: Vec<BlkSpec>,
    /// VirtIO filesystem devices.
    #[serde(default)]
    pub fs: Vec<FsSpec>,
    /// A VirtIO vsock device.
    pub vsock: Option<VsockSpec>,
    /// A VirtIO entropy device.
    pub entropy: Option<EntropySpec>,
    /// A VirtIO balloon device.
    pub balloon: Option<BalloonSpec>,
    /// Add a pvpanic device.
    #[serde(default)]
    pub pvpanic: bool,

    /// Extra items of the fw_cfg device.
    #[cfg(target_arch = "x86_64")]
    #[serde(default)]
    pub fw_cfg: Vec<FwCfgItemSpec>,

    /// Host PCI devices assigned to the guest using the IOMMUFD API.
    #[cfg(target_os = "linux")]
    #[serde(default)]
    pub vfio_cdev: Vec<VfioCdevSpec>,
    /// IO address spaces of IOMMUFD.
    #[cfg(target_os = "linux")]
    #[serde(default)]
    pub vfio_ioas: Vec<VfioIoasSpec>,
    /// Host PCI devices assigned to the guest using the legacy VFIO API.
    #[cfg(target_os = "linux")]
    #[serde(default)]
    pub vfio_group: Vec<VfioGroupSpec>,
    /// VFIO containers.
    #[cfg(target_os = "linux")]
    #[serde(default)]
    pub vfio_container: Vec<VfioContainerSpec>,
    /// vfio-user devices assigned to the guest.
    #[cfg(target_os = "linux")]
    #[serde(default)]
    pub vfio_user: Vec<VfioUserSpec>,
}

fn default_console() -> ConsoleSpec {
    ConsoleSpec::Pty(PtyConsoleSpec::default())
}
