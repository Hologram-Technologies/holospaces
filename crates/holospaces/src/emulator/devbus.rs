//! **Shared virtio-mmio device bus** — the substrate-backed `virtio` devices and
//! their split-virtqueue servicing, used by **both** ISA targets (the RISC-V
//! [`Emulator`](super::Emulator) and the AArch64 [`Cpu`](super::aarch64::Cpu)).
//!
//! The devices themselves are the hologram substrate: the block device's sectors
//! are κ-addressed content in the store ([`KappaBacking`](super::KappaBacking),
//! `CC-7`), the workspace is a 9P tree, the network terminates in the userspace
//! NAT ([`net`](super::net)). Only the CPU-facing MMIO transport differs between
//! the two machines (where RAM is based, and which interrupt controller latches
//! the IRQ — the RISC-V PLIC or the AArch64 GIC). So the queue-walking and the
//! device operations live here, parameterized by a [`GuestRam`] view and called
//! by each machine's thin MMIO dispatch — one implementation, no per-ISA
//! re-implementation (Law L4; the same κ-disk is read by both, Law L1).
//!
//! This is a child module of [`super`], so it reaches the device structs'
//! fields directly (an ancestor's privates are visible to a descendant) — no
//! widening of their visibility.

#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{vec, vec::Vec};

use super::{KappaBacking, VirtioBlk};

// Split-virtqueue descriptor flags (OASIS VirtIO v1.2 §2.7).
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

/// A view of a machine's guest RAM for virtqueue access: a byte slice mapped at
/// `base` (the guest-physical address of `ram[0]`). Virtqueue descriptors and
/// buffers always live in RAM, so accesses outside `[base, base+len)` read as 0 /
/// are ignored (a malformed-descriptor guard identical on both machines).
pub(super) struct GuestRam<'a> {
    pub ram: &'a mut [u8],
    pub base: u64,
}

impl GuestRam<'_> {
    #[inline]
    fn off(&self, pa: u64, len: usize) -> Option<usize> {
        let o = pa.checked_sub(self.base)?;
        let end = o.checked_add(len as u64)?;
        if end <= self.ram.len() as u64 {
            Some(o as usize)
        } else {
            None
        }
    }
    fn rd(&self, pa: u64, width: usize) -> u64 {
        match self.off(pa, width) {
            Some(o) => {
                let mut v = 0u64;
                for i in 0..width {
                    v |= u64::from(self.ram[o + i]) << (8 * i);
                }
                v
            }
            None => 0,
        }
    }
    fn wr(&mut self, pa: u64, width: usize, v: u64) {
        if let Some(o) = self.off(pa, width) {
            for i in 0..width {
                self.ram[o + i] = (v >> (8 * i)) as u8;
            }
        }
    }
    pub(super) fn rd16(&self, a: u64) -> u16 {
        self.rd(a, 2) as u16
    }
    pub(super) fn rd32(&self, a: u64) -> u32 {
        self.rd(a, 4) as u32
    }
    pub(super) fn rd64(&self, a: u64) -> u64 {
        self.rd(a, 8)
    }
    pub(super) fn wr8(&mut self, a: u64, v: u8) {
        self.wr(a, 1, u64::from(v));
    }
    pub(super) fn wr16(&mut self, a: u64, v: u16) {
        self.wr(a, 2, u64::from(v));
    }
    pub(super) fn wr32(&mut self, a: u64, v: u32) {
        self.wr(a, 4, u64::from(v));
    }
    pub(super) fn read_bytes(&self, pa: u64, dst: &mut [u8]) {
        match self.off(pa, dst.len()) {
            Some(o) => dst.copy_from_slice(&self.ram[o..o + dst.len()]),
            None => {
                for (i, b) in dst.iter_mut().enumerate() {
                    *b = self.rd(pa + i as u64, 1) as u8;
                }
            }
        }
    }
    pub(super) fn write_bytes(&mut self, pa: u64, src: &[u8]) {
        match self.off(pa, src.len()) {
            Some(o) => self.ram[o..o + src.len()].copy_from_slice(src),
            None => {
                for (i, b) in src.iter().enumerate() {
                    self.wr(pa + i as u64, 1, u64::from(*b));
                }
            }
        }
    }

    /// Walk a descriptor chain from `head`, collecting `(addr, len, flags)` —
    /// shared by every device's queue servicing. `qsz` bounds a malformed loop.
    pub(super) fn collect_chain(
        &self,
        desc_addr: u64,
        head: u16,
        qsz: usize,
    ) -> Vec<(u64, u32, u16)> {
        let mut chain = Vec::new();
        let mut idx = head;
        loop {
            let d = desc_addr + 16 * u64::from(idx);
            let addr = self.rd64(d);
            let len = self.rd32(d + 8);
            let flags = self.rd16(d + 12);
            let next = self.rd16(d + 14);
            chain.push((addr, len, flags));
            if flags & VIRTQ_DESC_F_NEXT == 0 || chain.len() > qsz {
                break;
            }
            idx = next;
        }
        chain
    }
}

// ── VirtIO block device (the κ-disk rootfs; CC-14) ──────────────────────────

/// Read a `virtio-mmio` register / block-config field of the block device.
pub(super) fn blk_mmio_read(dev: Option<&VirtioBlk>, off: u64) -> u64 {
    let Some(dev) = dev else {
        return 0;
    };
    match off {
        0x000 => 0x7472_6976, // MagicValue "virt"
        0x004 => 2,           // Version (modern)
        0x008 => 2,           // DeviceID = block
        0x00c => 0x554d_4551, // VendorID "QEMU"
        0x010 => match dev.device_features_sel {
            1 => 1, // VIRTIO_F_VERSION_1 (bit 32 = bit 0 of word 1)
            _ => 0,
        },
        0x034 => 1024, // QueueNumMax
        0x044 => u64::from(dev.queue_ready),
        0x060 => u64::from(dev.interrupt_status),
        0x070 => u64::from(dev.status),
        0x0fc => 0,
        0x100 => dev.capacity_sectors() & 0xffff_ffff,
        0x104 => dev.capacity_sectors() >> 32,
        _ => 0,
    }
}

/// Write a `virtio-mmio` register of the block device. Returns `true` if the
/// write was a `QueueNotify` (the caller then runs [`blk_service_queue`]).
pub(super) fn blk_mmio_write(dev: &mut VirtioBlk, off: u64, value: u32) -> bool {
    match off {
        0x014 => dev.device_features_sel = value,
        0x020 => {
            let w = dev.driver_features_sel.min(1) as usize;
            dev.driver_features[w] = value;
        }
        0x024 => dev.driver_features_sel = value,
        0x030 => dev.queue_sel = value,
        0x038 => dev.queue_num = value,
        0x044 => dev.queue_ready = value,
        0x064 => dev.interrupt_status &= !value,
        0x070 => {
            dev.status = value;
            if value == 0 {
                let disk = core::mem::replace(&mut dev.disk, KappaBacking::from_image(&[]));
                *dev = VirtioBlk::with_backing(disk);
            }
        }
        0x080 => dev.desc_addr = (dev.desc_addr & !0xffff_ffff) | u64::from(value),
        0x084 => dev.desc_addr = (dev.desc_addr & 0xffff_ffff) | (u64::from(value) << 32),
        0x090 => dev.avail_addr = (dev.avail_addr & !0xffff_ffff) | u64::from(value),
        0x094 => dev.avail_addr = (dev.avail_addr & 0xffff_ffff) | (u64::from(value) << 32),
        0x0a0 => dev.used_addr = (dev.used_addr & !0xffff_ffff) | u64::from(value),
        0x0a4 => dev.used_addr = (dev.used_addr & 0xffff_ffff) | (u64::from(value) << 32),
        0x050 => return true, // QueueNotify
        _ => {}
    }
    false
}

/// Process every newly-available request in the block device's virtqueue against
/// the κ-disk (VirtIO v1.2 §5.2). Returns `true` if the device IRQ must be
/// raised (a used-ring update the driver should be notified of).
pub(super) fn blk_service_queue(mem: &mut GuestRam, dev: &mut VirtioBlk) -> bool {
    let qsz = dev.queue_num as u16;
    if dev.queue_ready == 0 || qsz == 0 {
        return false;
    }
    let avail_idx = mem.rd16(dev.avail_addr + 2);
    let mut raised = false;
    while dev.last_avail != avail_idx {
        let slot = dev.last_avail % qsz;
        let head = mem.rd16(dev.avail_addr + 4 + 2 * u64::from(slot));
        let written = blk_service_chain(mem, dev, head);
        let used_idx = mem.rd16(dev.used_addr + 2);
        let ring = dev.used_addr + 4 + 8 * u64::from(used_idx % qsz);
        mem.wr32(ring, u32::from(head));
        mem.wr32(ring + 4, written);
        mem.wr16(dev.used_addr + 2, used_idx.wrapping_add(1));
        dev.last_avail = dev.last_avail.wrapping_add(1);
        dev.interrupt_status |= 1;
        raised = true;
    }
    raised
}

/// Service one block request descriptor chain (header → data → status) against
/// the κ-disk; returns the used-ring length (bytes written + the status byte).
fn blk_service_chain(mem: &mut GuestRam, dev: &mut VirtioBlk, head: u16) -> u32 {
    let chain = mem.collect_chain(dev.desc_addr, head, dev.queue_num as usize);
    if chain.is_empty() {
        return 0;
    }
    let (haddr, _, _) = chain[0];
    let req_type = mem.rd32(haddr);
    let sector = mem.rd64(haddr + 8);
    let status_desc = *chain.last().unwrap();
    let data = &chain[1..chain.len() - 1];

    const VIRTIO_BLK_T_IN: u32 = 0;
    const VIRTIO_BLK_T_OUT: u32 = 1;
    const VIRTIO_BLK_T_GET_ID: u32 = 8;
    const VIRTIO_BLK_S_OK: u8 = 0;
    const VIRTIO_BLK_S_IOERR: u8 = 1;

    let mut written = 0u32;
    let mut disk_off = (sector * 512) as usize;
    let mut status = VIRTIO_BLK_S_OK;
    match req_type {
        VIRTIO_BLK_T_IN => {
            for (addr, len, _flags) in data {
                let n = *len as usize;
                if disk_off + n > dev.disk.len() {
                    status = VIRTIO_BLK_S_IOERR;
                    break;
                }
                let mut buf = vec![0u8; n];
                dev.disk.read_into(disk_off, &mut buf);
                mem.write_bytes(*addr, &buf);
                disk_off += n;
                written += *len;
            }
        }
        VIRTIO_BLK_T_OUT => {
            for (addr, len, _flags) in data {
                let n = *len as usize;
                if disk_off + n > dev.disk.len() {
                    status = VIRTIO_BLK_S_IOERR;
                    break;
                }
                let mut buf = vec![0u8; n];
                mem.read_bytes(*addr, &mut buf);
                dev.disk.write_from(disk_off, &buf);
                disk_off += n;
            }
        }
        VIRTIO_BLK_T_GET_ID => {
            const ID: &[u8] = b"holospaces-rootfs";
            for (addr, len, _flags) in data {
                for i in 0..*len as usize {
                    mem.wr8(addr + i as u64, ID.get(i).copied().unwrap_or(0));
                }
                written += *len;
            }
        }
        _ => {}
    }
    let _ = VIRTQ_DESC_F_WRITE;
    mem.wr8(status_desc.0, status);
    written + 1
}
