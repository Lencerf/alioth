// Copyright 2026 Google LLC
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

use std::collections::HashMap;
use std::fs;

use alioth::virtio::dev::net::NetFeature;
use alioth::virtio::{self, DeviceId, VirtioFeature};
use anyhow::Result;
use glob::glob;

#[derive(Debug)]
struct FeatureStatus {
    driver: Option<bool>,
    device: Option<bool>,
    active: Option<bool>,
}

#[derive(Debug)]
struct VirtioDevice {
    name: String,
    ty: DeviceId,
    virtio_features: HashMap<u8, FeatureStatus>,
    device_features: HashMap<u8, FeatureStatus>,
}

fn get_status(bits: &[u8], index: usize) -> Option<bool> {
    bits.get(index).and_then(|bit| match *bit {
        b'1' => Some(true),
        b'0' => Some(false),
        _ => None,
    })
}

fn status_flag(status: Option<bool>) -> char {
    match status {
        Some(true) => '+',
        Some(false) => '-',
        None => '_',
    }
}

fn list_virtio_devices() -> Result<()> {
    let paths = glob("/sys/bus/virtio/devices/*")?;
    let mut devices = Vec::new();

    for entry in paths {
        let Ok(path) = entry else {
            continue;
        };
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().to_string();

        let Ok(ty) = fs::read_to_string(path.join("device")) else {
            eprintln!("Failed to read device");
            continue;
        };
        // println!("ty={ty:?}");
        let Ok(id) = u16::from_str_radix(ty.trim().strip_prefix("0x").unwrap(), 16) else {
            eprintln!("failed to parse device id");
            continue;
        };
        let dev_id = DeviceId::from(id);

        let active_bits = fs::read(path.join("features")).unwrap_or_default();
        let active_bits = active_bits.trim_ascii();

        let dev_bits = fs::read(path.join("device_features")).unwrap_or_default();
        let dev_bits = dev_bits.trim_ascii();

        let drv_bits = fs::read(path.join("driver/features")).unwrap_or_default();
        let drv_bits = drv_bits.trim_ascii();

        let mut virtio_features = HashMap::new();
        let mut device_features = HashMap::new();

        for i in 0..128 {
            let active = get_status(&active_bits, i);
            let dev_supported = get_status(&dev_bits, i);
            let drv_supported = get_status(&drv_bits, i);
            let f = VirtioFeature::from_bits_truncate(1 << i);

            if !f.is_empty() {
                virtio_features.insert(
                    i as u8,
                    FeatureStatus {
                        driver: None,
                        device: dev_supported,
                        active,
                    },
                );
                continue;
            }

            match dev_id {
                DeviceId::NET => {
                    let f = NetFeature::from_bits_truncate(1 << i);
                    if !f.is_empty() {
                        device_features.insert(
                            i as u8,
                            FeatureStatus {
                                driver: drv_supported,
                                device: dev_supported,
                                active,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        devices.push(VirtioDevice {
            ty: dev_id,
            name: name,
            virtio_features,
            device_features,
        });
    }
    for mut dev in devices {
        println!("{}: {:?}", dev.name, dev.ty);
        for (name, feat) in VirtioFeature::all().iter_names() {
            let bits = feat.bits().trailing_zeros() as u8;
            let Some(status) = dev.virtio_features.remove(&bits) else {
                continue;
            };
            println!(
                " {}{}{} {name}({bits})",
                status_flag(status.device),
                status_flag(status.driver),
                status_flag(status.active)
            );
            // device.
        }
        match dev.ty {
            DeviceId::NET => {
                for (name, feat) in NetFeature::all().iter_names() {
                    let bits = feat.bits().trailing_ones() as u8;
                    let Some(status) = dev.device_features.remove(&bits) else {
                        continue;
                    };
                    println!(
                        " {}{}{} {name}({bits})",
                        status_flag(status.device),
                        status_flag(status.driver),
                        status_flag(status.active)
                    );
                }
            }
            _ => {}
        }
        // for (feature, status) in &dev.virtio_features {
        //     let name = VirtioFeature::from_bits_truncate(1 << feature)
        //         .iter_names()
        //         .collect::<Vec<_>>()[0]
        //         .0;
        //     println!(
        //         " {}{}{} {name}({feature}) ",
        //         status_flag(status.driver),
        //         status_flag(status.device),
        //         status_flag(status.active),
        //         // feature,
        //         // VirtioFeature::from_bits_truncate(1 << feature)
        //         //     .iter_names()
        //         //     .collect::<Vec<_>>(),
        //     );
        // }
    }
    Ok(())
}

fn main() -> Result<()> {
    list_virtio_devices()
}
