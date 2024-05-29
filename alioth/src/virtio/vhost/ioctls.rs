use crate::{ioctl_none, ioctl_read, ioctl_write_buf, ioctl_write_ptr, ioctl_writeread};

use crate::virtio::vhost::bindings::{
    VhostMemory, VhostVringAddr, VhostVringFile, VhostVringState, VHOST_VIRTIO,
};

ioctl_read!(vhost_get_features, VHOST_VIRTIO, 0x00, u64);
ioctl_write_ptr!(vhost_set_features, VHOST_VIRTIO, 0x00, u64);
ioctl_none!(vhost_set_owner, VHOST_VIRTIO, 0x01);
ioctl_none!(vhost_reset_owner, VHOST_VIRTIO, 0x02);

ioctl_write_buf!(vhost_set_mem_table, VHOST_VIRTIO, 0x03, VhostMemory);
ioctl_write_ptr!(vhost_set_log_base, VHOST_VIRTIO, 0x04, u64);
ioctl_write_ptr!(vhost_set_log_fd, VHOST_VIRTIO, 0x07, i32);
// vhost_new_worker
// vhost_free_worker

ioctl_write_ptr!(vhost_set_vring_num, VHOST_VIRTIO, 0x10, VhostVringState);
ioctl_write_ptr!(vhost_set_vring_addr, VHOST_VIRTIO, 0x11, VhostVringAddr);
ioctl_write_ptr!(vhost_set_vring_base, VHOST_VIRTIO, 0x12, VhostVringState);
ioctl_writeread!(vhost_get_vring_base, VHOST_VIRTIO, 0x12, VhostVringState);
// num: number of buffers in the queue, or the availabe index
//
//
//
//
// vhost_set_vring_endian
// vhost_get_vring_endian
// vhost_attach_vring_worker
ioctl_write_ptr!(vhost_set_vring_kick, VHOST_VIRTIO, 0x20, VhostVringFile);
ioctl_write_ptr!(vhost_set_vring_call, VHOST_VIRTIO, 0x21, VhostVringFile);
ioctl_write_ptr!(vhost_set_vring_err, VHOST_VIRTIO, 0x22, VhostVringFile);

ioctl_write_ptr!(
    vhost_set_vring_busyloop_timeout,
    VHOST_VIRTIO,
    0x23,
    VhostVringState
);
ioctl_write_ptr!(
    vhost_get_vring_busyloop_timeout,
    VHOST_VIRTIO,
    0x24,
    VhostVringState
);

ioctl_write_ptr!(vhost_set_backend_features, VHOST_VIRTIO, 0x25, u64);
ioctl_read!(vhost_get_backend_features, VHOST_VIRTIO, 0x26, u64);

//
//
//
//
// vhost_net_set_backend
// vhost_scsi_set_endpoint
// vhost_scsi_clear_endpoint
// vhost_scsi_get_abi_version
// vhost_scsi_set_events_missed
// vhost_scsi_get_events_missed

ioctl_write_ptr!(vhost_vsock_set_guest_cid, VHOST_VIRTIO, 0x60, u64);
ioctl_write_ptr!(vhost_vsock_set_running, VHOST_VIRTIO, 0x61, i32);
//
//
// vhost_vdpa_get_device_id
// vhost_vdpa_get_status
// vhost_vdpa_set_status
// vhost_vdpa_get_config
// vhost_vdpa_set_config_call
// vhost_vdpa_get_iova_range
// vhost_vdpa_get_vqs_count
// vhost_vdpa_get_group_num
// vhost_vdpa_get_as_num
// vhost_vdpa_get_vring_group
// vhost_vdpa_resume
// vhost_vdpa_get_vring_desc_group

// #[cfg(test)]
// mod test {
//     use std::fs::File;

//     use crate::virtio::dev::vhost_vsock::VhostVsockFeature;
//     use crate::virtio::vhost::ioctls::{
//         vhost_get_backend_features, vhost_set_owner, vhost_vsock_set_guest_cid,
//     };

//     use super::vhost_get_features;

//     #[test]
//     fn test_ioctls() {
//         let vsock_dev = File::open("/dev/vhost-vsock").unwrap();
//         let feature = unsafe { vhost_get_features(&vsock_dev) }.unwrap();
//         println!(
//             "features = {:#x?}",
//             VhostVsockFeature::from_bits_retain(feature)
//         );
//         unsafe { vhost_set_owner(&vsock_dev) }.unwrap();
//         unsafe { vhost_vsock_set_guest_cid(&vsock_dev, &3) }.unwrap();
//         let backend_feature = unsafe { vhost_get_backend_features(&vsock_dev) }.unwrap();
//         println!("backend features = {:#x}", backend_feature)
//     }
// }
