//! `CC-46` — the **shared device bus** serves `virtio-9p`, `virtio-net` + the
//! userspace NAT, and the in-process guest bridge to **every** core, at arch
//! parity with the RISC-V machine (`CC-15`/`CC-16`/`CC-33`): the **AArch64** core
//! and the **x86-64** core both reach the one shared devbus.
//!
//! Law L4: the substrate's devices are *shared*, not per-ISA. The 9p/net/bridge
//! servicing lives in the core-agnostic [`emulator::devbus`]; the only per-ISA
//! difference is the MMIO transport (where RAM is based, the MMIO window, and
//! which interrupt controller latches the IRQ). This witness drives each
//! machine's own `virtio-mmio` slots — the same device path the executing CPU
//! routes its loads and stores through — acting as the guest's `virtio` driver:
//! it negotiates each queue, lays the split virtqueue in guest RAM, rings
//! `QueueNotify`, and reads the device's reply.
//!
//! The protocol authorities are the same as the RISC-V witnesses: the 9P2000.L
//! specification (`CC-15`), the OASIS VirtIO v1.2 `virtio-net` + the `10.0.2.0/24`
//! userspace NAT model (`CC-16`), and the in-process loopback bridge (ADR-020,
//! `CC-33`). No per-ISA device code is exercised — the assertions pass *because*
//! each core's transport reaches the one shared devbus.
//!
//! The driver logic is written once against the [`Mmio`] transport trait and run
//! against both cores; per-core entry points (`*_aarch64` / `*_x86_64`) keep the
//! failures attributable. These are **device-level** checks (no full kernel
//! boot): fast regression coverage that each core's MMIO transport reaches the
//! shared devbus, run in the default test set. They are *not* the `CC-46`
//! parity witness — that is the real arm64 boot in `cc46_realboot.rs`
//! (`CC-15`/`CC-16`/`CC-33` caliber), which a real kernel drives through its VFS
//! and TCP/IP stack.

use holospaces::emulator::aarch64::Cpu as Aarch64Cpu;
use holospaces::emulator::net::{ChannelEgress, Egress, RouterChannel};
use holospaces::emulator::x64::Cpu as X64Cpu;

/// The per-ISA `virtio-mmio` transport a witness drives a core through — the
/// device-driver hooks each core exposes (`vv_*`). The driver logic below is
/// written once against this trait; the only per-ISA differences are where RAM
/// is based and which MMIO window the `virtio` slots live in, both supplied here.
trait Mmio {
    fn ram_base() -> u64
    where
        Self: Sized;
    fn p9_base(&self) -> u64;
    fn net_base(&self) -> u64;
    fn mmio_w(&mut self, pa: u64, width: usize, value: u64);
    fn mmio_r(&mut self, pa: u64, width: usize) -> u64;
    fn ram_w(&mut self, pa: u64, bytes: &[u8]);
    fn ram_r(&self, pa: u64, len: usize) -> Vec<u8>;
    // The bridge surface (CC-33), shared by both cores.
    fn attach_net(&mut self, egress: Box<dyn Egress>);
    fn enable_loopback(&mut self) -> bool;
    fn dial_guest(&mut self, port: u16) -> Option<u32>;
    fn guest_send(&mut self, id: u32, data: &[u8]);
    fn guest_recv(&mut self, id: u32) -> Vec<u8>;
    fn guest_is_open(&self, id: u32) -> bool;
    fn guest_close(&mut self, id: u32);
    fn run(&mut self, steps: u64);
    // The workspace surface (CC-15) — the editor side holospaces observes.
    fn attach_workspace(&mut self, seed: &[(&str, &[u8])]);
    fn workspace_file(&self, name: &str) -> Option<Vec<u8>>;
}

impl Mmio for Aarch64Cpu {
    fn ram_base() -> u64 {
        0x4000_0000
    }
    fn p9_base(&self) -> u64 {
        Aarch64Cpu::vv_virtio_9p_base()
    }
    fn net_base(&self) -> u64 {
        Aarch64Cpu::vv_virtio_net_base()
    }
    fn mmio_w(&mut self, pa: u64, width: usize, value: u64) {
        self.vv_mmio_write(pa, width, value);
    }
    fn mmio_r(&mut self, pa: u64, width: usize) -> u64 {
        self.vv_mmio_read(pa, width)
    }
    fn ram_w(&mut self, pa: u64, bytes: &[u8]) {
        self.vv_ram_write(pa, bytes);
    }
    fn ram_r(&self, pa: u64, len: usize) -> Vec<u8> {
        self.vv_ram_read(pa, len)
    }
    fn attach_net(&mut self, egress: Box<dyn Egress>) {
        Aarch64Cpu::attach_net(self, egress);
    }
    fn enable_loopback(&mut self) -> bool {
        Aarch64Cpu::enable_loopback(self)
    }
    fn dial_guest(&mut self, port: u16) -> Option<u32> {
        Aarch64Cpu::dial_guest(self, port)
    }
    fn guest_send(&mut self, id: u32, data: &[u8]) {
        Aarch64Cpu::guest_send(self, id, data);
    }
    fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        Aarch64Cpu::guest_recv(self, id)
    }
    fn guest_is_open(&self, id: u32) -> bool {
        Aarch64Cpu::guest_is_open(self, id)
    }
    fn guest_close(&mut self, id: u32) {
        Aarch64Cpu::guest_close(self, id);
    }
    fn run(&mut self, steps: u64) {
        let _ = Aarch64Cpu::run(self, steps);
    }
    fn attach_workspace(&mut self, seed: &[(&str, &[u8])]) {
        Aarch64Cpu::attach_workspace(self, seed);
    }
    fn workspace_file(&self, name: &str) -> Option<Vec<u8>> {
        Aarch64Cpu::workspace_file(self, name).map(<[u8]>::to_vec)
    }
}

impl Mmio for X64Cpu {
    fn ram_base() -> u64 {
        0x0
    }
    fn p9_base(&self) -> u64 {
        X64Cpu::vv_virtio_9p_base()
    }
    fn net_base(&self) -> u64 {
        X64Cpu::vv_virtio_net_base()
    }
    fn mmio_w(&mut self, pa: u64, width: usize, value: u64) {
        self.vv_mmio_write(pa, width, value);
    }
    fn mmio_r(&mut self, pa: u64, width: usize) -> u64 {
        self.vv_mmio_read(pa, width)
    }
    fn ram_w(&mut self, pa: u64, bytes: &[u8]) {
        self.vv_ram_write(pa, bytes);
    }
    fn ram_r(&self, pa: u64, len: usize) -> Vec<u8> {
        self.vv_ram_read(pa, len)
    }
    fn attach_net(&mut self, egress: Box<dyn Egress>) {
        X64Cpu::attach_net(self, egress);
    }
    fn enable_loopback(&mut self) -> bool {
        X64Cpu::enable_loopback(self)
    }
    fn dial_guest(&mut self, port: u16) -> Option<u32> {
        X64Cpu::dial_guest(self, port)
    }
    fn guest_send(&mut self, id: u32, data: &[u8]) {
        X64Cpu::guest_send(self, id, data);
    }
    fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        X64Cpu::guest_recv(self, id)
    }
    fn guest_is_open(&self, id: u32) -> bool {
        X64Cpu::guest_is_open(self, id)
    }
    fn guest_close(&mut self, id: u32) {
        X64Cpu::guest_close(self, id);
    }
    fn run(&mut self, steps: u64) {
        let _ = X64Cpu::run(self, steps);
    }
    fn attach_workspace(&mut self, seed: &[(&str, &[u8])]) {
        X64Cpu::attach_workspace(self, seed);
    }
    fn workspace_file(&self, name: &str) -> Option<Vec<u8>> {
        X64Cpu::workspace_file(self, name).map(<[u8]>::to_vec)
    }
}

/// Build a fresh AArch64 machine in system mode (a minimal Linux boot context;
/// the device-level witness drives only its `virtio-mmio` slots).
fn aarch64() -> Aarch64Cpu {
    Aarch64Cpu::boot_linux(128 * 1024 * 1024, &[], "console=ttyAMA0")
}

/// Build a fresh x86-64 machine with system devices wired (the boot core; the
/// device-level witness drives only its `virtio-mmio` slots, no kernel boot).
fn x86_64() -> X64Cpu {
    X64Cpu::new(128 * 1024 * 1024)
}

// `virtio-mmio` register offsets (OASIS VirtIO v1.2 §4.2.2), the modern transport.
const R_MAGIC: u64 = 0x000;
const R_VERSION: u64 = 0x004;
const R_DEVICE_ID: u64 = 0x008;
const R_DEVICE_FEATURES_SEL: u64 = 0x014;
const R_DRIVER_FEATURES: u64 = 0x020;
const R_DRIVER_FEATURES_SEL: u64 = 0x024;
const R_QUEUE_SEL: u64 = 0x030;
const R_QUEUE_NUM: u64 = 0x038;
const R_QUEUE_READY: u64 = 0x044;
const R_QUEUE_NOTIFY: u64 = 0x050;
const R_STATUS: u64 = 0x070;
const R_DESC_LOW: u64 = 0x080;
const R_DESC_HIGH: u64 = 0x084;
const R_AVAIL_LOW: u64 = 0x090;
const R_AVAIL_HIGH: u64 = 0x094;
const R_USED_LOW: u64 = 0x0a0;
const R_USED_HIGH: u64 = 0x0a4;

// Split-virtqueue descriptor flags.
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const Q_SIZE: u32 = 8;

/// A minimal split-virtqueue laid out in guest RAM, driven by the witness as a
/// `virtio` driver would: a descriptor table, an available ring, and a used ring
/// at fixed guest-physical offsets, plus a scratch buffer arena.
struct Vq {
    desc: u64,
    avail: u64,
    used: u64,
    scratch_next: u64,
    avail_idx: u16,
}

impl Vq {
    /// Lay the queue out in a free region of RAM above where any image loads.
    fn new(base: u64) -> Self {
        Vq {
            desc: base,
            avail: base + 0x1000,
            used: base + 0x2000,
            scratch_next: base + 0x3000,
            avail_idx: 0,
        }
    }

    /// Reserve `len` bytes of scratch (8-byte aligned), returning its address.
    fn alloc(&mut self, len: usize) -> u64 {
        let a = self.scratch_next;
        self.scratch_next += (len as u64 + 7) & !7;
        a
    }

    /// Program a device's queue registers to point at this virtqueue, and make it
    /// ready (the driver's queue-setup handshake).
    fn program(&self, cpu: &mut dyn Mmio, dev_base: u64) {
        cpu.mmio_w(dev_base + R_QUEUE_SEL, 4, 0);
        cpu.mmio_w(dev_base + R_QUEUE_NUM, 4, u64::from(Q_SIZE));
        cpu.mmio_w(dev_base + R_DESC_LOW, 4, self.desc & 0xffff_ffff);
        cpu.mmio_w(dev_base + R_DESC_HIGH, 4, self.desc >> 32);
        cpu.mmio_w(dev_base + R_AVAIL_LOW, 4, self.avail & 0xffff_ffff);
        cpu.mmio_w(dev_base + R_AVAIL_HIGH, 4, self.avail >> 32);
        cpu.mmio_w(dev_base + R_USED_LOW, 4, self.used & 0xffff_ffff);
        cpu.mmio_w(dev_base + R_USED_HIGH, 4, self.used >> 32);
        cpu.mmio_w(dev_base + R_QUEUE_READY, 4, 1);
    }

    /// Write one descriptor `i` (16 bytes: addr[8] len[4] flags[2] next[2]).
    fn set_desc(&self, cpu: &mut dyn Mmio, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let d = self.desc + 16 * u64::from(i);
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&addr.to_le_bytes());
        buf[8..12].copy_from_slice(&len.to_le_bytes());
        buf[12..14].copy_from_slice(&flags.to_le_bytes());
        buf[14..16].copy_from_slice(&next.to_le_bytes());
        cpu.ram_w(d, &buf);
    }

    /// Publish descriptor-chain `head` on the available ring and bump the index.
    fn publish(&mut self, cpu: &mut dyn Mmio, head: u16) {
        let slot = self.avail_idx % (Q_SIZE as u16);
        cpu.ram_w(self.avail + 4 + 2 * u64::from(slot), &head.to_le_bytes());
        self.avail_idx = self.avail_idx.wrapping_add(1);
        cpu.ram_w(self.avail + 2, &self.avail_idx.to_le_bytes());
    }

    /// The device's used-ring index (how many chains it has completed).
    fn used_idx(&self, cpu: &dyn Mmio) -> u16 {
        let b = cpu.ram_r(self.used + 2, 2);
        u16::from_le_bytes([b[0], b[1]])
    }

    /// The used-ring length the device reported for the most recent completion.
    fn last_used_len(&self, cpu: &dyn Mmio) -> u32 {
        let idx = self.used_idx(cpu).wrapping_sub(1) % (Q_SIZE as u16);
        let ring = self.used + 4 + 8 * u64::from(idx);
        let b = cpu.ram_r(ring + 4, 4);
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
}

/// Bring a `virtio-mmio` device through the status handshake
/// (ACKNOWLEDGE→DRIVER→FEATURES_OK→DRIVER_OK) and accept `VIRTIO_F_VERSION_1`.
fn device_init(cpu: &mut dyn Mmio, dev_base: u64) {
    // The transport must be live (magic "virt", modern version).
    assert_eq!(cpu.mmio_r(dev_base + R_MAGIC, 4), 0x7472_6976);
    assert_eq!(cpu.mmio_r(dev_base + R_VERSION, 4), 2);
    cpu.mmio_w(dev_base + R_STATUS, 4, 0); // reset
    cpu.mmio_w(dev_base + R_STATUS, 4, 1); // ACKNOWLEDGE
    cpu.mmio_w(dev_base + R_STATUS, 4, 1 | 2); // + DRIVER
                                               // Accept VIRTIO_F_VERSION_1 (feature bit 32 = word 1, bit 0).
    cpu.mmio_w(dev_base + R_DEVICE_FEATURES_SEL, 4, 1);
    cpu.mmio_w(dev_base + R_DRIVER_FEATURES_SEL, 4, 1);
    cpu.mmio_w(dev_base + R_DRIVER_FEATURES, 4, 1);
    cpu.mmio_w(dev_base + R_STATUS, 4, 1 | 2 | 8); // + FEATURES_OK
    cpu.mmio_w(dev_base + R_STATUS, 4, 1 | 2 | 8 | 4); // + DRIVER_OK
}

// ── a minimal 9P2000.L client (the same wire format the guest's v9fs speaks) ──

struct P9 {
    tag: u16,
}
impl P9 {
    fn new() -> Self {
        P9 { tag: 1 }
    }
    fn next_tag(&mut self) -> u16 {
        let t = self.tag;
        self.tag = self.tag.wrapping_add(1);
        t
    }
    fn envelope(ttype: u8, tag: u16, body: &[u8]) -> Vec<u8> {
        let size = 7 + body.len() as u32;
        let mut m = Vec::with_capacity(size as usize);
        m.extend_from_slice(&size.to_le_bytes());
        m.push(ttype);
        m.extend_from_slice(&tag.to_le_bytes());
        m.extend_from_slice(body);
        m
    }
    fn pstr(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u16).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn tversion(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&65536u32.to_le_bytes()); // msize
        Self::pstr(&mut b, "9P2000.L");
        Self::envelope(100, self.next_tag(), &b)
    }
    fn tattach(&mut self, fid: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&fid.to_le_bytes());
        b.extend_from_slice(&u32::MAX.to_le_bytes()); // afid = NOFID
        Self::pstr(&mut b, "");
        Self::pstr(&mut b, "");
        b.extend_from_slice(&0u32.to_le_bytes()); // n_uname
        Self::envelope(104, self.next_tag(), &b)
    }
    fn twalk(&mut self, fid: u32, newfid: u32, name: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&fid.to_le_bytes());
        b.extend_from_slice(&newfid.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // nwname
        Self::pstr(&mut b, name);
        Self::envelope(110, self.next_tag(), &b)
    }
    fn tlopen(&mut self, fid: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&fid.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes()); // O_RDWR
        Self::envelope(12, self.next_tag(), &b)
    }
    fn tread(&mut self, fid: u32, offset: u64, count: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&fid.to_le_bytes());
        b.extend_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        Self::envelope(116, self.next_tag(), &b)
    }
    fn twrite(&mut self, fid: u32, offset: u64, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&fid.to_le_bytes());
        b.extend_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
        Self::envelope(118, self.next_tag(), &b)
    }
}

/// Submit one 9P T-message over a core's `virtio-9p` queue and return the
/// device's R-message (the leading readable descriptor carries the T-message;
/// the trailing writable descriptor receives the R-message). Drives the *real*
/// shared devbus through that core's MMIO transport.
fn p9_rpc(cpu: &mut dyn Mmio, dev_base: u64, vq: &mut Vq, tmsg: &[u8]) -> Vec<u8> {
    let tbuf = vq.alloc(tmsg.len().max(8));
    cpu.ram_w(tbuf, tmsg);
    let rbuf = vq.alloc(8192);
    // Two-descriptor chain: [0] readable T-message → [1] writable R-message.
    vq.set_desc(cpu, 0, tbuf, tmsg.len() as u32, VIRTQ_DESC_F_NEXT, 1);
    vq.set_desc(cpu, 1, rbuf, 8192, VIRTQ_DESC_F_WRITE, 0);
    let before = vq.used_idx(cpu);
    vq.publish(cpu, 0);
    cpu.mmio_w(dev_base + R_QUEUE_NOTIFY, 4, 0);
    assert_eq!(
        vq.used_idx(cpu),
        before.wrapping_add(1),
        "the virtio-9p device serviced the chain (used ring advanced)"
    );
    let written = vq.last_used_len(cpu) as usize;
    cpu.ram_r(rbuf, written)
}

/// Parse the message type byte (offset 4) of a 9P reply.
fn rtype(msg: &[u8]) -> u8 {
    msg[4]
}

// ── the three CC-46 device-driver checks, written once against `Mmio` ─────────

/// A core mounts a 9p workspace and the editor and the OS share files — `CC-15`
/// parity, over the shared devbus, through that core's MMIO transport.
fn check_9p_workspace<C: Mmio>(mut cpu: C) {
    // holospaces seeds a file on the shared workspace (the editor side).
    let seeded = b"from-holospaces-9p-share-OK\n";
    cpu.attach_workspace(&[("from-holospaces.txt", seeded)]);

    let dev = cpu.p9_base();
    // The transport is live and identifies as the 9P device (id 9).
    device_init(&mut cpu, dev);
    assert_eq!(cpu.mmio_r(dev + R_DEVICE_ID, 4), 9, "virtio-9p device id");

    let mut vq = Vq::new(C::ram_base() + 0x0080_0000);
    vq.program(&mut cpu, dev);
    let mut p9 = P9::new();

    // Mount: Tversion → Tattach (root fid 1).
    let rv = p9_rpc(&mut cpu, dev, &mut vq, &p9.tversion());
    assert_eq!(rtype(&rv), 101, "Rversion");
    let ra = p9_rpc(&mut cpu, dev, &mut vq, &p9.tattach(1));
    assert_eq!(rtype(&ra), 105, "Rattach");

    // The OS reads the file holospaces seeded: Twalk → Tlopen → Tread.
    let rw = p9_rpc(
        &mut cpu,
        dev,
        &mut vq,
        &p9.twalk(1, 2, "from-holospaces.txt"),
    );
    assert_eq!(rtype(&rw), 111, "Rwalk");
    let ro = p9_rpc(&mut cpu, dev, &mut vq, &p9.tlopen(2));
    assert_eq!(rtype(&ro), 13, "Rlopen");
    let rd = p9_rpc(&mut cpu, dev, &mut vq, &p9.tread(2, 0, 4096));
    assert_eq!(rtype(&rd), 117, "Rread");
    // Rread body: count[4] then data.
    let count = u32::from_le_bytes([rd[7], rd[8], rd[9], rd[10]]) as usize;
    assert_eq!(
        &rd[11..11 + count],
        seeded,
        "the OS read the bytes holospaces seeded on the 9p workspace (CC-15 parity)"
    );

    // The OS writes the file back (Twrite); the editor observes the same content.
    // The write fully covers the seeded content (a partial Twrite at offset 0
    // would leave the file's old tail, per 9p semantics — not what we assert).
    let guest_bytes = b"written-by-the-guest-over-9pXX";
    assert!(
        guest_bytes.len() >= seeded.len(),
        "the guest write covers the seeded content"
    );
    let rwr = p9_rpc(&mut cpu, dev, &mut vq, &p9.twrite(2, 0, guest_bytes));
    assert_eq!(rtype(&rwr), 119, "Rwrite");
    assert_eq!(
        cpu.workspace_file("from-holospaces.txt"),
        Some(guest_bytes.to_vec()),
        "the editor and the OS share the workspace file (one content, Law L1)"
    );
}

/// A core initiates (opens) an outbound TCP connection through the userspace
/// NAT — `CC-16` parity, over the shared devbus, through that core's MMIO
/// transport. The witness asserts only the egress OPEN that carries the
/// destination; a *completed* flow (SYN-ACK/established/data/reply) is the real
/// boot in `cc46_realboot.rs`.
fn check_nat_outbound<C: Mmio>(mut cpu: C) {
    let (egress, router): (ChannelEgress, RouterChannel) = ChannelEgress::new();
    cpu.attach_net(Box::new(egress));

    let dev = cpu.net_base();
    device_init(&mut cpu, dev);
    assert_eq!(cpu.mmio_r(dev + R_DEVICE_ID, 4), 1, "virtio-net device id");
    // The device reports its MAC in config space (VIRTIO_NET_F_MAC).
    let mac0 = cpu.mmio_r(dev + 0x100, 1);
    assert_eq!(mac0, 0x52, "virtio_net_config.mac[0]");

    // Set up the transmit queue (index 1).
    let mut tx = Vq::new(C::ram_base() + 0x0080_0000);
    cpu.mmio_w(dev + R_QUEUE_SEL, 4, 1);
    cpu.mmio_w(dev + R_QUEUE_NUM, 4, u64::from(Q_SIZE));
    cpu.mmio_w(dev + R_DESC_LOW, 4, tx.desc & 0xffff_ffff);
    cpu.mmio_w(dev + R_DESC_HIGH, 4, tx.desc >> 32);
    cpu.mmio_w(dev + R_AVAIL_LOW, 4, tx.avail & 0xffff_ffff);
    cpu.mmio_w(dev + R_AVAIL_HIGH, 4, tx.avail >> 32);
    cpu.mmio_w(dev + R_USED_LOW, 4, tx.used & 0xffff_ffff);
    cpu.mmio_w(dev + R_USED_HIGH, 4, tx.used >> 32);
    cpu.mmio_w(dev + R_QUEUE_READY, 4, 1);

    // Build a guest TCP SYN to 93.184.216.34:80 (an external host) — a real
    // Ethernet + IPv4 + TCP frame, prefixed with the 12-byte virtio_net_hdr.
    let dst_ip = [93u8, 184, 216, 34];
    let frame = tcp_syn_frame(dst_ip, 80);
    let mut buf = vec![0u8; 12]; // virtio_net_hdr_v1 (zeroed)
    buf.extend_from_slice(&frame);
    let fbuf = tx.alloc(buf.len());
    cpu.ram_w(fbuf, &buf);
    tx.set_desc(&mut cpu, 0, fbuf, buf.len() as u32, 0, 0);
    tx.publish(&mut cpu, 0);
    cpu.mmio_w(dev + R_QUEUE_NOTIFY, 4, 1); // notify the TX queue

    // The NAT terminated the guest's link layer and opened an outbound
    // connection toward the external host over the egress — observable as the
    // egress OPEN frame (op 0x01, the destination IP) the router would carry.
    let frames = router.drain_outbound();
    let opened = frames
        .iter()
        .find(|f| f.len() >= 11 && f[0] == 0x01 && f[5..9] == dst_ip);
    assert!(
        opened.is_some(),
        "the guest's TCP SYN drove the NAT to emit an outbound OPEN toward {dst_ip:?} \
         (CC-16 parity); egress frames: {frames:?}"
    );
}

/// A core exposes the guest-bridge API — `CC-33` parity. The bridge
/// (dial/send/recv/close) is the core-agnostic loopback surface over the shared
/// NAT; here it is enabled on the core's net device, a dial issues a connection
/// id, and the id reports open after a local send. No delivery to a guest
/// listener is asserted (there is no guest in this device-level witness).
fn check_guest_bridge<C: Mmio>(mut cpu: C) {
    // The bridge requires a network device (it shares the NAT's ingress path).
    assert!(
        !cpu.enable_loopback(),
        "no bridge without a network device attached"
    );
    let (egress, _router) = ChannelEgress::new();
    cpu.attach_net(Box::new(egress));
    assert!(
        cpu.enable_loopback(),
        "the net device exposes the in-process loopback bridge (CC-33 parity)"
    );

    // The workbench dials through the bridge API; it issues a connection id and,
    // after a local send, the id reports open (no delivery to a guest asserted).
    let id = cpu
        .dial_guest(8080)
        .expect("dial_guest returns a connection id once the loopback is enabled");
    cpu.guest_send(id, b"GET / HTTP/1.0\r\n\r\n");
    assert!(
        cpu.guest_is_open(id),
        "the bridge API reports the dialed connection open after a local send (CC-33 parity)"
    );
    // Exercise the rest of the API surface (pump, recv, close) for coverage.
    cpu.run(2000);
    let _ = cpu.guest_recv(id);
    cpu.guest_close(id);
}

// ── AArch64 entry points (CC-15/CC-16/CC-33 parity, over the shared devbus) ───

#[test]
fn the_aarch64_core_mounts_a_9p_workspace_over_the_shared_devbus() {
    check_9p_workspace(aarch64());
}

#[test]
fn the_aarch64_core_opens_an_outbound_tcp_connection_over_the_shared_devbus() {
    check_nat_outbound(aarch64());
}

#[test]
fn the_aarch64_core_exposes_the_guest_bridge_api() {
    check_guest_bridge(aarch64());
}

// ── x86-64 entry points — the same three assertions, on the third ISA core ────

#[test]
fn the_x86_64_core_mounts_a_9p_workspace_over_the_shared_devbus() {
    check_9p_workspace(x86_64());
}

#[test]
fn the_x86_64_core_opens_an_outbound_tcp_connection_over_the_shared_devbus() {
    check_nat_outbound(x86_64());
}

#[test]
fn the_x86_64_core_exposes_the_guest_bridge_api() {
    check_guest_bridge(x86_64());
}

// ── a hand-built Ethernet + IPv4 + TCP SYN frame (the differential oracle is
//    the same userspace NAT model qemu's user networking implements) ──────────

/// The guest's IP/MAC the NAT expects (the `10.0.2.0/24` model).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

fn tcp_syn_frame(dst_ip: [u8; 4], dst_port: u16) -> Vec<u8> {
    // TCP header (20 bytes, no options), SYN.
    let src_port: u16 = 50000;
    let seq: u32 = 0x1000;
    let mut tcp = Vec::new();
    tcp.extend_from_slice(&src_port.to_be_bytes());
    tcp.extend_from_slice(&dst_port.to_be_bytes());
    tcp.extend_from_slice(&seq.to_be_bytes());
    tcp.extend_from_slice(&0u32.to_be_bytes()); // ack
    tcp.push(5 << 4); // data offset = 5 words
    tcp.push(0x02); // SYN
    tcp.extend_from_slice(&64240u16.to_be_bytes()); // window
    tcp.extend_from_slice(&0u16.to_be_bytes()); // checksum (filled below)
    tcp.extend_from_slice(&0u16.to_be_bytes()); // urgent

    // IPv4 header (20 bytes).
    let total_len = (20 + tcp.len()) as u16;
    let mut ip = Vec::new();
    ip.push(0x45); // version 4, IHL 5
    ip.push(0); // DSCP/ECN
    ip.extend_from_slice(&total_len.to_be_bytes());
    ip.extend_from_slice(&0u16.to_be_bytes()); // id
    ip.extend_from_slice(&0x4000u16.to_be_bytes()); // flags = DF
    ip.push(64); // TTL
    ip.push(6); // protocol = TCP
    ip.extend_from_slice(&0u16.to_be_bytes()); // header checksum (filled below)
    ip.extend_from_slice(&GUEST_IP);
    ip.extend_from_slice(&dst_ip);
    let ipck = checksum(&ip);
    ip[10..12].copy_from_slice(&ipck.to_be_bytes());

    // TCP checksum over the pseudo-header + TCP segment.
    let mut pseudo = Vec::new();
    pseudo.extend_from_slice(&GUEST_IP);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(6);
    pseudo.extend_from_slice(&(tcp.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(&tcp);
    let tcpck = checksum(&pseudo);
    tcp[16..18].copy_from_slice(&tcpck.to_be_bytes());

    // Ethernet header: dst = gateway MAC, src = guest MAC, ethertype IPv4.
    let mut eth = Vec::new();
    eth.extend_from_slice(&GW_MAC);
    eth.extend_from_slice(&GUEST_MAC);
    eth.extend_from_slice(&0x0800u16.to_be_bytes());
    eth.extend_from_slice(&ip);
    eth.extend_from_slice(&tcp);
    eth
}

/// The 16-bit one's-complement checksum (IPv4/TCP).
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u32::from(u16::from_be_bytes([data[i], data[i + 1]]));
        i += 2;
    }
    if i < data.len() {
        sum += u32::from(data[i]) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
