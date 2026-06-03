//! **System emulator** — a real RISC-V (RV64GC: IMAFDC + Zicsr) machine, the core of
//! the arbitrary-OS execution surface (ADR-009, arc42 chapter 9).
//!
//! holospaces hosts *arbitrary* operating systems by running a real system
//! emulator as a κ-addressed Wasm codemodule over the substrate host ABI: the
//! guest's disk is the [κ-disk](crate::disk) (`CC-7`), its image is imported by κ
//! (`CC-8`), its console and state are hologram channels and κ snapshots. This
//! module is the emulator *core* — a faithful RISC-V interpreter — which the
//! codemodule wraps. It is `no_std` + `alloc`, so it compiles into the Wasm
//! container that runs on hologram's `wasmi`/Wasmtime engine (the same engine
//! that boots a userland, `CC-6`).
//!
//! The core is verified conformance-first against the
//! [RISC-V](https://riscv.org/technical/specifications/) ISA as its external
//! authority (`CC-9`, arc42 chapter 10): it passes the **official
//! [riscv-tests](https://github.com/riscv-software-src/riscv-tests) conformance
//! suite** — the same suite real hardware and QEMU are validated against. It
//! implements the base integer set, integer multiply/divide (M), atomics (A),
//! single/double floating point (F/D — correctly rounded with the IEEE-754 flags
//! and rounding modes, on the libm foundation hologram's float kernels use), the
//! compressed encoding (C), the control/status registers (Zicsr), and
//! trap handling across privilege levels — machine and supervisor mode with
//! delegation (`ecall`/`ebreak` exceptions → `mtvec`/`stvec`, `mcause`/`scause`,
//! `mret`/`sret`), and **Sv39/Sv48/Sv57 paging** (the page-table walk with accessed/dirty
//! bits and U/SUM/MXR permissions), and **interrupts** (the CLINT memory-mapped
//! timer, `mip`/`mie` with `mideleg`, vectored `mtvec`) — so the official
//! machine- and supervisor-mode tests (including supervisor paging) run
//! unmodified, and a kernel receives its scheduler tick. In firmware mode
//! ([`Emulator::enable_sbi`]) the emulator is the M-mode SEE: it services a
//! supervisor's **SBI** calls (the RISC-V SBI specification — console, timer,
//! system reset) so a real S-mode OS kernel boots over it. A flat program may
//! instead use the `ecall` host boundary (console `write` / `exit`) when it
//! installs no trap vector.

use alloc::collections::BTreeMap;
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{boxed::Box, vec, vec::Vec};
use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{KappaLabel71, KappaStore};

pub mod net;

/// The **AArch64 (ARMv8-A) core** — the system emulator's second ISA target
/// (ADR-021). The A64 integer instruction set, then (CC-36) the EL0/EL1
/// privileged model + VMSAv8-64 paging + the ARM `virt` platform, reusing this
/// module's `virtio`/9p/net/κ-disk device substrate unchanged. Conformance:
/// `CC-35`/`CC-36`/`CC-37`.
pub mod aarch64;

/// The shared virtio-mmio device bus: the substrate-backed `virtio` devices and
/// their split-virtqueue servicing, used by **both** the RISC-V and AArch64
/// machines (one κ-disk/9p/NAT implementation, two thin MMIO transports).
mod devbus;

/// The instruction-set architecture a holospace's guest runs (ADR-021). The
/// system emulator is one of two ISA targets — the RISC-V `RV64GC` core (this
/// module, `CC-9`) or the AArch64 `ARMv8-A` core ([`aarch64`], `CC-35`/`CC-36`).
///
/// An architecture is **fixed at provisioning**: the OCI image
/// (`platform.architecture`), the guest kernel, and the machine's device tree
/// are all ISA-specific, so it is selected in the Platform Manager *before* a
/// devcontainer is created (`CC-37`) and cannot change afterward — while every
/// other holospace parameter stays reconfigurable over the substrate (`CC-28`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Arch {
    /// 64-bit RISC-V (`RV64GC`) — the default target (`CC-9`).
    #[default]
    Riscv64,
    /// 64-bit ARM (`AArch64`, ARMv8-A) — `CC-35`/`CC-36`/`CC-37`.
    Aarch64,
}

impl Arch {
    /// The OCI image `platform.architecture` this ISA selects from a multi-arch
    /// index (`linux/<arch>`), matched at the import boundary (`CC-10`/`CC-20`).
    #[must_use]
    pub fn oci_arch(self) -> &'static str {
        match self {
            Arch::Riscv64 => "riscv64",
            Arch::Aarch64 => "arm64",
        }
    }

    /// The stable identifier used in holospace configs and the Manager UI.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Arch::Riscv64 => "riscv64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// Parse an architecture id (a Manager selection or a config field); `None`
    /// for an unknown id. Accepts both the canonical id and the OCI spelling.
    #[must_use]
    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "riscv64" | "rv64" | "rv64gc" => Some(Arch::Riscv64),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    /// A human label for the Manager console's architecture selector.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Arch::Riscv64 => "RISC-V (RV64GC)",
            Arch::Aarch64 => "ARM (AArch64)",
        }
    }

    /// Every architecture the Manager offers — the selection list, in default
    /// order (the default architecture first).
    #[must_use]
    pub fn all() -> &'static [Arch] {
        &[Arch::Riscv64, Arch::Aarch64]
    }
}

/// The Linux RISC-V syscall numbers the emulator's `ecall` boundary recognises
/// (a guest passes the number in `a7`). A real statically-linked binary that
/// only writes and exits runs unmodified — the path toward "a real binary's
/// output matches the native run".
mod syscall {
    pub const WRITE: u64 = 64;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
}

/// The CSR numbers the trap architecture and supervisor mode read and write.
mod csr {
    // Floating point (F/D): the accrued exception flags, rounding mode, and the
    // combined control/status register (RISC-V Unprivileged ISA §11.2).
    pub const FFLAGS: u32 = 0x001;
    pub const FRM: u32 = 0x002;
    pub const FCSR: u32 = 0x003;
    // Supervisor.
    pub const SSTATUS: u32 = 0x100;
    pub const SIE: u32 = 0x104;
    pub const STVEC: u32 = 0x105;
    pub const SEPC: u32 = 0x141;
    pub const SCAUSE: u32 = 0x142;
    pub const STVAL: u32 = 0x143;
    pub const SIP: u32 = 0x144;
    // Machine.
    pub const MSTATUS: u32 = 0x300;
    pub const MISA: u32 = 0x301;
    pub const MEDELEG: u32 = 0x302;
    pub const MIDELEG: u32 = 0x303;
    pub const MIE: u32 = 0x304;
    pub const MTVEC: u32 = 0x305;
    pub const MEPC: u32 = 0x341;
    pub const MCAUSE: u32 = 0x342;
    pub const MTVAL: u32 = 0x343;
    pub const MIP: u32 = 0x344;

    /// Interrupt-pending/enable bit positions (RISC-V Privileged ISA §3.1.9):
    /// supervisor/machine software (1/3), timer (5/7), external (9/11).
    pub const SSIP: u32 = 1;
    pub const MSIP: u32 = 3;
    pub const STIP: u32 = 5;
    pub const MTIP: u32 = 7;
    pub const SEIP: u32 = 9;
    pub const MEIP: u32 = 11;

    /// User-mode read-only shadow counters (RISC-V Unprivileged ISA §10.1):
    /// `time` mirrors the CLINT `mtime` (the kernel's `rdtime` clocksource);
    /// `cycle`/`instret` mirror it too (a monotonic free-running counter).
    pub const CYCLE: u32 = 0xc00;
    pub const TIME: u32 = 0xc01;
    pub const INSTRET: u32 = 0xc02;

    /// The debug trigger module (Sdtrig) CSRs — optional. This hart implements no
    /// triggers, so they read as 0 and ignore writes (RISC-V Debug spec: a
    /// no-trigger hart hardwires `tselect`/`tdata*`; software detects the absence
    /// and skips — the `breakpoint` conformance test does exactly that).
    pub const TSELECT: u32 = 0x7a0;
    pub const TDATA1: u32 = 0x7a1;
    pub const TDATA2: u32 = 0x7a2;
    pub const TDATA3: u32 = 0x7a3;
    pub const TINFO: u32 = 0x7a4;
    pub const TCONTROL: u32 = 0x7a5;

    /// `mstatus.UXL`/`SXL` (bits 33:32 / 35:34) — the U-/S-mode XLEN, a WARL field
    /// fixed at 2 (XLEN = 64) on this RV64 hart (RISC-V Privileged ISA §3.1.6.3).
    pub const XLEN_MASK: u64 = (3 << 32) | (3 << 34);
    pub const XLEN_64: u64 = (2 << 32) | (2 << 34);
    /// `sstatus.UXL` (bits 33:32) — the U-mode XLEN view, fixed at 2.
    pub const UXL_64: u64 = 2 << 32;

    /// The `sstatus` view of `mstatus` (the S-mode-visible bits): SIE, SPIE, SPP,
    /// FS, SUM, MXR (RISC-V Privileged ISA §4.1.1).
    pub const SSTATUS_MASK: u64 =
        (1 << 1) | (1 << 5) | (1 << 8) | (3 << 13) | (1 << 18) | (1 << 19);
    /// The `sie`/`sip` view of `mie`/`mip`: the S-mode interrupt bits.
    pub const S_INT_MASK: u64 = (1 << 1) | (1 << 5) | (1 << 9);
}

/// Privilege levels (RISC-V Privileged ISA): machine, supervisor, user.
const PRIV_M: u8 = 3;
const PRIV_S: u8 = 1;

/// The trap entry PC for a trap vector (`mtvec`/`stvec`): the base, or — in
/// vectored mode (low bit 1), for an interrupt — `base + 4*code`.
fn trap_vector(tvec: u64, is_interrupt: bool, code: u64) -> u64 {
    let base = tvec & !3;
    if is_interrupt && tvec & 1 == 1 {
        base + 4 * code
    } else {
        base
    }
}

/// Why the machine stopped stepping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The guest called `exit`/`exit_group` with this status code.
    Exit(u64),
    /// An instruction could not be executed (unimplemented opcode or a fault).
    Trap(Trap),
    /// The step budget was exhausted before the guest exited (a liveness bound,
    /// not a guest fault — the caller decides whether to continue).
    OutOfBudget,
}

/// A processor trap — an instruction the core cannot (yet) execute, or a fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trap {
    /// An opcode/funct combination the core does not implement.
    IllegalInstruction(u32),
    /// A load/store outside guest RAM.
    AccessFault(u64),
    /// An `ecall` whose `a7` is not a recognised syscall.
    UnknownSyscall(u64),
    /// `ebreak`.
    Breakpoint,
    /// A page fault (Sv39/Sv48/Sv57): `cause` is the RISC-V exception code (12 fetch / 13
    /// load / 15 store), `addr` the faulting virtual address.
    PageFault {
        /// The RISC-V page-fault exception code (12/13/15).
        cause: u64,
        /// The faulting virtual address.
        addr: u64,
    },
}

/// The kind of guest memory access (selects the page-table permission bit and
/// the page-fault cause).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    Fetch,
    Load,
    Store,
}

/// A RISC-V hart (hardware thread): 32 integer registers, 32 floating-point
/// registers (the F/D extension, stored as raw bits — `f32` values are
/// NaN-boxed in the high half), and a program counter.
#[derive(Clone)]
struct Hart {
    x: [u64; 32],
    f: [u64; 32],
    pc: u64,
}

/// A single-hart RV64GC RISC-V machine over a flat little-endian RAM, with an
/// `ecall` console (the `write` syscall appends to [`Emulator::console`]).
///
/// RAM is mapped at the machine's `base` address; a flat guest image is loaded
/// there and the reset PC is `base`. The machine is deterministic — identical
/// image + identical input yield identical console output and identical final
/// state, so its κ snapshot is reproducible (Law L1/L5; `CC-9`).
pub struct Emulator {
    hart: Hart,
    ram: Vec<u8>,
    base: u64,
    console: Vec<u8>,
    /// Pending console *input* (the bytes a driver fed to the machine's terminal,
    /// delivered to the guest through the SBI console; `in_cursor` is the next
    /// unread byte). The terminal-input channel of a workspace projection (CC-11).
    console_in: Vec<u8>,
    in_cursor: usize,
    /// Control and status registers (Zicsr) — a flat backing store; the WARL and
    /// read-only semantics (`mstatus.UXL`/`SXL`, `misa`, the debug triggers, …)
    /// are enforced at the `csr_read`/`csr_write` boundary.
    csrs: BTreeMap<u32, u64>,
    /// The LR/SC reservation address (A extension, single hart).
    reservation: Option<u64>,
    /// The current privilege level (M/S/U) — starts in machine mode.
    priv_level: u8,
    /// The HTIF `tohost` address, if set — a store there signals exit (the
    /// riscv-tests / SBI console channel); otherwise `None` (flat programs).
    htif: Option<u64>,
    /// The CLINT timer (`mtime`) and its per-hart compare (`mtimecmp`) and
    /// software-interrupt latch (`msip`) — a memory-mapped timer the guest reads
    /// and arms to receive timer/software interrupts (RISC-V CLINT).
    mtime: u64,
    mtimecmp: u64,
    msip: bool,
    /// When set, the emulator acts as the M-mode firmware (SEE): a supervisor
    /// `ecall` is serviced as an SBI call (console / timer / shutdown) rather
    /// than trapping — the boot mode a real S-mode kernel runs under.
    provide_sbi: bool,
    /// The platform-level interrupt controller — routes device interrupts (the
    /// VirtIO block device) to the hart's external-interrupt line (`CC-14`).
    plic: Plic,
    /// The VirtIO block device, when a root filesystem disk is attached; `None`
    /// for a diskless machine (the `CC-9` boot path is unchanged).
    virtio: Option<VirtioBlk>,
    /// The VirtIO 9P device serving the shared workspace filesystem, when
    /// attached (`CC-15`); `None` otherwise.
    virtio9p: Option<Virtio9p>,
    /// The VirtIO network device + userspace TCP/IP NAT, when networking is
    /// attached (`CC-16`); `None` for an offline machine.
    virtionet: Option<VirtioNet>,
    /// The host side of the in-process loopback ingress (ADR-020, `CC-33`), when
    /// the workbench dials guest listeners over the substrate bridge; `None` until
    /// [`Emulator::enable_loopback`] attaches it.
    loopback: Option<net::LoopbackHandle>,
    /// A software TLB over [`Emulator::translate`]: a direct-mapped cache (per
    /// access class) of virtual-page → physical-frame translations, so a hot loop
    /// does not re-walk the page table on every fetch/load/store. Flushed (by
    /// bumping `tlb_gen`) on every change to the translation context — SFENCE.VMA,
    /// a `satp` write, a translation-relevant `mstatus`/`sstatus` change, and
    /// every privilege transition — so a hit is always valid for the current
    /// context. It is a pure cache, reconstructable from the page tables, so it is
    /// deliberately *not* part of the κ snapshot (and a resumed machine rebuilds
    /// it from a clean flush).
    tlb: Box<[[TlbEntry; TLB_SETS]; 3]>,
    /// The current TLB generation; an entry is live iff its `gen` matches.
    tlb_gen: u64,
}

/// The CLINT memory-mapped region (one hart): `msip` at +0, `mtimecmp` at
/// +0x4000, `mtime` at +0xBFF8. Public so the [machine](crate::machine) Boot
/// Orchestrator generates a device tree describing the same memory map.
pub(crate) const CLINT_BASE: u64 = 0x0200_0000;
const CLINT_END: u64 = 0x0201_0000;

/// The PLIC (platform-level interrupt controller) memory-mapped region — the
/// same base the QEMU `virt` machine uses, so the device tree and the
/// differential oracle line up (RISC-V PLIC specification). It routes device
/// interrupts (here, the VirtIO block device) to the hart's external-interrupt
/// line. Single hart → two contexts: 0 = M-mode, 1 = S-mode (the one a Linux
/// kernel drives).
pub(crate) const PLIC_BASE: u64 = 0x0c00_0000;
const PLIC_END: u64 = 0x1000_0000;
/// The VirtIO-MMIO transport region (one device), mirroring QEMU `virt`'s first
/// virtio-mmio slot at `0x1000_1000` with PLIC source 1.
pub(crate) const VIRTIO_BASE: u64 = 0x1000_1000;
pub(crate) const VIRTIO_END: u64 = 0x1000_2000;
/// The PLIC interrupt source number the VirtIO block device raises.
pub(crate) const VIRTIO_IRQ: u32 = 1;
/// The second `virtio-mmio` slot — the VirtIO 9P (shared workspace filesystem)
/// device, with PLIC source 2 (`CC-15`).
pub(crate) const VIRTIO9P_BASE: u64 = 0x1000_2000;
pub(crate) const VIRTIO9P_END: u64 = 0x1000_3000;
/// The PLIC interrupt source number the VirtIO 9P device raises.
pub(crate) const VIRTIO9P_IRQ: u32 = 2;
/// The `mount_tag` the guest selects the workspace 9P share by.
pub(crate) const WORKSPACE_TAG: &str = "hsworkspace";
/// The third `virtio-mmio` slot — the VirtIO **network** device (`CC-16`), with
/// PLIC source 3. The guest drives a real NIC over it; its frames terminate in
/// the userspace TCP/IP [NAT](net) and stream out over a pluggable egress.
pub(crate) const VIRTIONET_BASE: u64 = 0x1000_3000;
pub(crate) const VIRTIONET_END: u64 = 0x1000_4000;
/// The PLIC interrupt source number the VirtIO network device raises.
pub(crate) const VIRTIONET_IRQ: u32 = 3;
/// PLIC contexts for a single hart: M-mode (0) and S-mode (1).
const PLIC_CONTEXTS: usize = 2;

/// The top of the device-MMIO window: every memory-mapped device region above
/// lies strictly below this. A physical access at or above this address cannot
/// be a device, so the load/store fast path routes it straight to RAM without
/// the per-access device range checks (the booting kernel's RAM is at
/// `0x8000_0000`, far above the devices, so this is the overwhelmingly common
/// case). The `const` assertion below pins the invariant: if a device is ever
/// mapped at or above this address, the build fails rather than silently
/// mis-routing it to RAM.
const DEVICE_MMIO_END: u64 = VIRTIONET_END;
const _: () = assert!(
    CLINT_END <= DEVICE_MMIO_END
        && PLIC_END <= DEVICE_MMIO_END
        && VIRTIO_END <= DEVICE_MMIO_END
        && VIRTIO9P_END <= DEVICE_MMIO_END
        && VIRTIONET_END <= DEVICE_MMIO_END,
    "every device MMIO window must lie below DEVICE_MMIO_END for the RAM fast path to be correct",
);

/// Software-TLB geometry: a direct-mapped translation cache per access class
/// (fetch/load/store), `TLB_SETS` sets each. A power of two so the set index is
/// a mask. 256 sets per class covers a large working set while staying small.
const TLB_SETS: usize = 256;
const TLB_MASK: u64 = TLB_SETS as u64 - 1;

/// `mstatus` bits that change address translation — MPRV (17), SUM (18),
/// MXR (19), and MPP (12:11). A write that touches any of these invalidates the
/// TLB; one that does not (e.g. the FS dirty bits an FP op sets) does not, so the
/// hot path is not flushed for unrelated `mstatus` traffic.
const MSTATUS_XLATE_BITS: u64 = (1 << 17) | (1 << 18) | (1 << 19) | (3 << 11);

/// One software-TLB entry: a cached virtual-page → physical-frame translation.
/// Valid iff `gen` equals the emulator's current `tlb_gen` (a generation bump is
/// a whole-TLB flush — see [`Emulator::tlb_flush`]).
#[derive(Clone, Copy)]
struct TlbEntry {
    /// The virtual page number (`vaddr >> 12`) this entry translates.
    tag: u64,
    /// The physical 4 KiB frame base (low 12 bits zero); the full address is
    /// `frame | (vaddr & 0xfff)` — correct for any page size, since the low 12
    /// bits always pass through.
    frame: u64,
    /// The generation this entry was filled in; stale once `tlb_gen` moves on.
    gen: u64,
}

/// Read a little-endian integer of `width` bytes from `ram[o..]`. The caller has
/// already bounds-checked `o + width <= ram.len()` (via `Emulator::offset`).
/// RISC-V memory accesses are 1/2/4/8 bytes; any other width falls back to a
/// byte loop. This replaces a per-byte shift-and-or with a single native read —
/// bit-identical to it, since `from_le_bytes` is endianness-defined regardless
/// of the host.
#[inline]
fn load_le(ram: &[u8], o: usize, width: usize) -> u64 {
    match width {
        1 => u64::from(ram[o]),
        2 => u64::from(u16::from_le_bytes([ram[o], ram[o + 1]])),
        4 => u64::from(u32::from_le_bytes([
            ram[o],
            ram[o + 1],
            ram[o + 2],
            ram[o + 3],
        ])),
        8 => u64::from_le_bytes([
            ram[o],
            ram[o + 1],
            ram[o + 2],
            ram[o + 3],
            ram[o + 4],
            ram[o + 5],
            ram[o + 6],
            ram[o + 7],
        ]),
        _ => {
            let mut v = 0u64;
            for i in 0..width {
                v |= u64::from(ram[o + i]) << (8 * i);
            }
            v
        }
    }
}

/// Write the low `width` bytes of `value` little-endian into `ram[o..]`. The
/// caller has already bounds-checked `o + width <= ram.len()`. Bit-identical to
/// the per-byte shift-and-store it replaces.
#[inline]
fn store_le(ram: &mut [u8], o: usize, width: usize, value: u64) {
    let bytes = value.to_le_bytes();
    match width {
        1 | 2 | 4 | 8 => ram[o..o + width].copy_from_slice(&bytes[..width]),
        _ => {
            for (i, b) in bytes.iter().enumerate().take(width) {
                ram[o + i] = *b;
            }
        }
    }
}

/// A malformed [`Emulator::snapshot`] could not be [restored](Emulator::restore)
/// — the bytes ended before a field could be read (never a valid snapshot this
/// emulator produced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The snapshot bytes were truncated mid-field.
    Truncated,
    /// A field held an invalid value (e.g. a non-UTF-8 9P path name).
    Malformed,
}

impl core::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SnapshotError::Truncated => write!(f, "snapshot is truncated"),
            SnapshotError::Malformed => write!(f, "snapshot field is malformed"),
        }
    }
}

impl core::error::Error for SnapshotError {}

/// A little-endian cursor over [`Emulator::snapshot`] bytes — the deserialization
/// dual of the `to_le_bytes` writes `snapshot` performs, in the same order.
struct SnapshotReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> SnapshotReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        let end = self.pos.checked_add(n).ok_or(SnapshotError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(SnapshotError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SnapshotError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        self.take(n)
    }
    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }
}

/// The RISC-V **PLIC** — priority per source, a pending latch, per-context
/// enable bits and a priority threshold, and the claim/complete handshake
/// (RISC-V PLIC spec). Only the sources holospaces wires (the VirtIO IRQ) are
/// live; the array is sized to cover them with headroom.
struct Plic {
    /// Interrupt priority for each source (index 0 is unused/"no interrupt").
    priority: [u32; 32],
    /// Pending latch, one bit per source (bit `i` = source `i`).
    pending: u32,
    /// Per-context enable bitmaps (bit `i` = source `i` enabled for the context).
    enable: [u32; PLIC_CONTEXTS],
    /// Per-context priority threshold (a source fires only if its priority is
    /// strictly greater).
    threshold: [u32; PLIC_CONTEXTS],
}

impl Plic {
    fn new() -> Self {
        Plic {
            priority: [0; 32],
            pending: 0,
            enable: [0; PLIC_CONTEXTS],
            threshold: [0; PLIC_CONTEXTS],
        }
    }

    /// Raise (latch pending) interrupt source `src`.
    fn raise(&mut self, src: u32) {
        self.pending |= 1 << src;
    }

    /// The highest-priority source pending, enabled for `context`, and above the
    /// context threshold — or 0 if none. Does not clear it (that is *claim*).
    fn best(&self, context: usize) -> u32 {
        let mut best_src = 0u32;
        let mut best_pri = 0u32;
        let candidates = self.pending & self.enable[context];
        for src in 1..32u32 {
            if candidates & (1 << src) != 0 {
                let pri = self.priority[src as usize];
                if pri > self.threshold[context] && pri > best_pri {
                    best_pri = pri;
                    best_src = src;
                }
            }
        }
        best_src
    }

    /// True if `context` has a deliverable external interrupt (drives `*EIP`).
    fn pending_for(&self, context: usize) -> bool {
        self.best(context) != 0
    }

    /// Claim the top interrupt for `context`: return its source and clear its
    /// pending bit (the driver will service it, then *complete*).
    fn claim(&mut self, context: usize) -> u32 {
        let src = self.best(context);
        if src != 0 {
            self.pending &= !(1 << src);
        }
        src
    }
}

/// The byte size of a disk sector (`virtio-blk` LBA unit).
const DISK_SECTOR: usize = 512;

/// The system emulator's block-device backing — the **κ-disk** (`CC-7`): every
/// 512-byte sector is *content-addressed in an owned [`KappaStore`]*, not held as
/// an in-RAM image. The KappaStore IS the memory (Law L3): identical sectors dedup
/// to one κ, an all-zero sector is sparse (stored as `None`, never put), and every
/// read/write goes through the store — the substrate, not a second medium (Law
/// L4). This is the same κ-addressed block device as [`crate::disk`], owned and
/// sync for the emulator's hot path. The disk snapshot is the reconstructed image
/// (content captured), so resume is faithful.
struct KappaBacking {
    /// The owned in-memory canonical store backing the sectors (the substrate).
    store: MemKappaStore,
    /// One entry per sector: the sector's content κ, or `None` for a sparse
    /// (never-written, all-zero) sector.
    index: Vec<Option<KappaLabel71>>,
}

impl KappaBacking {
    /// Load a disk image into the κ-disk: each 512-byte sector is content-addressed
    /// in the store (all-zero sectors stay sparse). The image is padded to a sector
    /// boundary.
    fn from_image(image: &[u8]) -> Self {
        let sector_count = image.len().div_ceil(DISK_SECTOR);
        let store = MemKappaStore::new();
        let mut index = Vec::with_capacity(sector_count);
        let mut sector = [0u8; DISK_SECTOR];
        for i in 0..sector_count {
            let start = i * DISK_SECTOR;
            let end = (start + DISK_SECTOR).min(image.len());
            sector.fill(0);
            sector[..end - start].copy_from_slice(&image[start..end]);
            index.push(Self::store_sector(&store, &sector));
        }
        KappaBacking { store, index }
    }

    /// Content-address a sector through the store (sparse all-zero → `None`).
    fn store_sector(store: &MemKappaStore, sector: &[u8; DISK_SECTOR]) -> Option<KappaLabel71> {
        if sector.iter().all(|&b| b == 0) {
            None
        } else {
            Some(store.put("blake3", sector).expect("κ-disk: put sector"))
        }
    }

    /// The disk capacity in bytes.
    fn len(&self) -> usize {
        self.index.len() * DISK_SECTOR
    }

    /// Read sector `i` (sparse → zeros) as a 512-byte block.
    fn read_sector(&self, i: usize) -> [u8; DISK_SECTOR] {
        let mut out = [0u8; DISK_SECTOR];
        if let Some(k) = &self.index[i] {
            let bytes = self
                .store
                .get(k)
                .ok()
                .flatten()
                .expect("κ-disk: a sector's content resolves for its κ");
            out.copy_from_slice(bytes.as_ref());
        }
        out
    }

    /// Read `buf.len()` bytes from byte offset `off` (spanning sectors).
    fn read_into(&self, off: usize, buf: &mut [u8]) {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done;
            let sector = self.read_sector(pos / DISK_SECTOR);
            let so = pos % DISK_SECTOR;
            let n = (DISK_SECTOR - so).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&sector[so..so + n]);
            done += n;
        }
    }

    /// Write `data` at byte offset `off` (read-modify-write per affected sector,
    /// re-content-addressing each touched sector through the store).
    fn write_from(&mut self, off: usize, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let pos = off + done;
            let si = pos / DISK_SECTOR;
            let mut sector = self.read_sector(si);
            let so = pos % DISK_SECTOR;
            let n = (DISK_SECTOR - so).min(data.len() - done);
            sector[so..so + n].copy_from_slice(&data[done..done + n]);
            self.index[si] = Self::store_sector(&self.store, &sector);
            done += n;
        }
    }

    /// Reconstruct the full disk image — the self-contained snapshot of the disk
    /// content (the live store dedups; the snapshot captures the bytes).
    fn to_image(&self) -> Vec<u8> {
        let mut image = vec![0u8; self.len()];
        for (i, slot) in self.index.iter().enumerate() {
            if let Some(k) = slot {
                let bytes = self
                    .store
                    .get(k)
                    .ok()
                    .flatten()
                    .expect("κ-disk: a sector's content resolves for its κ");
                image[i * DISK_SECTOR..(i + 1) * DISK_SECTOR].copy_from_slice(bytes.as_ref());
            }
        }
        image
    }
}

/// The VirtIO **block device** over the **virtio-mmio** transport (modern /
/// version 2, OASIS VirtIO v1.2). Its backing store is the root filesystem
/// image (the κ-disk's content, `CC-7`); a guest kernel mounts it over
/// `/dev/vda`. The device processes the split virtqueue on `QueueNotify`:
/// read the available ring, walk each request's descriptor chain (header →
/// data → status), serve it against the disk, write the used ring, and raise
/// the PLIC interrupt (`CC-14`).
struct VirtioBlk {
    /// The backing disk — the assembled rootfs as κ-addressed content (`CC-7`):
    /// every sector lives in the store, not an in-RAM image (Law L3/L4).
    disk: KappaBacking,
    /// Device status (the guest's driver progresses ACKNOWLEDGE→DRIVER→…→OK).
    status: u32,
    /// `DeviceFeaturesSel` / `DriverFeaturesSel` (which 32-bit feature word).
    device_features_sel: u32,
    driver_features_sel: u32,
    /// The features the driver accepted (word 0 and word 1).
    driver_features: [u32; 2],
    /// Selected queue (this device has one, index 0).
    queue_sel: u32,
    /// Negotiated queue size.
    queue_num: u32,
    /// Whether the queue is live (`QueueReady`).
    queue_ready: u32,
    /// Guest-physical addresses of the descriptor table, available ring, used
    /// ring (set by the driver via the `Queue*Low/High` registers).
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    /// The last available-ring index the device has consumed.
    last_avail: u16,
    /// `InterruptStatus` — bit 0 = used-ring update (the driver ACKs it).
    interrupt_status: u32,
    /// Set when the device has raised its IRQ and the PLIC must latch it.
    irq_pending: bool,
}

impl VirtioBlk {
    /// Construct from a disk *image*, content-addressing it into the κ-disk.
    fn new(image: Vec<u8>) -> Self {
        Self::with_backing(KappaBacking::from_image(&image))
    }

    /// Construct around an existing κ-disk backing (queue state freshly reset) —
    /// used on a device reset, which keeps the disk content and clears only the
    /// negotiated queue registers.
    fn with_backing(disk: KappaBacking) -> Self {
        VirtioBlk {
            disk,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: [0; 2],
            queue_sel: 0,
            queue_num: 0,
            queue_ready: 0,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail: 0,
            interrupt_status: 0,
            irq_pending: false,
        }
    }

    /// The disk capacity in 512-byte sectors (the `virtio_blk_config.capacity`).
    fn capacity_sectors(&self) -> u64 {
        self.disk.len() as u64 / 512
    }
}

/// The VirtIO **9P transport** block — a second `virtio-mmio` device carrying a
/// `virtio-9p` filesystem (OASIS VirtIO §5.7; device id 9). It serves a shared
/// **workspace filesystem** to the guest over the **9P2000.L** protocol: the
/// editor and the running OS read and write the *same* files (the IDE's shared
/// FS — `CC-15`). The guest mounts it with `-t 9p -o trans=virtio`, matching the
/// `mount_tag` in this device's config space.
struct Virtio9p {
    /// The 9P2000.L backend filesystem (the shared workspace tree).
    fs: ninep::Fs9p,
    /// Open file ids → backend inode (the 9P fid table).
    fids: alloc::collections::BTreeMap<u32, u64>,
    /// The mount tag the guest selects this device by.
    tag: alloc::string::String,
    // ── the virtio-mmio transport registers (same shape as VirtioBlk) ──
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: [u32; 2],
    queue_num: u32,
    queue_ready: u32,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    last_avail: u16,
    interrupt_status: u32,
    irq_pending: bool,
}

impl Virtio9p {
    fn new(fs: ninep::Fs9p, tag: &str) -> Self {
        Virtio9p {
            fs,
            fids: alloc::collections::BTreeMap::new(),
            tag: tag.into(),
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: [0; 2],
            queue_num: 0,
            queue_ready: 0,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail: 0,
            interrupt_status: 0,
            irq_pending: false,
        }
    }
}

/// The VirtIO **network device** over the **virtio-mmio** transport (OASIS
/// VirtIO v1.2 §5.1; device id 1). It carries the guest's Ethernet frames to and
/// from the userspace TCP/IP [NAT](net), which streams the payloads out over a
/// pluggable [egress](net::Egress) (`CC-16`, ADR-014). Two virtqueues: the
/// **receive** queue (index 0, device → guest) and the **transmit** queue (index
/// 1, guest → device). With `VIRTIO_F_VERSION_1` negotiated every buffer carries
/// the 12-byte `virtio_net_hdr_v1` prefix.
struct VirtioNet {
    /// The userspace network the device terminates the guest's link layer in.
    nat: net::Nat,
    /// The egress transport the NAT's TCP streams flow out over (a host socket
    /// natively; a WebSocket tunnel in the browser).
    egress: Box<dyn net::Egress>,
    /// The ingress transport carrying forwarded-port (inbound) connections to a
    /// server inside the devcontainer (`CC-21`); a no-op when no port is
    /// forwarded.
    ingress: Box<dyn net::Ingress>,
    /// MAC reported in config space (`VIRTIO_NET_F_MAC`).
    mac: [u8; 6],
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: [u32; 2],
    /// Selected queue (0 = receive, 1 = transmit).
    queue_sel: u32,
    // Per-queue transport state (index 0 = RX, 1 = TX).
    queue_num: [u32; 2],
    queue_ready: [u32; 2],
    desc_addr: [u64; 2],
    avail_addr: [u64; 2],
    used_addr: [u64; 2],
    last_avail: [u16; 2],
    interrupt_status: u32,
    irq_pending: bool,
}

impl VirtioNet {
    fn new(egress: Box<dyn net::Egress>, ingress: Box<dyn net::Ingress>) -> Self {
        VirtioNet {
            nat: net::Nat::new(),
            egress,
            ingress,
            mac: net::GUEST_MAC,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: [0; 2],
            queue_sel: 0,
            queue_num: [0; 2],
            queue_ready: [0; 2],
            desc_addr: [0; 2],
            avail_addr: [0; 2],
            used_addr: [0; 2],
            last_avail: [0; 2],
            interrupt_status: 0,
            irq_pending: false,
        }
    }
}

/// The **9P2000.L** protocol + an in-memory backend filesystem — the shared
/// workspace the `virtio-9p` device serves (the protocol authority is the
/// 9P2000.L specification; the differential oracle is `qemu-system-riscv64`'s
/// own 9p server). Only the messages a Linux 9p client issues to mount, stat,
/// open, read, write, create, and read directories are implemented; anything
/// else replies `Rlerror(EOPNOTSUPP)`.
mod ninep {
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;

    // 9P2000.L message types.
    const RLERROR: u8 = 7;
    const TSTATFS: u8 = 8;
    const TLOPEN: u8 = 12;
    const TLCREATE: u8 = 14;
    const TGETATTR: u8 = 24;
    const TSETATTR: u8 = 26;
    const TREADDIR: u8 = 40;
    const TRENAMEAT: u8 = 74;
    const TUNLINKAT: u8 = 76;
    const TMKDIR: u8 = 72;

    // `Tsetattr.valid` bits (9P2000.L) — which attributes the request changes.
    const SETATTR_MODE: u32 = 0x0000_0001;
    const SETATTR_SIZE: u32 = 0x0000_0008;
    // The mode's file-type bits (preserved when only permissions change).
    const S_IFMT: u32 = 0xf000;
    const TVERSION: u8 = 100;
    const TATTACH: u8 = 104;
    const TFLUSH: u8 = 108;
    const TWALK: u8 = 110;
    const TREAD: u8 = 116;
    const TWRITE: u8 = 118;
    const TCLUNK: u8 = 120;

    // POSIX file-type bits.
    const S_IFDIR: u32 = 0x4000;
    const S_IFREG: u32 = 0x8000;
    // Directory-entry types.
    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;
    // errno values the backend returns.
    const ENOENT: u32 = 2;
    const EOPNOTSUPP: u32 = 95;

    /// A backend inode: a directory (name → child inode) or a file (bytes).
    struct Inode {
        is_dir: bool,
        mode: u32,
        data: Vec<u8>,
        children: BTreeMap<String, u64>,
    }

    /// The shared workspace filesystem. Inode 1 is the root directory.
    pub struct Fs9p {
        inodes: BTreeMap<u64, Inode>,
        next: u64,
    }

    impl Fs9p {
        /// A new filesystem with an empty root directory.
        pub fn new() -> Self {
            let mut inodes = BTreeMap::new();
            inodes.insert(
                1,
                Inode {
                    is_dir: true,
                    mode: S_IFDIR | 0o755,
                    data: Vec::new(),
                    children: BTreeMap::new(),
                },
            );
            Fs9p { inodes, next: 2 }
        }

        /// Seed a regular file `name` in the root with `data` — content
        /// holospaces places on the share for the guest to read.
        pub fn seed_file(&mut self, name: &str, data: &[u8]) {
            let id = self.next;
            self.next += 1;
            self.inodes.insert(
                id,
                Inode {
                    is_dir: false,
                    mode: S_IFREG | 0o644,
                    data: data.to_vec(),
                    children: BTreeMap::new(),
                },
            );
            self.inodes
                .get_mut(&1)
                .unwrap()
                .children
                .insert(name.into(), id);
        }

        /// Read a root file's bytes back (what the guest wrote) — how holospaces
        /// observes the guest's edits to the shared workspace.
        pub fn read_file(&self, name: &str) -> Option<&[u8]> {
            let id = *self.inodes.get(&1)?.children.get(name)?;
            let n = self.inodes.get(&id)?;
            (!n.is_dir).then_some(n.data.as_slice())
        }

        /// List the root directory — `(name, is_dir, size)` per entry. The
        /// editor-side enumeration of the shared workspace (the workbench's
        /// `FileSystemProvider.readDirectory`; `CC-17`).
        pub fn list_root(&self) -> Vec<(String, bool, usize)> {
            let mut out = Vec::new();
            if let Some(root) = self.inodes.get(&1) {
                for (name, &id) in &root.children {
                    if let Some(n) = self.inodes.get(&id) {
                        out.push((name.clone(), n.is_dir, n.data.len()));
                    }
                }
            }
            out
        }

        /// Serialize the filesystem into a machine snapshot — every inode (the
        /// `BTreeMap` iterates in id order, and each inode's children in name
        /// order, so the bytes are deterministic and the snapshot κ reproducible,
        /// Law L1). The dual of [`Fs9p::restore`].
        pub fn snapshot_into(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.next.to_le_bytes());
            out.extend_from_slice(&(self.inodes.len() as u64).to_le_bytes());
            for (id, node) in &self.inodes {
                out.extend_from_slice(&id.to_le_bytes());
                out.push(u8::from(node.is_dir));
                out.extend_from_slice(&node.mode.to_le_bytes());
                out.extend_from_slice(&(node.data.len() as u64).to_le_bytes());
                out.extend_from_slice(&node.data);
                out.extend_from_slice(&(node.children.len() as u64).to_le_bytes());
                for (name, child) in &node.children {
                    out.extend_from_slice(&(name.len() as u64).to_le_bytes());
                    out.extend_from_slice(name.as_bytes());
                    out.extend_from_slice(&child.to_le_bytes());
                }
            }
        }

        /// Reconstruct a filesystem from snapshot bytes (the inverse of
        /// [`Fs9p::snapshot_into`]).
        ///
        /// # Errors
        ///
        /// [`super::SnapshotError`] if the bytes are truncated or a path name is
        /// not valid UTF-8.
        pub fn restore(r: &mut super::SnapshotReader) -> Result<Self, super::SnapshotError> {
            let next = r.u64()?;
            let count = r.u64()?;
            let mut inodes = BTreeMap::new();
            for _ in 0..count {
                let id = r.u64()?;
                let is_dir = r.u8()? != 0;
                let mode = r.u32()?;
                let data_len = r.u64()? as usize;
                let data = r.bytes(data_len)?.to_vec();
                let nchild = r.u64()?;
                let mut children = BTreeMap::new();
                for _ in 0..nchild {
                    let name_len = r.u64()? as usize;
                    let name = String::from_utf8(r.bytes(name_len)?.to_vec())
                        .map_err(|_| super::SnapshotError::Malformed)?;
                    children.insert(name, r.u64()?);
                }
                inodes.insert(
                    id,
                    Inode {
                        is_dir,
                        mode,
                        data,
                        children,
                    },
                );
            }
            Ok(Fs9p { inodes, next })
        }

        /// Write a root file (update in place, or create) — the editor saving
        /// into the *same* shared content the guest OS reads over `virtio-9p`
        /// (one content, Law L1; the workbench's `FileSystemProvider.writeFile`,
        /// `CC-17`). Returns the bytes' content address (κ on the blake3 axis) —
        /// the file's identity (Law L1/L2).
        pub fn write_file(&mut self, name: &str, data: &[u8]) {
            if let Some(&id) = self.inodes.get(&1).and_then(|r| r.children.get(name)) {
                if let Some(n) = self.inodes.get_mut(&id) {
                    if !n.is_dir {
                        n.data = data.to_vec();
                        return;
                    }
                }
            }
            let id = self.alloc(false, S_IFREG | 0o644);
            self.inodes.get_mut(&id).unwrap().data = data.to_vec();
            self.inodes
                .get_mut(&1)
                .unwrap()
                .children
                .insert(name.into(), id);
        }

        fn alloc(&mut self, is_dir: bool, mode: u32) -> u64 {
            let id = self.next;
            self.next += 1;
            self.inodes.insert(
                id,
                Inode {
                    is_dir,
                    mode,
                    data: Vec::new(),
                    children: BTreeMap::new(),
                },
            );
            id
        }

        /// Recursively drop an inode and its whole subtree (a removed directory's
        /// descendants), so an unlinked tree leaves no orphaned inodes.
        fn free_subtree(&mut self, id: u64) {
            if let Some(node) = self.inodes.remove(&id) {
                for child in node.children.values() {
                    self.free_subtree(*child);
                }
            }
        }

        /// Delete a root entry (and its subtree) — the editor removing a file or
        /// folder from the shared workspace (`FileSystemProvider.delete`, `CC-17`),
        /// the host-side dual of `Tunlinkat`. `true` if it existed.
        pub fn delete_file(&mut self, name: &str) -> bool {
            let Some(id) = self
                .inodes
                .get_mut(&1)
                .and_then(|r| r.children.remove(name))
            else {
                return false;
            };
            self.free_subtree(id);
            true
        }

        /// Rename a root entry — the editor moving a file or folder in the shared
        /// workspace (`FileSystemProvider.rename`, `CC-17`), the host-side dual of
        /// `Trenameat`. `true` if the source existed.
        pub fn rename(&mut self, from: &str, to: &str) -> bool {
            let Some(id) = self
                .inodes
                .get_mut(&1)
                .and_then(|r| r.children.remove(from))
            else {
                return false;
            };
            if let Some(displaced) = self
                .inodes
                .get_mut(&1)
                .and_then(|r| r.children.insert(to.into(), id))
            {
                self.free_subtree(displaced);
            }
            true
        }

        /// Create a root directory — the editor making a new folder in the shared
        /// workspace (`FileSystemProvider.createDirectory`, `CC-17`), the host-side
        /// dual of `Tmkdir`. A no-op if a directory of that name already exists.
        pub fn make_dir(&mut self, name: &str) {
            if let Some(&id) = self.inodes.get(&1).and_then(|r| r.children.get(name)) {
                if self.inodes.get(&id).is_some_and(|n| n.is_dir) {
                    return;
                }
            }
            let id = self.alloc(true, S_IFDIR | 0o755);
            self.inodes
                .get_mut(&1)
                .unwrap()
                .children
                .insert(name.into(), id);
        }
    }

    /// A little-endian cursor over a 9P message body.
    struct R<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl<'a> R<'a> {
        fn new(b: &'a [u8]) -> Self {
            R { b, p: 0 }
        }
        fn u8(&mut self) -> u8 {
            let v = self.b.get(self.p).copied().unwrap_or(0);
            self.p += 1;
            v
        }
        fn u16(&mut self) -> u16 {
            let mut a = [0u8; 2];
            a.copy_from_slice(self.b.get(self.p..self.p + 2).unwrap_or(&[0, 0]));
            self.p += 2;
            u16::from_le_bytes(a)
        }
        fn u32(&mut self) -> u32 {
            let mut a = [0u8; 4];
            if let Some(s) = self.b.get(self.p..self.p + 4) {
                a.copy_from_slice(s);
            }
            self.p += 4;
            u32::from_le_bytes(a)
        }
        fn u64(&mut self) -> u64 {
            let mut a = [0u8; 8];
            if let Some(s) = self.b.get(self.p..self.p + 8) {
                a.copy_from_slice(s);
            }
            self.p += 8;
            u64::from_le_bytes(a)
        }
        fn str(&mut self) -> String {
            let n = self.u16() as usize;
            let s = self.b.get(self.p..self.p + n).unwrap_or(&[]);
            self.p += n;
            String::from_utf8_lossy(s).into_owned()
        }
        fn bytes(&mut self, n: usize) -> &'a [u8] {
            let s = self.b.get(self.p..self.p + n).unwrap_or(&[]);
            self.p += n;
            s
        }
    }

    /// A little-endian 9P message body builder.
    struct W(Vec<u8>);
    impl W {
        fn new() -> Self {
            W(Vec::new())
        }
        fn u8(&mut self, v: u8) {
            self.0.push(v);
        }
        fn u16(&mut self, v: u16) {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        fn u32(&mut self, v: u32) {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        fn u64(&mut self, v: u64) {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        fn str(&mut self, s: &str) {
            self.u16(s.len() as u16);
            self.0.extend_from_slice(s.as_bytes());
        }
        /// A `qid[13]`: type, version, path.
        fn qid(&mut self, is_dir: bool, path: u64) {
            self.u8(if is_dir { 0x80 } else { 0x00 });
            self.u32(0);
            self.u64(path);
        }
    }

    /// Wrap a reply body in the `size[4] type[1] tag[2] body` envelope.
    fn envelope(rtype: u8, tag: u16, body: &[u8]) -> Vec<u8> {
        let size = 7 + body.len() as u32;
        let mut out = Vec::with_capacity(size as usize);
        out.extend_from_slice(&size.to_le_bytes());
        out.push(rtype);
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn rlerror(tag: u16, ecode: u32) -> Vec<u8> {
        let mut w = W::new();
        w.u32(ecode);
        envelope(RLERROR, tag, &w.0)
    }

    /// Handle one T-message against `fs`/`fids`, returning the R-message bytes.
    pub fn handle(fs: &mut Fs9p, fids: &mut BTreeMap<u32, u64>, msg: &[u8]) -> Vec<u8> {
        let mut r = R::new(msg);
        let _size = r.u32();
        let ttype = r.u8();
        let tag = r.u16();

        match ttype {
            TVERSION => {
                let msize = r.u32();
                let _ver = r.str();
                let mut w = W::new();
                w.u32(msize.min(64 * 1024));
                w.str("9P2000.L");
                envelope(TVERSION + 1, tag, &w.0)
            }
            TATTACH => {
                let fid = r.u32();
                fids.insert(fid, 1); // the root inode
                let mut w = W::new();
                w.qid(true, 1);
                envelope(TATTACH + 1, tag, &w.0)
            }
            TWALK => {
                let fid = r.u32();
                let newfid = r.u32();
                let nwname = r.u16() as usize;
                let Some(&start) = fids.get(&fid) else {
                    return rlerror(tag, ENOENT);
                };
                let mut cur = start;
                let mut qids: Vec<(bool, u64)> = Vec::new();
                for i in 0..nwname {
                    let name = r.str();
                    let next = match fs.inodes.get(&cur) {
                        Some(n) if n.is_dir => resolve_child(n, &name),
                        _ => None,
                    };
                    match next {
                        Some(id) => {
                            cur = id;
                            let isd = fs.inodes.get(&id).map(|n| n.is_dir).unwrap_or(false);
                            qids.push((isd, id));
                        }
                        None => {
                            if i == 0 {
                                return rlerror(tag, ENOENT);
                            }
                            break; // partial walk
                        }
                    }
                }
                if qids.len() == nwname {
                    fids.insert(newfid, cur);
                }
                let mut w = W::new();
                w.u16(qids.len() as u16);
                for (isd, id) in &qids {
                    w.qid(*isd, *id);
                }
                envelope(TWALK + 1, tag, &w.0)
            }
            TGETATTR => {
                let fid = r.u32();
                let _mask = r.u64();
                let Some(n) = fids.get(&fid).and_then(|id| fs.inodes.get(id)) else {
                    return rlerror(tag, ENOENT);
                };
                let id = *fids.get(&fid).unwrap();
                let mut w = W::new();
                w.u64(0x0000_07ff); // valid: the basic fields
                w.qid(n.is_dir, id);
                w.u32(n.mode);
                w.u32(0); // uid
                w.u32(0); // gid
                w.u64(if n.is_dir { 2 } else { 1 }); // nlink
                w.u64(0); // rdev
                w.u64(n.data.len() as u64); // size
                w.u64(4096); // blksize
                w.u64(n.data.len().div_ceil(512) as u64); // blocks
                for _ in 0..8 {
                    w.u64(0); // atime/mtime/ctime/btime sec+nsec
                }
                w.u64(0); // gen
                w.u64(0); // data_version
                envelope(TGETATTR + 1, tag, &w.0)
            }
            TLOPEN => {
                let fid = r.u32();
                let _flags = r.u32();
                let Some(&id) = fids.get(&fid) else {
                    return rlerror(tag, ENOENT);
                };
                let isd = fs.inodes.get(&id).map(|n| n.is_dir).unwrap_or(false);
                let mut w = W::new();
                w.qid(isd, id);
                w.u32(0); // iounit (0 = no limit)
                envelope(TLOPEN + 1, tag, &w.0)
            }
            TLCREATE => {
                let dfid = r.u32();
                let name = r.str();
                let _flags = r.u32();
                let mode = r.u32();
                let _gid = r.u32();
                let Some(&dir) = fids.get(&dfid) else {
                    return rlerror(tag, ENOENT);
                };
                let id = fs.alloc(false, S_IFREG | (mode & 0o7777));
                if let Some(d) = fs.inodes.get_mut(&dir) {
                    d.children.insert(name, id);
                }
                fids.insert(dfid, id); // the fid now refers to the new file
                let mut w = W::new();
                w.qid(false, id);
                w.u32(0);
                envelope(TLCREATE + 1, tag, &w.0)
            }
            TMKDIR => {
                let dfid = r.u32();
                let name = r.str();
                let mode = r.u32();
                let _gid = r.u32();
                let Some(&dir) = fids.get(&dfid) else {
                    return rlerror(tag, ENOENT);
                };
                let id = fs.alloc(true, S_IFDIR | (mode & 0o7777));
                if let Some(d) = fs.inodes.get_mut(&dir) {
                    d.children.insert(name, id);
                }
                let mut w = W::new();
                w.qid(true, id);
                envelope(TMKDIR + 1, tag, &w.0)
            }
            TREAD => {
                let fid = r.u32();
                let offset = r.u64() as usize;
                let count = r.u32() as usize;
                let Some(n) = fids.get(&fid).and_then(|id| fs.inodes.get(id)) else {
                    return rlerror(tag, ENOENT);
                };
                let slice = n.data.get(offset..).unwrap_or(&[]);
                let take = slice.len().min(count);
                let mut w = W::new();
                w.u32(take as u32);
                w.0.extend_from_slice(&slice[..take]);
                envelope(TREAD + 1, tag, &w.0)
            }
            TWRITE => {
                let fid = r.u32();
                let offset = r.u64() as usize;
                let count = r.u32() as usize;
                let data = r.bytes(count);
                let Some(&id) = fids.get(&fid) else {
                    return rlerror(tag, ENOENT);
                };
                if let Some(n) = fs.inodes.get_mut(&id) {
                    if n.data.len() < offset + count {
                        n.data.resize(offset + count, 0);
                    }
                    n.data[offset..offset + count].copy_from_slice(data);
                }
                let mut w = W::new();
                w.u32(count as u32);
                envelope(TWRITE + 1, tag, &w.0)
            }
            TREADDIR => {
                let fid = r.u32();
                let offset = r.u64();
                let count = r.u32() as usize;
                let Some(&id) = fids.get(&fid) else {
                    return rlerror(tag, ENOENT);
                };
                let mut body = W::new();
                let mut entries: Vec<(String, u64, bool)> = Vec::new();
                if let Some(n) = fs.inodes.get(&id) {
                    // "." and ".." then the children, with sequential cookies.
                    entries.push((".".into(), id, true));
                    entries.push(("..".into(), 1, true));
                    for (name, cid) in &n.children {
                        let isd = fs.inodes.get(cid).map(|c| c.is_dir).unwrap_or(false);
                        entries.push((name.clone(), *cid, isd));
                    }
                }
                let mut data = W::new();
                for (i, (name, cid, isd)) in entries.iter().enumerate() {
                    let cookie = (i + 1) as u64;
                    if cookie <= offset {
                        continue;
                    }
                    let mut e = W::new();
                    e.qid(*isd, *cid);
                    e.u64(cookie);
                    e.u8(if *isd { DT_DIR } else { DT_REG });
                    e.str(name);
                    if data.0.len() + e.0.len() > count {
                        break;
                    }
                    data.0.extend_from_slice(&e.0);
                }
                body.u32(data.0.len() as u32);
                body.0.extend_from_slice(&data.0);
                envelope(TREADDIR + 1, tag, &body.0)
            }
            TCLUNK => {
                let fid = r.u32();
                fids.remove(&fid);
                envelope(TCLUNK + 1, tag, &[])
            }
            TSTATFS => {
                let _fid = r.u32();
                let mut w = W::new();
                w.u32(0x0102_1997); // f_type (V9FS_MAGIC)
                w.u32(4096); // bsize
                w.u64(1 << 20); // blocks
                w.u64(1 << 20); // bfree
                w.u64(1 << 20); // bavail
                w.u64(1 << 16); // files
                w.u64(1 << 16); // ffree
                w.u64(0); // fsid
                w.u32(255); // namelen
                envelope(TSTATFS + 1, tag, &w.0)
            }
            TSETATTR => {
                // Apply the attribute change so it persists (a guest `chmod +x`
                // on a workspace file is honoured, not silently dropped): update
                // the mode's permission bits and/or truncate to the new size, as
                // the `valid` mask selects.
                let fid = r.u32();
                let valid = r.u32();
                let mode = r.u32();
                let _uid = r.u32();
                let _gid = r.u32();
                let size = r.u64();
                let Some(&id) = fids.get(&fid) else {
                    return rlerror(tag, ENOENT);
                };
                if let Some(n) = fs.inodes.get_mut(&id) {
                    if valid & SETATTR_MODE != 0 {
                        n.mode = (n.mode & S_IFMT) | (mode & 0o7777);
                    }
                    if valid & SETATTR_SIZE != 0 && !n.is_dir {
                        n.data.resize(size as usize, 0);
                    }
                }
                envelope(TSETATTR + 1, tag, &[])
            }
            TUNLINKAT => {
                // Remove `name` from the directory `dfid` (a guest `rm` / `rmdir`
                // on the workspace) — honoured, not dropped.
                let dfid = r.u32();
                let name = r.str();
                let _flags = r.u32();
                let Some(&dir) = fids.get(&dfid) else {
                    return rlerror(tag, ENOENT);
                };
                let child = fs
                    .inodes
                    .get(&dir)
                    .and_then(|d| d.children.get(&name).copied());
                match child {
                    Some(cid) => {
                        if let Some(d) = fs.inodes.get_mut(&dir) {
                            d.children.remove(&name);
                        }
                        fs.free_subtree(cid);
                        envelope(TUNLINKAT + 1, tag, &[])
                    }
                    None => rlerror(tag, ENOENT),
                }
            }
            TRENAMEAT => {
                // Move `oldname` under `olddirfid` to `newname` under `newdirfid`
                // (a guest `mv` on the workspace) — honoured, not dropped.
                let olddir = r.u32();
                let oldname = r.str();
                let newdir = r.u32();
                let newname = r.str();
                let (Some(&od), Some(&nd)) = (fids.get(&olddir), fids.get(&newdir)) else {
                    return rlerror(tag, ENOENT);
                };
                let child = fs
                    .inodes
                    .get(&od)
                    .and_then(|d| d.children.get(&oldname).copied());
                match child {
                    Some(cid) => {
                        if let Some(d) = fs.inodes.get_mut(&od) {
                            d.children.remove(&oldname);
                        }
                        // Replace any existing target so a `mv -f` is faithful.
                        let displaced = fs
                            .inodes
                            .get_mut(&nd)
                            .and_then(|d| d.children.insert(newname, cid));
                        if let Some(old) = displaced {
                            fs.free_subtree(old);
                        }
                        envelope(TRENAMEAT + 1, tag, &[])
                    }
                    None => rlerror(tag, ENOENT),
                }
            }
            TFLUSH => envelope(TFLUSH + 1, tag, &[]),
            _ => rlerror(tag, EOPNOTSUPP),
        }
    }

    fn resolve_child(dir: &Inode, name: &str) -> Option<u64> {
        match name {
            "." => None, // handled by the caller's current fid
            _ => dir.children.get(name).copied(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Build a T-message envelope (`size[4] type[1] tag[2] body`).
        fn msg(ttype: u8, tag: u16, body: &[u8]) -> Vec<u8> {
            let size = 7 + body.len() as u32;
            let mut m = size.to_le_bytes().to_vec();
            m.push(ttype);
            m.extend_from_slice(&tag.to_le_bytes());
            m.extend_from_slice(body);
            m
        }
        fn rtype(reply: &[u8]) -> u8 {
            reply[4]
        }

        // Attach (root fid=1) and walk fid=1 → newfid=2 to a named root child.
        fn attach_and_walk(fs: &mut Fs9p, fids: &mut BTreeMap<u32, u64>, name: &str) {
            let mut att = W::new();
            att.u32(1); // fid
            att.u32(0); // afid
            att.str("u");
            att.str("");
            att.u32(0);
            handle(fs, fids, &msg(TATTACH, 0, &att.0));
            let mut wlk = W::new();
            wlk.u32(1); // fid (root)
            wlk.u32(2); // newfid
            wlk.u16(1); // nwname
            wlk.str(name);
            handle(fs, fids, &msg(TWALK, 0, &wlk.0));
        }

        #[test]
        fn tsetattr_persists_a_mode_change() {
            let mut fs = Fs9p::new();
            fs.seed_file("script.sh", b"#!/bin/sh\n");
            let mut fids = BTreeMap::new();
            attach_and_walk(&mut fs, &mut fids, "script.sh");
            // chmod 0755 on fid 2.
            let mut b = W::new();
            b.u32(2); // fid
            b.u32(SETATTR_MODE); // valid: mode only
            b.u32(0o755); // mode
            b.u32(0); // uid
            b.u32(0); // gid
            b.u64(0); // size
            for _ in 0..4 {
                b.u64(0); // atime/mtime sec+nsec
            }
            let reply = handle(&mut fs, &mut fids, &msg(TSETATTR, 0, &b.0));
            assert_eq!(rtype(&reply), TSETATTR + 1);
            // Tgetattr: mode is at body offset 8 (valid u64) + 13 (qid) = 21.
            let mut g = W::new();
            g.u32(2);
            g.u64(0x07ff);
            let attr = handle(&mut fs, &mut fids, &msg(TGETATTR, 0, &g.0));
            let mode_off = 7 + 8 + 13;
            let mode = u32::from_le_bytes(attr[mode_off..mode_off + 4].try_into().unwrap());
            assert_eq!(mode & 0o7777, 0o755, "the chmod persisted (not dropped)");
            assert_eq!(mode & S_IFMT, S_IFREG, "the file-type bits are preserved");
        }

        #[test]
        fn tunlinkat_removes_a_root_entry() {
            let mut fs = Fs9p::new();
            fs.seed_file("gone.txt", b"x");
            let mut fids = BTreeMap::new();
            attach_and_walk(&mut fs, &mut fids, "gone.txt"); // also attaches root fid 1
            let mut b = W::new();
            b.u32(1); // dfid = root
            b.str("gone.txt");
            b.u32(0); // flags
            let reply = handle(&mut fs, &mut fids, &msg(TUNLINKAT, 0, &b.0));
            assert_eq!(rtype(&reply), TUNLINKAT + 1);
            assert!(fs.read_file("gone.txt").is_none(), "the file was removed");
        }

        #[test]
        fn trenameat_moves_a_root_entry() {
            let mut fs = Fs9p::new();
            fs.seed_file("old.txt", b"data");
            let mut fids = BTreeMap::new();
            attach_and_walk(&mut fs, &mut fids, "old.txt");
            let mut b = W::new();
            b.u32(1); // olddirfid = root
            b.str("old.txt");
            b.u32(1); // newdirfid = root
            b.str("new.txt");
            let reply = handle(&mut fs, &mut fids, &msg(TRENAMEAT, 0, &b.0));
            assert_eq!(rtype(&reply), TRENAMEAT + 1);
            assert!(fs.read_file("old.txt").is_none());
            assert_eq!(fs.read_file("new.txt"), Some(&b"data"[..]));
        }

        #[test]
        fn host_side_duals_match_the_wire_ops() {
            let mut fs = Fs9p::new();
            fs.make_dir("src");
            assert!(fs.list_root().iter().any(|(n, d, _)| n == "src" && *d));
            fs.seed_file("a", b"1");
            assert!(fs.rename("a", "b"));
            assert_eq!(fs.read_file("b"), Some(&b"1"[..]));
            assert!(fs.delete_file("b"));
            assert!(fs.read_file("b").is_none());
            assert!(!fs.delete_file("missing"));
        }
    }
}

impl Emulator {
    /// Create a machine with `ram_bytes` of RAM mapped at `base`, the reset PC.
    #[must_use]
    pub fn new(base: u64, ram_bytes: usize) -> Self {
        let mut csrs = BTreeMap::new();
        // `misa` reports the ISA: RV64 (MXL=2) with the I, M, A, C extensions a
        // kernel checks for. mhartid defaults to 0 (single hart).
        // RV64 (MXL=2) with A, C, D, F, I, M.
        let misa = (2u64 << 62) | (1 << 0) | (1 << 2) | (1 << 3) | (1 << 5) | (1 << 8) | (1 << 12);
        csrs.insert(csr::MISA, misa);
        // `mstatus.UXL`/`SXL` are fixed at 2 (XLEN 64) on this RV64 hart.
        csrs.insert(csr::MSTATUS, csr::XLEN_64);
        Self {
            hart: Hart {
                x: [0; 32],
                f: [0; 32],
                pc: base,
            },
            ram: vec![0; ram_bytes],
            base,
            console: Vec::new(),
            console_in: Vec::new(),
            in_cursor: 0,
            csrs,
            reservation: None,
            priv_level: PRIV_M,
            htif: None,
            mtime: 0,
            mtimecmp: 0,
            msip: false,
            provide_sbi: false,
            plic: Plic::new(),
            virtio: None,
            virtio9p: None,
            virtionet: None,
            loopback: None,
            // Entries default to gen 0; starting `tlb_gen` at 1 means no entry is
            // live until it is filled (no false hit on a zeroed slot).
            tlb: Box::new(
                [[TlbEntry {
                    tag: 0,
                    frame: 0,
                    gen: 0,
                }; TLB_SETS]; 3],
            ),
            tlb_gen: 1,
        }
    }

    /// Attach a shared **workspace filesystem** to the machine's VirtIO 9P device
    /// (`CC-15`). `seed` is the files holospaces places on the share (name →
    /// bytes); the guest mounts it (`-t 9p`, tag `hsworkspace`) and the editor and
    /// the running OS read and write the *same* files. Read the guest's writes
    /// back with [`Self::workspace_file`].
    pub fn attach_workspace(&mut self, seed: &[(&str, &[u8])]) {
        let mut fs = ninep::Fs9p::new();
        for (name, data) in seed {
            fs.seed_file(name, data);
        }
        self.virtio9p = Some(Virtio9p::new(fs, WORKSPACE_TAG));
    }

    /// Read a file from the shared workspace filesystem — how holospaces observes
    /// the edits the guest made over 9P (`CC-15`). `None` if no 9P device is
    /// attached or the file is absent.
    #[must_use]
    pub fn workspace_file(&self, name: &str) -> Option<&[u8]> {
        self.virtio9p.as_ref().and_then(|d| d.fs.read_file(name))
    }

    /// List the shared workspace's root entries — `(name, is_dir, size)` — the
    /// editor's directory view over the running holospace (`CC-17`; the workbench
    /// `FileSystemProvider` binds to this over the wasm peer). Empty if no
    /// workspace is attached.
    #[must_use]
    pub fn workspace_list(&self) -> Vec<(alloc::string::String, bool, usize)> {
        self.virtio9p
            .as_ref()
            .map(|d| d.fs.list_root())
            .unwrap_or_default()
    }

    /// Write a file into the shared workspace — the editor saving content the
    /// running OS reads over `virtio-9p` (one content, Law L1; `CC-17`). No-op if
    /// no workspace is attached.
    pub fn workspace_write(&mut self, name: &str, data: &[u8]) {
        if let Some(d) = self.virtio9p.as_mut() {
            d.fs.write_file(name, data);
        }
    }

    /// Delete a file or folder from the shared workspace — the editor's
    /// `FileSystemProvider.delete` over the running holospace (`CC-17`). `true` if
    /// it existed. No-op (and `false`) if no workspace is attached.
    pub fn workspace_delete(&mut self, name: &str) -> bool {
        self.virtio9p
            .as_mut()
            .map(|d| d.fs.delete_file(name))
            .unwrap_or(false)
    }

    /// Rename a file or folder in the shared workspace — the editor's
    /// `FileSystemProvider.rename` over the running holospace (`CC-17`). `true` if
    /// the source existed.
    pub fn workspace_rename(&mut self, from: &str, to: &str) -> bool {
        self.virtio9p
            .as_mut()
            .map(|d| d.fs.rename(from, to))
            .unwrap_or(false)
    }

    /// Create a folder in the shared workspace — the editor's
    /// `FileSystemProvider.createDirectory` over the running holospace (`CC-17`).
    pub fn workspace_mkdir(&mut self, name: &str) {
        if let Some(d) = self.virtio9p.as_mut() {
            d.fs.make_dir(name);
        }
    }

    /// Attach a root filesystem disk to the machine's VirtIO block device — the
    /// assembled rootfs (`CC-7` κ-disk content). The guest kernel discovers it
    /// through the device tree's `virtio_mmio` node and mounts it over
    /// `/dev/vda` (`CC-14`). The disk image is part of the machine's
    /// reproducible state (it is snapshotted with the rest).
    pub fn attach_disk(&mut self, image: Vec<u8>) {
        self.virtio = Some(VirtioBlk::new(image));
    }

    /// Attach the machine's VirtIO **network** device, bridged to the world over
    /// `egress` (`CC-16`). The guest kernel discovers the NIC through the device
    /// tree's third `virtio_mmio` node, configures it with DHCP, and its frames
    /// are terminated by the userspace TCP/IP [NAT](net) and streamed out over
    /// `egress` — a host socket natively ([`net::StdEgress`]) or a WebSocket
    /// tunnel in the browser (ADR-014).
    pub fn attach_net(&mut self, egress: Box<dyn net::Egress>) {
        self.virtionet = Some(VirtioNet::new(egress, Box::new(net::NoIngress)));
    }

    /// Attach the network device with both an `egress` (outbound) and an
    /// `ingress` (forwarded-port inbound) transport (`CC-16` + `CC-21`). A server
    /// the devcontainer runs on a forwarded port is then reachable from outside
    /// (the running-app preview).
    pub fn attach_net_forward(
        &mut self,
        egress: Box<dyn net::Egress>,
        ingress: Box<dyn net::Ingress>,
    ) {
        self.virtionet = Some(VirtioNet::new(egress, ingress));
    }

    /// Enable the **in-process loopback ingress** (ADR-020, `CC-33`) on the
    /// already-attached network device: the workbench (in the same process as the
    /// emulator) can `dial`/`send`/`recv`/`close` a connection to a server
    /// *inside* the guest, the inward in-process dual of the egress relay. Keeps
    /// the existing egress (so the guest can still reach the internet). Returns
    /// `false` if no network device is attached. The host side is driven through
    /// the emulator's `dial_guest`/`guest_send`/`guest_recv`/`guest_close`.
    pub fn enable_loopback(&mut self) -> bool {
        let Some(net) = self.virtionet.as_mut() else {
            return false;
        };
        let (ingress, handle) = net::LoopbackIngress::new();
        net.ingress = Box::new(ingress);
        self.loopback = Some(handle);
        true
    }

    /// Dial an in-process connection to the guest's listening `guest_port` over
    /// the loopback bridge (`CC-33`). Returns the connection id, or `None` if the
    /// loopback ingress is not enabled. Pump the machine (`run`) so the NAT opens
    /// the connection toward the guest and the byte stream flows.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.loopback.as_ref().map(|h| h.dial(guest_port))
    }

    /// Write host bytes toward the guest server on a loopback connection.
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        if let Some(h) = self.loopback.as_ref() {
            h.send(id, data);
        }
    }

    /// Drain the guest server's reply bytes on a loopback connection (empty if
    /// none have arrived yet — pump the machine to advance the stream).
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.loopback
            .as_ref()
            .map(|h| h.recv(id))
            .unwrap_or_default()
    }

    /// Close the host side of a loopback connection.
    pub fn guest_close(&mut self, id: u32) {
        if let Some(h) = self.loopback.as_ref() {
            h.close(id);
        }
    }

    /// Whether a loopback connection is still usable (the guest has not closed it,
    /// or has but unread bytes remain).
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.loopback.as_ref().is_some_and(|h| h.is_open(id))
    }

    /// **Live** network reconfiguration (ADR-018, `CC-28`): begin forwarding
    /// `guest_port` on the *running* machine — the control plane's network
    /// directive applied to an instance after boot, without a reboot. Returns the
    /// host port the new route is reachable on, or `None` if the machine has no
    /// network device or its ingress transport cannot add a forward live.
    pub fn forward_port(&mut self, guest_port: u16) -> Option<u16> {
        self.virtionet
            .as_mut()
            .and_then(|n| n.ingress.add_forward(guest_port))
    }

    /// Run the emulator as the M-mode firmware (SEE), servicing supervisor SBI
    /// calls (console / timer / shutdown) — the mode a real S-mode OS kernel
    /// boots under (the conformance tests run with this off).
    pub fn enable_sbi(&mut self) {
        self.provide_sbi = true;
    }

    /// Set the HTIF `tohost` address — a store there ends the run with an exit
    /// code (the riscv-tests / SBI signalling channel). Configured from the
    /// guest image's `tohost` symbol.
    pub fn set_htif(&mut self, tohost: u64) {
        self.htif = Some(tohost);
    }

    fn csr_read(&self, csr: u32) -> u64 {
        // `sstatus`/`sie`/`sip` are restricted views of `mstatus`/`mie`/`mip`.
        match csr {
            // `mstatus`/`sstatus` expose the WARL XLEN fields fixed at 2 (XLEN 64).
            csr::MSTATUS => (self.raw_csr(csr::MSTATUS) & !csr::XLEN_MASK) | csr::XLEN_64,
            csr::SSTATUS => (self.raw_csr(csr::MSTATUS) & csr::SSTATUS_MASK) | csr::UXL_64,
            csr::SIE => self.raw_csr(csr::MIE) & csr::S_INT_MASK,
            csr::SIP => self.raw_csr(csr::MIP) & csr::S_INT_MASK,
            csr::FFLAGS => self.raw_csr(csr::FCSR) & 0x1f,
            csr::FRM => (self.raw_csr(csr::FCSR) >> 5) & 0x7,
            csr::TIME | csr::CYCLE | csr::INSTRET => self.mtime,
            // No debug triggers implemented — the trigger CSRs read as 0.
            csr::TSELECT | csr::TDATA1 | csr::TDATA2 | csr::TDATA3 | csr::TINFO | csr::TCONTROL => {
                0
            }
            _ => self.raw_csr(csr),
        }
    }

    fn raw_csr(&self, csr: u32) -> u64 {
        self.csrs.get(&csr).copied().unwrap_or(0)
    }

    fn csr_write(&mut self, csr: u32, value: u64) {
        match csr {
            csr::SSTATUS => {
                let old = self.raw_csr(csr::MSTATUS);
                let m = (old & !csr::SSTATUS_MASK) | (value & csr::SSTATUS_MASK);
                self.csrs.insert(csr::MSTATUS, m);
                if (old ^ m) & MSTATUS_XLATE_BITS != 0 {
                    self.tlb_flush();
                }
            }
            csr::MSTATUS => {
                let old = self.raw_csr(csr::MSTATUS);
                self.csrs.insert(csr::MSTATUS, value);
                if (old ^ value) & MSTATUS_XLATE_BITS != 0 {
                    self.tlb_flush();
                }
            }
            csr::SIE => {
                let m = (self.raw_csr(csr::MIE) & !csr::S_INT_MASK) | (value & csr::S_INT_MASK);
                self.csrs.insert(csr::MIE, m);
            }
            csr::SIP => {
                let m = (self.raw_csr(csr::MIP) & !csr::S_INT_MASK) | (value & csr::S_INT_MASK);
                self.csrs.insert(csr::MIP, m);
            }
            csr::FFLAGS => {
                let f = (self.raw_csr(csr::FCSR) & !0x1f) | (value & 0x1f);
                self.csrs.insert(csr::FCSR, f);
            }
            csr::FRM => {
                let f = (self.raw_csr(csr::FCSR) & !0xe0) | ((value & 0x7) << 5);
                self.csrs.insert(csr::FCSR, f);
            }
            csr::FCSR => {
                // Only frm (7:5) + fflags (4:0) are writable; the rest is 0.
                self.csrs.insert(csr::FCSR, value & 0xff);
            }
            // `misa` is read-only on this hart (a fixed ISA — the common
            // implementation; the `ma_fetch` test detects that C cannot be
            // disabled and skips the IALIGN cases). The debug trigger CSRs are
            // likewise hardwired (no triggers).
            csr::MISA
            | csr::TSELECT
            | csr::TDATA1
            | csr::TDATA2
            | csr::TDATA3
            | csr::TINFO
            | csr::TCONTROL => {}
            0x180 => {
                // `satp` MODE is WARL (RISC-V Privileged ISA §4.1.11). This hart
                // implements bare + the full Sv39/Sv48/Sv57 set (the `translate`
                // walker handles all three), so a write selecting any of those
                // takes; a reserved MODE leaves the field bare. A modern kernel
                // probes for the deepest mode by writing `satp` and reading it
                // back — and gets whatever it asks for, up to Sv57.
                let v = if matches!(value >> 60, 0 | 8 | 9 | 10) {
                    value
                } else {
                    value & !(0xfu64 << 60)
                };
                self.csrs.insert(0x180, v);
                // A new root page table / ASID: the whole TLB is now stale.
                self.tlb_flush();
            }
            _ => {
                self.csrs.insert(csr, value);
            }
        }
    }

    // ── floating-point register file (F/D) — raw bits; `f32` values are
    //    NaN-boxed in the high half (RISC-V Unprivileged ISA §11.3) ──

    fn frd(&self, i: u32) -> u64 {
        self.hart.f[i as usize]
    }

    fn fwr(&mut self, i: u32, bits: u64) {
        self.hart.f[i as usize] = bits;
        self.mark_fs_dirty();
    }

    /// Read FP register `i` as an `f32` (the low half if properly NaN-boxed,
    /// else the canonical NaN per the ISA).
    fn frd32(&self, i: u32) -> f32 {
        let bits = self.hart.f[i as usize];
        if bits >> 32 == 0xffff_ffff {
            f32::from_bits(bits as u32)
        } else {
            f32::from_bits(0x7fc0_0000) // canonical NaN
        }
    }

    fn fwr32(&mut self, i: u32, x: f32) {
        self.hart.f[i as usize] = 0xffff_ffff_0000_0000 | u64::from(x.to_bits());
        self.mark_fs_dirty();
    }

    fn frd64(&self, i: u32) -> f64 {
        f64::from_bits(self.hart.f[i as usize])
    }

    fn fwr64(&mut self, i: u32, x: f64) {
        self.hart.f[i as usize] = x.to_bits();
        self.mark_fs_dirty();
    }

    /// Mark the FP state dirty (`mstatus.FS = 3`) — a context switch saves the
    /// F registers (RISC-V Privileged ISA §3.1.6).
    fn mark_fs_dirty(&mut self) {
        let st = self.raw_csr(csr::MSTATUS) | (3 << 13);
        self.csrs.insert(csr::MSTATUS, st);
    }

    /// Whether the floating-point unit is on (`mstatus.FS` ≠ Off). When it is
    /// off, any FP instruction or FP-CSR access traps as illegal (RISC-V
    /// Privileged ISA §3.1.6.6) — the kernel uses this for lazy FP context save.
    fn fp_enabled(&self) -> bool {
        (self.raw_csr(csr::MSTATUS) >> 13) & 3 != 0
    }

    /// Accrue floating-point exception flags into `fcsr.fflags`
    /// (NV=0x10 / DZ=0x08 / OF=0x04 / UF=0x02 / NX=0x01).
    fn set_fflags(&mut self, flags: u8) {
        if flags != 0 {
            let f = self.raw_csr(csr::FCSR) | u64::from(flags);
            self.csrs.insert(csr::FCSR, f);
        }
    }

    /// The effective rounding mode for an FP instruction: its `rm` field, or —
    /// when that is "dynamic" (7) — the `fcsr.frm` field.
    fn rounding_mode(&self, inst: u32) -> u32 {
        let rm = (inst >> 12) & 0x7;
        if rm == 7 {
            ((self.raw_csr(csr::FCSR) >> 5) & 0x7) as u32
        } else {
            rm
        }
    }

    /// Take a trap (RISC-V Privileged ISA §3.1.6/§4.1.1). An exception taken in
    /// S/U mode whose cause is delegated (`medeleg`) is handled in supervisor
    /// mode (`stvec`/`sepc`/`scause`/`stval`, `sstatus` SPP/SPIE); otherwise it
    /// is handled in machine mode (`mtvec`/`mepc`/`mcause`/`mtval`, `mstatus`
    /// MPP/MPIE).
    fn trap(&mut self, cause: u64, tval: u64, epc: u64) {
        let is_int = cause >> 63 != 0;
        let code = cause & 0xfff;
        let deleg = if is_int {
            self.raw_csr(csr::MIDELEG)
        } else {
            self.raw_csr(csr::MEDELEG)
        };
        let delegated = self.priv_level <= PRIV_S && (deleg >> code) & 1 != 0;
        let mut st = self.raw_csr(csr::MSTATUS);
        if delegated {
            self.csr_write(csr::SEPC, epc);
            self.csr_write(csr::SCAUSE, cause);
            self.csr_write(csr::STVAL, tval);
            let sie = (st >> 1) & 1;
            st = (st & !(1 << 5)) | (sie << 5); // SPIE = SIE
            st &= !(1 << 1); // SIE = 0
            st = (st & !(1 << 8)) | ((u64::from(self.priv_level) & 1) << 8); // SPP
            self.csrs.insert(csr::MSTATUS, st);
            self.priv_level = PRIV_S;
            self.hart.pc = trap_vector(self.csr_read(csr::STVEC), is_int, code);
        } else {
            self.csr_write(csr::MEPC, epc);
            self.csr_write(csr::MCAUSE, cause);
            self.csr_write(csr::MTVAL, tval);
            let mie = (st >> 3) & 1;
            st = (st & !(1 << 7)) | (mie << 7); // MPIE = MIE
            st &= !(1 << 3); // MIE = 0
            st = (st & !(3 << 11)) | (u64::from(self.priv_level) << 11); // MPP
            self.csrs.insert(csr::MSTATUS, st);
            self.priv_level = PRIV_M;
            self.hart.pc = trap_vector(self.csr_read(csr::MTVEC), is_int, code);
        }
        // A trap changes the privilege level (and `mstatus`), so the effective
        // translation context changes — invalidate the TLB.
        self.tlb_flush();
    }

    /// Return from a machine-mode trap (`mret`).
    fn mret(&mut self) {
        let mut st = self.raw_csr(csr::MSTATUS);
        let mpie = (st >> 7) & 1;
        let mpp = (st >> 11) & 3;
        st = (st & !(1 << 3)) | (mpie << 3); // MIE = MPIE
        st |= 1 << 7; // MPIE = 1
        st &= !(3 << 11); // MPP = U
        self.csrs.insert(csr::MSTATUS, st);
        self.priv_level = mpp as u8;
        self.hart.pc = self.csr_read(csr::MEPC);
        self.tlb_flush();
    }

    /// Return from a supervisor-mode trap (`sret`).
    fn sret(&mut self) {
        let mut st = self.raw_csr(csr::MSTATUS);
        let spie = (st >> 5) & 1;
        let spp = (st >> 8) & 1;
        st = (st & !(1 << 1)) | (spie << 1); // SIE = SPIE
        st |= 1 << 5; // SPIE = 1
        st &= !(1 << 8); // SPP = U
        self.csrs.insert(csr::MSTATUS, st);
        self.priv_level = spp as u8;
        self.hart.pc = self.csr_read(csr::SEPC);
        self.tlb_flush();
    }

    /// Load a flat guest image at `base` and set the reset PC there.
    ///
    /// # Errors
    ///
    /// [`Trap::AccessFault`] if the image does not fit in RAM.
    pub fn load_flat(&mut self, image: &[u8]) -> Result<(), Trap> {
        if image.len() > self.ram.len() {
            return Err(Trap::AccessFault(self.base));
        }
        self.ram[..image.len()].copy_from_slice(image);
        self.hart.pc = self.base;
        Ok(())
    }

    /// Boot a RISC-V supervisor OS `Image`: place the kernel at the load offset
    /// its own header declares, the flattened device tree (`dtb`) at `dtb_addr`,
    /// and hand off the way the SBI firmware does — drop to S-mode at the kernel
    /// entry with `a0` = hart id (0) and `a1` = the DTB pointer (RISC-V boot
    /// protocol). The emulator services the kernel's SBI calls (`enable_sbi`).
    ///
    /// The load offset is read from the RISC-V Image header (`text_offset` at
    /// byte 8, identified by the `RSC\x05` magic at byte 56), so the boot adapts
    /// to whatever the image asks for rather than assuming one layout.
    ///
    /// # Errors
    ///
    /// [`Trap::AccessFault`] if `image` is not a RISC-V `Image` (no header) or it
    /// and the device tree do not fit at their addresses in RAM.
    pub fn boot_kernel(&mut self, image: &[u8], dtb: &[u8], dtb_addr: u64) -> Result<(), Trap> {
        let text_offset = image_text_offset(image).ok_or(Trap::AccessFault(self.base))?;
        let entry = self.base + text_offset;
        let koff = self.offset(entry, image.len())?;
        self.ram[koff..koff + image.len()].copy_from_slice(image);
        let doff = self.offset(dtb_addr, dtb.len())?;
        self.ram[doff..doff + dtb.len()].copy_from_slice(dtb);
        self.hart.pc = entry;
        self.priv_level = PRIV_S;
        self.hart.x[10] = 0; // a0 = boot hart id
        self.hart.x[11] = dtb_addr; // a1 = device-tree blob
        self.provide_sbi = true; // the emulator is the SEE / SBI firmware
                                 // Delegate the standard exceptions and S-mode interrupts to supervisor
                                 // mode, the way the SBI firmware (OpenSBI) does — so the kernel's own
                                 // `stvec` handler services its page faults, syscalls, and timer/software
                                 // interrupts (RISC-V Privileged ISA §3.1.8; OpenSBI `medeleg`/`mideleg`).
                                 // medeleg: misaligned/access/illegal/breakpoint/ecall-from-U + the three
                                 // page faults (causes 0-8, 12, 13, 15). mideleg: S software/timer/external.
        self.csrs.insert(csr::MEDELEG, 0xb1ff);
        self.csrs
            .insert(csr::MIDELEG, (1 << 1) | (1 << 5) | (1 << 9));
        Ok(())
    }

    /// The bytes the guest has written to fd 1/2 via the `write` syscall — its
    /// console output (the channel the emulator codemodule publishes).
    #[must_use]
    pub fn console(&self) -> &[u8] {
        &self.console
    }

    /// Feed bytes to the machine's console *input* — the operator's keystrokes /
    /// terminal commands. The guest reads them through the SBI console (legacy
    /// `console_getchar` or the DBCN `console_read`). This is the terminal-input
    /// intent of a workspace projection (CC-11): driving a running holospace.
    pub fn feed_console(&mut self, bytes: &[u8]) {
        self.console_in.extend_from_slice(bytes);
    }

    /// Take one pending console input byte (SBI `console_getchar`), or `None`.
    fn console_getchar(&mut self) -> Option<u8> {
        let b = self.console_in.get(self.in_cursor).copied();
        if b.is_some() {
            self.in_cursor += 1;
        }
        b
    }

    /// The current program counter (diagnostics / single-stepping).
    #[must_use]
    pub fn pc(&self) -> u64 {
        self.hart.pc
    }

    /// Read a CSR (diagnostics / OS bring-up).
    #[must_use]
    pub fn csr(&self, n: u32) -> u64 {
        self.csr_read(n)
    }

    /// Read an integer register (diagnostics / OS bring-up).
    #[must_use]
    pub fn xreg(&self, i: usize) -> u64 {
        self.hart.x[i]
    }

    /// Read `width` bytes of physical RAM, little-endian (diagnostics — page
    /// table inspection during OS bring-up). Out-of-range reads return 0.
    #[must_use]
    pub fn peek(&self, addr: u64, width: usize) -> u64 {
        match self.offset(addr, width) {
            Ok(o) => {
                let mut v = 0u64;
                for i in 0..width {
                    v |= (self.ram[o + i] as u64) << (8 * i);
                }
                v
            }
            Err(_) => 0,
        }
    }

    /// Advance the timer, take any pending interrupt, and execute one
    /// instruction (diagnostics / single-stepping during OS bring-up).
    pub fn step_once(&mut self) -> Result<(), Halt> {
        self.tick();
        if self.take_interrupt() {
            return Ok(());
        }
        self.step()
    }

    /// A reproducible snapshot of machine state — registers, PC, and RAM — the
    /// canonical bytes the substrate κ-addresses on suspend (`CC-9`). Identical
    /// runs produce identical snapshots (Law L1).
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 * 33 + self.ram.len());
        out.extend_from_slice(&self.hart.pc.to_le_bytes());
        for r in &self.hart.x {
            out.extend_from_slice(&r.to_le_bytes());
        }
        for r in &self.hart.f {
            out.extend_from_slice(&r.to_le_bytes());
        }
        // The CSR file (Zicsr), in canonical (sorted) order — deterministic, so
        // the snapshot κ is reproducible (BTreeMap iterates in key order).
        out.extend_from_slice(&(self.csrs.len() as u32).to_le_bytes());
        for (csr, value) in &self.csrs {
            out.extend_from_slice(&csr.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        // The remaining machine state: privilege, the LR/SC reservation, the
        // CLINT timer, and the firmware mode — so a suspended *running* machine
        // resumes identically (a complete, reproducible κ snapshot; `CC-9`).
        out.push(self.priv_level);
        out.push(u8::from(self.provide_sbi));
        out.push(u8::from(self.msip));
        out.extend_from_slice(&self.reservation.unwrap_or(u64::MAX).to_le_bytes());
        out.extend_from_slice(&self.mtime.to_le_bytes());
        out.extend_from_slice(&self.mtimecmp.to_le_bytes());
        // The pending console input + read cursor (the terminal-input channel), so
        // a suspended interactive machine resumes identically (CC-11).
        out.extend_from_slice(&(self.console_in.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.console_in);
        out.extend_from_slice(&(self.in_cursor as u64).to_le_bytes());
        // The PLIC state (priority / pending / enable / threshold) — so a
        // suspended machine with a device interrupt in flight resumes identically.
        for p in &self.plic.priority {
            out.extend_from_slice(&p.to_le_bytes());
        }
        out.extend_from_slice(&self.plic.pending.to_le_bytes());
        for e in &self.plic.enable {
            out.extend_from_slice(&e.to_le_bytes());
        }
        for t in &self.plic.threshold {
            out.extend_from_slice(&t.to_le_bytes());
        }
        // The VirtIO block device: its negotiated queue state and the disk
        // contents (the κ-disk is part of the machine's reproducible state).
        match &self.virtio {
            None => out.push(0),
            Some(dev) => {
                out.push(1);
                out.extend_from_slice(&dev.status.to_le_bytes());
                out.extend_from_slice(&dev.device_features_sel.to_le_bytes());
                out.extend_from_slice(&dev.driver_features_sel.to_le_bytes());
                out.extend_from_slice(&dev.driver_features[0].to_le_bytes());
                out.extend_from_slice(&dev.driver_features[1].to_le_bytes());
                out.extend_from_slice(&dev.queue_sel.to_le_bytes());
                out.extend_from_slice(&dev.queue_num.to_le_bytes());
                out.extend_from_slice(&dev.queue_ready.to_le_bytes());
                out.extend_from_slice(&dev.desc_addr.to_le_bytes());
                out.extend_from_slice(&dev.avail_addr.to_le_bytes());
                out.extend_from_slice(&dev.used_addr.to_le_bytes());
                out.extend_from_slice(&dev.last_avail.to_le_bytes());
                out.extend_from_slice(&dev.interrupt_status.to_le_bytes());
                out.push(u8::from(dev.irq_pending));
                // The disk content (reconstructed from the κ-disk) — a self-
                // contained snapshot so resume is faithful on any peer.
                let image = dev.disk.to_image();
                out.extend_from_slice(&(image.len() as u64).to_le_bytes());
                out.extend_from_slice(&image);
            }
        }
        // The VirtIO 9P device: its transport state, the fid table, the mount
        // tag, and the shared workspace filesystem itself (the user's files) —
        // so a suspended *workspace* resumes with its content intact (`CC-15`).
        match &self.virtio9p {
            None => out.push(0),
            Some(dev) => {
                out.push(1);
                out.extend_from_slice(&dev.status.to_le_bytes());
                out.extend_from_slice(&dev.device_features_sel.to_le_bytes());
                out.extend_from_slice(&dev.driver_features_sel.to_le_bytes());
                out.extend_from_slice(&dev.driver_features[0].to_le_bytes());
                out.extend_from_slice(&dev.driver_features[1].to_le_bytes());
                out.extend_from_slice(&dev.queue_num.to_le_bytes());
                out.extend_from_slice(&dev.queue_ready.to_le_bytes());
                out.extend_from_slice(&dev.desc_addr.to_le_bytes());
                out.extend_from_slice(&dev.avail_addr.to_le_bytes());
                out.extend_from_slice(&dev.used_addr.to_le_bytes());
                out.extend_from_slice(&dev.last_avail.to_le_bytes());
                out.extend_from_slice(&dev.interrupt_status.to_le_bytes());
                out.push(u8::from(dev.irq_pending));
                out.extend_from_slice(&(dev.tag.len() as u64).to_le_bytes());
                out.extend_from_slice(dev.tag.as_bytes());
                out.extend_from_slice(&(dev.fids.len() as u64).to_le_bytes());
                for (fid, ino) in &dev.fids {
                    out.extend_from_slice(&fid.to_le_bytes());
                    out.extend_from_slice(&ino.to_le_bytes());
                }
                dev.fs.snapshot_into(&mut out);
            }
        }
        // VirtIO networking is deliberately *not* snapshotted: its egress/ingress
        // are live, external transports (a host socket, or a WebSocket to a relay)
        // that cannot be frozen into content. A resumed machine re-establishes its
        // network — connections reset, exactly as a host does on wake.
        out.extend_from_slice(&self.ram);
        out
    }

    /// Restore a machine from the bytes [`Emulator::snapshot`] produced — the
    /// inverse that makes suspend/resume a round trip (`CC-30`). `base` is the
    /// machine's RAM base (the construction parameter; not part of the content
    /// snapshot). The reconstructed machine re-snapshots to the *same* bytes and
    /// continues execution byte-identically to the machine that was suspended
    /// (Law L1). The software TLB is a pure cache and is rebuilt empty.
    ///
    /// # Errors
    ///
    /// [`SnapshotError`] if the bytes are truncated or internally inconsistent
    /// (a malformed snapshot, never a valid one this emulator produced).
    pub fn restore(base: u64, bytes: &[u8]) -> Result<Self, SnapshotError> {
        let mut r = SnapshotReader::new(bytes);
        let mut emu = Self::new(base, 0);
        emu.hart.pc = r.u64()?;
        for x in &mut emu.hart.x {
            *x = r.u64()?;
        }
        for f in &mut emu.hart.f {
            *f = r.u64()?;
        }
        let csr_count = r.u32()?;
        emu.csrs.clear();
        for _ in 0..csr_count {
            let csr = r.u32()?;
            let value = r.u64()?;
            emu.csrs.insert(csr, value);
        }
        emu.priv_level = r.u8()?;
        emu.provide_sbi = r.u8()? != 0;
        emu.msip = r.u8()? != 0;
        let reservation = r.u64()?;
        emu.reservation = (reservation != u64::MAX).then_some(reservation);
        emu.mtime = r.u64()?;
        emu.mtimecmp = r.u64()?;
        let console_in_len = r.u64()? as usize;
        emu.console_in = r.bytes(console_in_len)?.to_vec();
        emu.in_cursor = r.u64()? as usize;
        for p in &mut emu.plic.priority {
            *p = r.u32()?;
        }
        emu.plic.pending = r.u32()?;
        for e in &mut emu.plic.enable {
            *e = r.u32()?;
        }
        for t in &mut emu.plic.threshold {
            *t = r.u32()?;
        }
        emu.virtio = match r.u8()? {
            0 => None,
            _ => {
                let status = r.u32()?;
                let device_features_sel = r.u32()?;
                let driver_features_sel = r.u32()?;
                let driver_features = [r.u32()?, r.u32()?];
                let queue_sel = r.u32()?;
                let queue_num = r.u32()?;
                let queue_ready = r.u32()?;
                let desc_addr = r.u64()?;
                let avail_addr = r.u64()?;
                let used_addr = r.u64()?;
                let last_avail = r.u16()?;
                let interrupt_status = r.u32()?;
                let irq_pending = r.u8()? != 0;
                let disk_len = r.u64()? as usize;
                // Rebuild the κ-disk by content-addressing the snapshot image back
                // into a fresh store (the substrate; Law L3).
                let disk = KappaBacking::from_image(r.bytes(disk_len)?);
                Some(VirtioBlk {
                    disk,
                    status,
                    device_features_sel,
                    driver_features_sel,
                    driver_features,
                    queue_sel,
                    queue_num,
                    queue_ready,
                    desc_addr,
                    avail_addr,
                    used_addr,
                    last_avail,
                    interrupt_status,
                    irq_pending,
                })
            }
        };
        emu.virtio9p = match r.u8()? {
            0 => None,
            _ => {
                let status = r.u32()?;
                let device_features_sel = r.u32()?;
                let driver_features_sel = r.u32()?;
                let driver_features = [r.u32()?, r.u32()?];
                let queue_num = r.u32()?;
                let queue_ready = r.u32()?;
                let desc_addr = r.u64()?;
                let avail_addr = r.u64()?;
                let used_addr = r.u64()?;
                let last_avail = r.u16()?;
                let interrupt_status = r.u32()?;
                let irq_pending = r.u8()? != 0;
                let tag_len = r.u64()? as usize;
                let tag = alloc::string::String::from_utf8(r.bytes(tag_len)?.to_vec())
                    .map_err(|_| SnapshotError::Malformed)?;
                let nfids = r.u64()?;
                let mut fids = BTreeMap::new();
                for _ in 0..nfids {
                    let fid = r.u32()?;
                    fids.insert(fid, r.u64()?);
                }
                let fs = ninep::Fs9p::restore(&mut r)?;
                Some(Virtio9p {
                    fs,
                    fids,
                    tag,
                    status,
                    device_features_sel,
                    driver_features_sel,
                    driver_features,
                    queue_num,
                    queue_ready,
                    desc_addr,
                    avail_addr,
                    used_addr,
                    last_avail,
                    interrupt_status,
                    irq_pending,
                })
            }
        };
        emu.ram = r.rest().to_vec();
        Ok(emu)
    }

    /// Run until the guest exits, traps, or `max_steps` is reached.
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for _ in 0..max_steps {
            // At each instruction boundary: advance the timer, reconcile the
            // CLINT interrupt latches into `mip`, and take a pending interrupt
            // (which redirects the PC) before the next instruction.
            self.tick();
            // Pump the network periodically so host-side data and connection
            // events reach the guest without it having to transmit first (the
            // `virtio-net` receive path; CC-16).
            if self.virtionet.is_some() && self.mtime & 0x3ff == 0 {
                self.virtio_net_pump();
            }
            if self.take_interrupt() {
                continue;
            }
            match self.step() {
                Ok(()) => {}
                Err(halt) => return halt,
            }
        }
        Halt::OutOfBudget
    }

    // ── memory (flat, little-endian; `base`-relative) ──

    fn offset(&self, addr: u64, width: usize) -> Result<usize, Trap> {
        let off = addr.wrapping_sub(self.base);
        let end = off
            .checked_add(width as u64)
            .ok_or(Trap::AccessFault(addr))?;
        if end > self.ram.len() as u64 {
            return Err(Trap::AccessFault(addr));
        }
        Ok(off as usize)
    }

    fn load_phys(&mut self, addr: u64, width: usize) -> Result<u64, Trap> {
        // Fast path: an access at or above the top of the device window cannot be
        // a device, so route straight to RAM. This is the overwhelmingly common
        // case (RAM sits at 0x8000_0000, far above every device). Semantics are
        // identical to the device-check chain below, which falls through to RAM
        // for exactly these addresses.
        if addr >= DEVICE_MMIO_END {
            let o = self.offset(addr, width)?;
            return Ok(load_le(&self.ram, o, width));
        }
        if (CLINT_BASE..CLINT_END).contains(&addr) {
            return Ok(self.clint_read(addr));
        }
        if (PLIC_BASE..PLIC_END).contains(&addr) {
            return Ok(self.plic_read(addr));
        }
        if (VIRTIO_BASE..VIRTIO_END).contains(&addr) {
            return Ok(self.virtio_read(addr, width));
        }
        if (VIRTIO9P_BASE..VIRTIO9P_END).contains(&addr) {
            return Ok(self.virtio9p_read(addr));
        }
        if (VIRTIONET_BASE..VIRTIONET_END).contains(&addr) {
            return Ok(self.virtionet_read(addr));
        }
        // A sub-device-window address that matched no device — RAM (or a fault).
        let o = self.offset(addr, width)?;
        Ok(load_le(&self.ram, o, width))
    }

    fn store_phys(&mut self, addr: u64, width: usize, value: u64) -> Result<(), Trap> {
        if addr >= DEVICE_MMIO_END {
            let o = self.offset(addr, width)?;
            store_le(&mut self.ram, o, width, value);
            return Ok(());
        }
        if (CLINT_BASE..CLINT_END).contains(&addr) {
            self.clint_write(addr, value);
            return Ok(());
        }
        if (PLIC_BASE..PLIC_END).contains(&addr) {
            self.plic_write(addr, value as u32);
            return Ok(());
        }
        if (VIRTIO_BASE..VIRTIO_END).contains(&addr) {
            self.virtio_write(addr, value as u32);
            return Ok(());
        }
        if (VIRTIO9P_BASE..VIRTIO9P_END).contains(&addr) {
            self.virtio9p_write(addr, value as u32);
            return Ok(());
        }
        if (VIRTIONET_BASE..VIRTIONET_END).contains(&addr) {
            self.virtionet_write(addr, value as u32);
            return Ok(());
        }
        let o = self.offset(addr, width)?;
        store_le(&mut self.ram, o, width, value);
        Ok(())
    }

    /// Read the CLINT timer registers (RISC-V CLINT memory map).
    fn clint_read(&self, addr: u64) -> u64 {
        match addr - CLINT_BASE {
            0x0 => u64::from(self.msip),
            0x4000 => self.mtimecmp,
            0xbff8 => self.mtime,
            _ => 0,
        }
    }

    fn clint_write(&mut self, addr: u64, value: u64) {
        match addr - CLINT_BASE {
            0x0 => self.msip = value & 1 != 0,
            0x4000 => self.mtimecmp = value,
            0xbff8 => self.mtime = value,
            _ => {}
        }
    }

    // ── PLIC (RISC-V platform-level interrupt controller) ──

    /// Read a PLIC register (priority / pending / enable / threshold / claim).
    /// A read of a context's claim register *claims* the top interrupt (clears
    /// its pending bit) — the only mutating read.
    fn plic_read(&mut self, addr: u64) -> u64 {
        let off = addr - PLIC_BASE;
        if off < 0x1000 {
            let src = (off / 4) as usize;
            return u64::from(*self.plic.priority.get(src).unwrap_or(&0));
        }
        if (0x1000..0x2000).contains(&off) {
            // Pending bits; word 0 holds sources 0..31 (all we wire).
            return if (off - 0x1000) / 4 == 0 {
                u64::from(self.plic.pending)
            } else {
                0
            };
        }
        if (0x2000..0x20_0000).contains(&off) {
            let ctx = ((off - 0x2000) / 0x80) as usize;
            let word = ((off - 0x2000) % 0x80) / 4;
            if word == 0 && ctx < PLIC_CONTEXTS {
                return u64::from(self.plic.enable[ctx]);
            }
            return 0;
        }
        // Threshold (reg 0) / claim (reg 4) per context.
        let ctx = ((off - 0x20_0000) / 0x1000) as usize;
        let reg = (off - 0x20_0000) % 0x1000;
        if ctx >= PLIC_CONTEXTS {
            return 0;
        }
        match reg {
            0 => u64::from(self.plic.threshold[ctx]),
            4 => u64::from(self.plic.claim(ctx)),
            _ => 0,
        }
    }

    /// Write a PLIC register. A write to a context's complete register (the same
    /// offset as claim) acknowledges the source; pending was already cleared on
    /// claim, so the source is free to fire again.
    fn plic_write(&mut self, addr: u64, value: u32) {
        let off = addr - PLIC_BASE;
        if off < 0x1000 {
            let src = (off / 4) as usize;
            if let Some(p) = self.plic.priority.get_mut(src) {
                *p = value;
            }
            return;
        }
        if (0x2000..0x20_0000).contains(&off) {
            let ctx = ((off - 0x2000) / 0x80) as usize;
            let word = ((off - 0x2000) % 0x80) / 4;
            if word == 0 && ctx < PLIC_CONTEXTS {
                self.plic.enable[ctx] = value;
            }
            return;
        }
        if off >= 0x20_0000 {
            let ctx = ((off - 0x20_0000) / 0x1000) as usize;
            let reg = (off - 0x20_0000) % 0x1000;
            if ctx < PLIC_CONTEXTS && reg == 0 {
                self.plic.threshold[ctx] = value;
            }
            // reg == 4 is *complete*: a no-op here (pending cleared at claim).
        }
    }

    // ── VirtIO-MMIO transport + block device (OASIS VirtIO v1.2) ──

    /// Read a VirtIO-MMIO register or the block-config space.
    fn virtio_read(&self, addr: u64, _width: usize) -> u64 {
        devbus::blk_mmio_read(self.virtio.as_ref(), addr - VIRTIO_BASE)
    }

    /// Write a VirtIO-MMIO register; a write to `QueueNotify` runs the queue.
    fn virtio_write(&mut self, addr: u64, value: u32) {
        let off = addr - VIRTIO_BASE;
        let notify = match self.virtio.as_mut() {
            Some(dev) => devbus::blk_mmio_write(dev, off, value),
            None => return,
        };
        if notify {
            self.virtio_process_queue();
        }
    }

    /// Process every newly-available request in the block device's virtqueue:
    /// walk each descriptor chain (header → data → status), serve it against the
    /// disk, write the used ring, and raise the PLIC interrupt (VirtIO v1.2 §2.7
    /// split virtqueue; §5.2 block device).
    fn virtio_process_queue(&mut self) {
        let mut dev = self.virtio.take().unwrap();
        let mut mem = devbus::GuestRam {
            ram: &mut self.ram,
            base: self.base,
        };
        let raise = devbus::blk_service_queue(&mut mem, &mut dev);
        self.virtio = Some(dev);
        if raise {
            self.plic.raise(VIRTIO_IRQ);
        }
    }

    // Little-endian guest-physical helpers for virtqueue structures.
    fn rd8(&mut self, a: u64) -> u8 {
        self.load_phys(a, 1).unwrap_or(0) as u8
    }
    fn rd16(&mut self, a: u64) -> u16 {
        self.load_phys(a, 2).unwrap_or(0) as u16
    }
    fn rd32(&mut self, a: u64) -> u32 {
        self.load_phys(a, 4).unwrap_or(0) as u32
    }
    fn rd64(&mut self, a: u64) -> u64 {
        self.load_phys(a, 8).unwrap_or(0)
    }
    fn wr8(&mut self, a: u64, v: u8) {
        let _ = self.store_phys(a, 1, u64::from(v));
    }
    fn wr16(&mut self, a: u64, v: u16) {
        let _ = self.store_phys(a, 2, u64::from(v));
    }
    fn wr32(&mut self, a: u64, v: u32) {
        let _ = self.store_phys(a, 4, u64::from(v));
    }

    // ── VirtIO 9P device (the shared workspace filesystem; CC-15) ──

    /// Read a VirtIO-MMIO register or 9P-config field of the 9P device.
    fn virtio9p_read(&self, addr: u64) -> u64 {
        let Some(dev) = self.virtio9p.as_ref() else {
            return 0;
        };
        let off = addr - VIRTIO9P_BASE;
        match off {
            0x000 => 0x7472_6976, // MagicValue
            0x004 => 2,           // Version (modern)
            0x008 => 9,           // DeviceID = 9P transport
            0x00c => 0x554d_4551, // VendorID
            0x010 => match dev.device_features_sel {
                // word 0: VIRTIO_9P_MOUNT_TAG (bit 0); word 1: VERSION_1 (bit 32).
                0 => 1,
                1 => 1,
                _ => 0,
            },
            0x034 => 1024, // QueueNumMax
            0x044 => u64::from(dev.queue_ready),
            0x060 => u64::from(dev.interrupt_status),
            0x070 => u64::from(dev.status),
            0x0fc => 0, // ConfigGeneration
            // 9P config: tag length (u16) then the tag bytes.
            0x100 => dev.tag.len() as u64,
            0x101 => (dev.tag.len() >> 8) as u64,
            _ if (0x102..0x102 + dev.tag.len() as u64).contains(&off) => {
                u64::from(dev.tag.as_bytes()[(off - 0x102) as usize])
            }
            _ => 0,
        }
    }

    /// Write a VirtIO-MMIO register of the 9P device; `QueueNotify` runs the queue.
    fn virtio9p_write(&mut self, addr: u64, value: u32) {
        let off = addr - VIRTIO9P_BASE;
        if self.virtio9p.is_none() {
            return;
        }
        {
            let dev = self.virtio9p.as_mut().unwrap();
            match off {
                0x014 => dev.device_features_sel = value,
                0x020 => {
                    let w = dev.driver_features_sel.min(1) as usize;
                    dev.driver_features[w] = value;
                }
                0x024 => dev.driver_features_sel = value,
                0x038 => dev.queue_num = value,
                0x044 => dev.queue_ready = value,
                0x064 => dev.interrupt_status &= !value,
                0x070 => dev.status = value,
                0x080 => dev.desc_addr = (dev.desc_addr & !0xffff_ffff) | u64::from(value),
                0x084 => dev.desc_addr = (dev.desc_addr & 0xffff_ffff) | (u64::from(value) << 32),
                0x090 => dev.avail_addr = (dev.avail_addr & !0xffff_ffff) | u64::from(value),
                0x094 => dev.avail_addr = (dev.avail_addr & 0xffff_ffff) | (u64::from(value) << 32),
                0x0a0 => dev.used_addr = (dev.used_addr & !0xffff_ffff) | u64::from(value),
                0x0a4 => dev.used_addr = (dev.used_addr & 0xffff_ffff) | (u64::from(value) << 32),
                _ => {}
            }
        }
        if off == 0x050 {
            self.virtio9p_process_queue();
        }
    }

    /// Process every newly-available 9P request: gather the T-message from the
    /// chain's readable descriptors, handle it against the workspace filesystem,
    /// scatter the R-message into the writable descriptors, and raise the IRQ.
    fn virtio9p_process_queue(&mut self) {
        let mut dev = self.virtio9p.take().unwrap();
        let qsz = dev.queue_num as u16;
        if dev.queue_ready == 0 || qsz == 0 {
            self.virtio9p = Some(dev);
            return;
        }
        let avail_idx = self.rd16(dev.avail_addr + 2);
        while dev.last_avail != avail_idx {
            let slot = dev.last_avail % qsz;
            let head = self.rd16(dev.avail_addr + 4 + 2 * u64::from(slot));
            let written = self.virtio9p_service_chain(&mut dev, head);
            let used_idx = self.rd16(dev.used_addr + 2);
            let ring = dev.used_addr + 4 + 8 * u64::from(used_idx % qsz);
            self.wr32(ring, u32::from(head));
            self.wr32(ring + 4, written);
            self.wr16(dev.used_addr + 2, used_idx.wrapping_add(1));
            dev.last_avail = dev.last_avail.wrapping_add(1);
            dev.interrupt_status |= 1;
            dev.irq_pending = true;
        }
        let raise = dev.irq_pending;
        dev.irq_pending = false;
        self.virtio9p = Some(dev);
        if raise {
            self.plic.raise(VIRTIO9P_IRQ);
        }
    }

    /// Service one 9P request chain: the leading read-only descriptors carry the
    /// T-message; the trailing write-only descriptors receive the R-message.
    fn virtio9p_service_chain(&mut self, dev: &mut Virtio9p, head: u16) -> u32 {
        const F_NEXT: u16 = 1;
        const F_WRITE: u16 = 2;
        let mut readable: Vec<u8> = Vec::new();
        let mut writable: Vec<(u64, u32)> = Vec::new();
        let mut idx = head;
        let mut guard = 0;
        loop {
            let d = dev.desc_addr + 16 * u64::from(idx);
            let addr = self.rd64(d);
            let len = self.rd32(d + 8);
            let flags = self.rd16(d + 12);
            let next = self.rd16(d + 14);
            if flags & F_WRITE != 0 {
                writable.push((addr, len));
            } else {
                for i in 0..u64::from(len) {
                    readable.push(self.rd8(addr + i));
                }
            }
            guard += 1;
            if flags & F_NEXT == 0 || guard > dev.queue_num {
                break;
            }
            idx = next;
        }
        // Handle the T-message, producing the R-message.
        let reply = ninep::handle(&mut dev.fs, &mut dev.fids, &readable);
        // Scatter the reply into the writable descriptors.
        let mut written = 0u32;
        let mut pos = 0usize;
        for (addr, len) in &writable {
            if pos >= reply.len() {
                break;
            }
            let n = ((*len as usize).min(reply.len() - pos)) as u32;
            for i in 0..n {
                self.wr8(addr + u64::from(i), reply[pos + i as usize]);
            }
            pos += n as usize;
            written += n;
        }
        written
    }

    // ── VirtIO network device (the userspace TCP/IP NAT; CC-16) ──

    /// Read a VirtIO-MMIO register or net-config field of the network device.
    fn virtionet_read(&self, addr: u64) -> u64 {
        let Some(dev) = self.virtionet.as_ref() else {
            return 0;
        };
        let off = addr - VIRTIONET_BASE;
        match off {
            0x000 => 0x7472_6976, // MagicValue "virt"
            0x004 => 2,           // Version (modern)
            0x008 => 1,           // DeviceID = network
            0x00c => 0x554d_4551, // VendorID "QEMU"
            0x010 => match dev.device_features_sel {
                // word 0: VIRTIO_NET_F_MAC (bit 5); word 1: VERSION_1 (bit 32).
                0 => 0x20,
                1 => 1,
                _ => 0,
            },
            0x034 => 1024, // QueueNumMax
            0x044 => u64::from(dev.queue_ready[(dev.queue_sel.min(1)) as usize]),
            0x060 => u64::from(dev.interrupt_status),
            0x070 => u64::from(dev.status),
            0x0fc => 0, // ConfigGeneration
            // virtio_net_config: mac[6] at offset 0.
            _ if (0x100..0x106).contains(&off) => u64::from(dev.mac[(off - 0x100) as usize]),
            _ => 0,
        }
    }

    /// Write a VirtIO-MMIO register of the network device; a write to
    /// `QueueNotify` services the transmit queue (or tries to fill the receive
    /// queue). Queue registers apply to the currently selected queue
    /// (0 = receive, 1 = transmit).
    fn virtionet_write(&mut self, addr: u64, value: u32) {
        let off = addr - VIRTIONET_BASE;
        if self.virtionet.is_none() {
            return;
        }
        {
            let dev = self.virtionet.as_mut().unwrap();
            let q = (dev.queue_sel.min(1)) as usize;
            match off {
                0x014 => dev.device_features_sel = value,
                0x020 => {
                    let w = dev.driver_features_sel.min(1) as usize;
                    dev.driver_features[w] = value;
                }
                0x024 => dev.driver_features_sel = value,
                0x030 => dev.queue_sel = value,
                0x038 => dev.queue_num[q] = value,
                0x044 => dev.queue_ready[q] = value,
                0x064 => dev.interrupt_status &= !value,
                0x070 => dev.status = value,
                0x080 => dev.desc_addr[q] = (dev.desc_addr[q] & !0xffff_ffff) | u64::from(value),
                0x084 => {
                    dev.desc_addr[q] = (dev.desc_addr[q] & 0xffff_ffff) | (u64::from(value) << 32);
                }
                0x090 => dev.avail_addr[q] = (dev.avail_addr[q] & !0xffff_ffff) | u64::from(value),
                0x094 => {
                    dev.avail_addr[q] =
                        (dev.avail_addr[q] & 0xffff_ffff) | (u64::from(value) << 32);
                }
                0x0a0 => dev.used_addr[q] = (dev.used_addr[q] & !0xffff_ffff) | u64::from(value),
                0x0a4 => {
                    dev.used_addr[q] = (dev.used_addr[q] & 0xffff_ffff) | (u64::from(value) << 32);
                }
                _ => {}
            }
        }
        if off == 0x050 {
            // QueueNotify: `value` is the notified queue index. The guest filled
            // the transmit queue (1) with frames, or posted buffers to the
            // receive queue (0); either way, service the network.
            if value == 1 {
                self.virtionet_process_tx();
            } else {
                self.virtio_net_pump();
            }
        }
    }

    /// Service the transmit queue: for each frame the guest queued, strip the
    /// 12-byte `virtio_net_hdr` and hand the Ethernet frame to the NAT, then pump
    /// the NAT so any immediate replies (ARP, DHCP, SYN-ACK) reach the guest.
    fn virtionet_process_tx(&mut self) {
        let mut dev = self.virtionet.take().unwrap();
        let q = 1usize; // transmit
        let qsz = dev.queue_num[q] as u16;
        if dev.queue_ready[q] == 0 || qsz == 0 {
            self.virtionet = Some(dev);
            return;
        }
        let avail_idx = self.rd16(dev.avail_addr[q] + 2);
        while dev.last_avail[q] != avail_idx {
            let slot = dev.last_avail[q] % qsz;
            let head = self.rd16(dev.avail_addr[q] + 4 + 2 * u64::from(slot));
            let frame = self.virtionet_gather(&dev, q, head);
            // Strip the virtio_net_hdr_v1 (12 bytes) → the Ethernet frame.
            if frame.len() > 12 {
                dev.nat.on_guest_frame(&frame[12..], dev.egress.as_mut());
            }
            let used_idx = self.rd16(dev.used_addr[q] + 2);
            let ring = dev.used_addr[q] + 4 + 8 * u64::from(used_idx % qsz);
            self.wr32(ring, u32::from(head));
            self.wr32(ring + 4, frame.len() as u32);
            self.wr16(dev.used_addr[q] + 2, used_idx.wrapping_add(1));
            dev.last_avail[q] = dev.last_avail[q].wrapping_add(1);
            dev.interrupt_status |= 1;
            dev.irq_pending = true;
        }
        let raise = dev.irq_pending;
        dev.irq_pending = false;
        self.virtionet = Some(dev);
        if raise {
            self.plic.raise(VIRTIONET_IRQ);
        }
        // The transmitted frames may have produced replies — deliver them.
        self.virtio_net_pump();
    }

    /// Pump the NAT (pull host-side bytes + advance connection state) and deliver
    /// any pending receive frames into the guest's receive queue. Called on a
    /// receive-queue notify and periodically from the run loop (so host data
    /// arrives without the guest having to transmit first).
    fn virtio_net_pump(&mut self) {
        if self.virtionet.is_none() {
            return;
        }
        let mut dev = self.virtionet.take().unwrap();
        dev.nat.poll(dev.egress.as_mut());
        // Service forwarded-port (inbound) connections too (CC-21).
        let VirtioNet { nat, ingress, .. } = &mut dev;
        nat.poll_ingress(ingress.as_mut());
        let q = 0usize; // receive
        let qsz = dev.queue_num[q] as u16;
        let mut raise = false;
        if dev.queue_ready[q] != 0 && qsz != 0 {
            while dev.nat.has_rx() {
                let avail_idx = self.rd16(dev.avail_addr[q] + 2);
                if dev.last_avail[q] == avail_idx {
                    break; // the guest has posted no receive buffer
                }
                let frame = dev.nat.take_rx().unwrap();
                let slot = dev.last_avail[q] % qsz;
                let head = self.rd16(dev.avail_addr[q] + 4 + 2 * u64::from(slot));
                let written = self.virtionet_scatter_rx(&dev, q, head, &frame);
                let used_idx = self.rd16(dev.used_addr[q] + 2);
                let ring = dev.used_addr[q] + 4 + 8 * u64::from(used_idx % qsz);
                self.wr32(ring, u32::from(head));
                self.wr32(ring + 4, written);
                self.wr16(dev.used_addr[q] + 2, used_idx.wrapping_add(1));
                dev.last_avail[q] = dev.last_avail[q].wrapping_add(1);
                dev.interrupt_status |= 1;
                raise = true;
            }
        }
        self.virtionet = Some(dev);
        if raise {
            self.plic.raise(VIRTIONET_IRQ);
        }
    }

    /// Gather the bytes of a transmit descriptor chain (all descriptors carry
    /// guest-provided data: the `virtio_net_hdr` followed by the frame).
    fn virtionet_gather(&mut self, dev: &VirtioNet, q: usize, head: u16) -> Vec<u8> {
        const F_NEXT: u16 = 1;
        let mut out: Vec<u8> = Vec::new();
        let mut idx = head;
        let mut guard = 0u32;
        loop {
            let d = dev.desc_addr[q] + 16 * u64::from(idx);
            let addr = self.rd64(d);
            let len = self.rd32(d + 8);
            let flags = self.rd16(d + 12);
            let next = self.rd16(d + 14);
            for i in 0..u64::from(len) {
                out.push(self.rd8(addr + i));
            }
            guard += 1;
            if flags & F_NEXT == 0 || guard > dev.queue_num[q] {
                break;
            }
            idx = next;
        }
        out
    }

    /// Scatter a received frame — prefixed with a 12-byte `virtio_net_hdr_v1`
    /// (zeroed, `num_buffers = 1`) — into the writable descriptors of a receive
    /// chain. Returns the number of bytes written (the used-ring length).
    fn virtionet_scatter_rx(&mut self, dev: &VirtioNet, q: usize, head: u16, frame: &[u8]) -> u32 {
        const F_NEXT: u16 = 1;
        const F_WRITE: u16 = 2;
        // virtio_net_hdr_v1: 10 zero bytes then num_buffers = 1 (little-endian).
        let mut buf: Vec<u8> = Vec::with_capacity(12 + frame.len());
        buf.extend_from_slice(&[0u8; 10]);
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(frame);

        let mut idx = head;
        let mut pos = 0usize;
        let mut written = 0u32;
        let mut guard = 0u32;
        loop {
            let d = dev.desc_addr[q] + 16 * u64::from(idx);
            let addr = self.rd64(d);
            let len = self.rd32(d + 8);
            let flags = self.rd16(d + 12);
            let next = self.rd16(d + 14);
            if flags & F_WRITE != 0 && pos < buf.len() {
                let n = (len as usize).min(buf.len() - pos);
                for i in 0..n {
                    self.wr8(addr + i as u64, buf[pos + i]);
                }
                pos += n;
                written += n as u32;
            }
            guard += 1;
            if flags & F_NEXT == 0 || guard > dev.queue_num[q] || pos >= buf.len() {
                break;
            }
            idx = next;
        }
        written
    }

    /// Advance the timer and reconcile the memory-mapped interrupt latches into
    /// `mip` (CLINT → MTIP/MSIP), called once per executed instruction.
    fn tick(&mut self) {
        self.mtime = self.mtime.wrapping_add(1);
        let mut mip = self.raw_csr(csr::MIP);
        let orig_mip = mip;
        // The timer interrupt: in firmware (SBI) mode the SEE delivers it to the
        // supervisor (STIP) — what an S-mode kernel handles; otherwise it is the
        // machine timer (MTIP), as the conformance tests expect.
        let timer_bit = if self.provide_sbi {
            csr::STIP
        } else {
            csr::MTIP
        };
        if self.mtimecmp != 0 && self.mtime >= self.mtimecmp {
            mip |= 1 << timer_bit;
        } else {
            mip &= !(1 << timer_bit);
        }
        // Machine software interrupt latch.
        if self.msip {
            mip |= 1 << csr::MSIP;
        } else {
            mip &= !(1 << csr::MSIP);
        }
        // External interrupts from the PLIC: the S-mode context (1) drives SEIP
        // — the line a Linux kernel's PLIC driver services — and the M-mode
        // context (0) drives MEIP (RISC-V PLIC → hart external-interrupt line).
        // Only a machine with a device wired to the PLIC drives these bits; a
        // diskless machine leaves SEIP/MEIP software-managed (the privileged
        // riscv-tests write them directly — `CC-9` is unchanged).
        if self.virtio.is_some() || self.virtio9p.is_some() || self.virtionet.is_some() {
            if self.plic.pending_for(1) {
                mip |= 1 << csr::SEIP;
            } else {
                mip &= !(1 << csr::SEIP);
            }
            if self.plic.pending_for(0) {
                mip |= 1 << csr::MEIP;
            } else {
                mip &= !(1 << csr::MEIP);
            }
        }
        // Write back only when the pending bits actually changed — most
        // instructions leave `mip` untouched, so this skips a BTreeMap
        // write-traversal. The map ends in the same state either way, so the κ
        // snapshot stays byte-identical (re-storing an unchanged value is a no-op
        // for the serialized form).
        if mip != orig_mip {
            self.csrs.insert(csr::MIP, mip);
        }
    }

    /// Take the highest-priority enabled+pending interrupt, if any (RISC-V
    /// Privileged ISA §3.1.9): machine interrupts unless delegated (`mideleg`)
    /// to supervisor, each gated by the global enable for the current privilege.
    /// Returns `true` if an interrupt was taken.
    fn take_interrupt(&mut self) -> bool {
        let pending = self.raw_csr(csr::MIP) & self.raw_csr(csr::MIE);
        if pending == 0 {
            return false;
        }
        let mstatus = self.raw_csr(csr::MSTATUS);
        let mideleg = self.raw_csr(csr::MIDELEG);
        // Priority order (high → low): MEI, MSI, MTI, SEI, SSI, STI.
        const ORDER: [u32; 6] = [
            csr::MEIP,
            csr::MSIP,
            csr::MTIP,
            csr::SEIP,
            csr::SSIP,
            csr::STIP,
        ];
        for bit in ORDER {
            if pending & (1 << bit) == 0 {
                continue;
            }
            let to_s = (mideleg >> bit) & 1 != 0;
            let enabled = if to_s {
                self.priv_level < PRIV_S || (self.priv_level == PRIV_S && (mstatus >> 1) & 1 == 1)
            } else {
                self.priv_level < PRIV_M || (mstatus >> 3) & 1 == 1
            };
            if enabled {
                let cause = (1u64 << 63) | u64::from(bit);
                self.trap(cause, 0, self.hart.pc);
                return true;
            }
        }
        false
    }

    /// A guest virtual load (translate then read).
    fn load(&mut self, addr: u64, width: usize, access: Access) -> Result<u64, Trap> {
        // An access that stays within one 4 KiB page translates once (the common
        // case). One that *straddles* a page boundary must translate each page
        // independently: the two pages may map to non-adjacent physical frames
        // (e.g. a demand-paged userland), so reading `width` contiguous physical
        // bytes from a single translation would draw the spilled bytes from the
        // wrong frame. RISC-V permits unaligned (hence page-crossing) accesses.
        let off = (addr & 0xfff) as usize;
        if off + width <= 0x1000 {
            let pa = self.translate(addr, access)?;
            return self.load_phys(pa, width);
        }
        let first = 0x1000 - off;
        let pa1 = self.translate(addr, access)?;
        let pa2 = self.translate(addr.wrapping_add(first as u64), access)?;
        let lo = self.load_phys(pa1, first)?;
        let hi = self.load_phys(pa2, width - first)?;
        Ok(lo | (hi << (8 * first)))
    }

    /// A guest virtual store (translate then write).
    fn store(&mut self, addr: u64, width: usize, value: u64) -> Result<(), Trap> {
        let off = (addr & 0xfff) as usize;
        if off + width <= 0x1000 {
            let pa = self.translate(addr, Access::Store)?;
            return self.store_phys(pa, width, value);
        }
        // Page-straddling store: translate *both* pages before writing either, so
        // a fault on the second page leaves no partial write (the handler maps the
        // page and the instruction re-executes). `store_phys` writes the low
        // `width` bytes of its value, so the high page gets `value >> 8*first`.
        let first = 0x1000 - off;
        let pa1 = self.translate(addr, Access::Store)?;
        let pa2 = self.translate(addr.wrapping_add(first as u64), Access::Store)?;
        self.store_phys(pa1, first, value)?;
        self.store_phys(pa2, width - first, value >> (8 * first))
    }

    /// Invalidate the whole software TLB by moving to a new generation — O(1).
    /// Called on every change to the translation context (SFENCE.VMA, `satp`, a
    /// translation-relevant `mstatus`/`sstatus` write, a privilege transition).
    #[inline]
    fn tlb_flush(&mut self) {
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
    }

    /// Record a virtual-page → physical-frame translation for `access`. Called by
    /// [`Emulator::translate_walk`] only at a successful *paging* leaf — by which
    /// point the walk has already set the PTE's Accessed bit (and Dirty, for a
    /// store), so a later hit may legitimately skip the A/D write-back (it would
    /// be a no-op: the bit is already set). Keying on the access class keeps the
    /// fetch/load/store permission and A/D semantics distinct.
    #[inline]
    fn tlb_fill(&mut self, vaddr: u64, access: Access, pa: u64) {
        let vpn = vaddr >> 12;
        let set = (vpn & TLB_MASK) as usize;
        self.tlb[access as usize][set] = TlbEntry {
            tag: vpn,
            frame: pa & !0xfff,
            gen: self.tlb_gen,
        };
    }

    /// Translate a guest virtual address to physical, through the software TLB.
    /// A hit returns immediately with no CSR reads and no page-table walk; a miss
    /// (or bare/M-mode, which the walker passes through and does not cache) falls
    /// to [`Emulator::translate_walk`]. A hit is always valid for the current
    /// context because every context change flushes the TLB ([`Emulator::tlb_flush`]).
    fn translate(&mut self, vaddr: u64, access: Access) -> Result<u64, Trap> {
        let vpn = vaddr >> 12;
        let class = access as usize;
        let set = (vpn & TLB_MASK) as usize;
        let slot = self.tlb[class][set]; // `TlbEntry: Copy` — no borrow held
        if slot.gen == self.tlb_gen && slot.tag == vpn {
            let pa = slot.frame | (vaddr & 0xfff);
            // Shadow-verify (debug builds only): the full walk must agree with the
            // cached entry. If a flush site is ever missed, this fires immediately
            // under the CC-9/CC-14 differential suites (heavy paging). `translate_walk`
            // is idempotent for A/D (the bits are already set on a cached page), so
            // re-running it here has no observable effect.
            #[cfg(debug_assertions)]
            debug_assert_eq!(
                self.translate_walk(vaddr, access),
                Ok(pa),
                "TLB diverged from the page-table walk at va={vaddr:#x} {access:?}"
            );
            return Ok(pa);
        }
        self.translate_walk(vaddr, access)
    }

    /// Translate a virtual address through Sv39/Sv48/Sv57 paging (RISC-V Privileged ISA
    /// §4.3-4.6) when `satp.MODE` selects Sv39/Sv48/Sv57 and the effective privilege is below
    /// machine; otherwise the address is physical (bare mode). Sets the
    /// accessed/dirty bits and enforces the page permissions and U/SUM/MXR. On a
    /// successful paging translation it fills the software TLB ([`Emulator::tlb_fill`]).
    fn translate_walk(&mut self, vaddr: u64, access: Access) -> Result<u64, Trap> {
        let satp = self.raw_csr(0x180);
        // Sv39/Sv48/Sv57 differ only in the page-table depth (3/4/5 levels); a
        // modern kernel probes for the deepest the hart accepts, so all three are
        // implemented (RISC-V Privileged ISA §4.4-4.6).
        let levels: i32 = match satp >> 60 {
            8 => 3,
            9 => 4,
            10 => 5,
            _ => return Ok(vaddr), // bare (no paging)
        };
        let mstatus = self.raw_csr(csr::MSTATUS);
        // MPRV makes loads/stores use the previous privilege (MPP); fetches don't.
        let eff = if access != Access::Fetch && (mstatus >> 17) & 1 == 1 {
            ((mstatus >> 11) & 3) as u8
        } else {
            self.priv_level
        };
        if eff == PRIV_M {
            return Ok(vaddr);
        }
        let sum = (mstatus >> 18) & 1;
        let mxr = (mstatus >> 19) & 1;
        let pf = |a: Access| Trap::PageFault {
            cause: match a {
                Access::Fetch => 12,
                Access::Load => 13,
                Access::Store => 15,
            },
            addr: vaddr,
        };
        // The root page-table PPN is `satp` bits 43:0 (44 bits); bits 59:44 are
        // the ASID and must be masked off (RISC-V Privileged ISA §4.1.11) — a
        // kernel that uses a nonzero ASID would otherwise corrupt the root.
        let mut a = (satp & 0xfff_ffff_ffff) << 12;
        let mut level = levels - 1;
        loop {
            let vpn_l = (vaddr >> (12 + 9 * level)) & 0x1ff;
            let pte_addr = a + vpn_l * 8;
            let pte = self.load_phys(pte_addr, 8)?;
            let (v, r, w, x, u) = (
                pte & 1,
                (pte >> 1) & 1,
                (pte >> 2) & 1,
                (pte >> 3) & 1,
                (pte >> 4) & 1,
            );
            if v == 0 || (r == 0 && w == 1) {
                return Err(pf(access));
            }
            if r == 1 || x == 1 {
                // Leaf PTE: check permissions, U/SUM/MXR, and alignment.
                let perm = match access {
                    Access::Fetch => x == 1,
                    Access::Load => r == 1 || (mxr == 1 && x == 1),
                    Access::Store => w == 1,
                };
                if !perm {
                    return Err(pf(access));
                }
                if eff == 0 && u == 0 {
                    return Err(pf(access)); // U-mode needs a user page
                }
                if eff == PRIV_S && u == 1 && (access == Access::Fetch || sum == 0) {
                    return Err(pf(access)); // S-mode into a user page without SUM
                }
                let ppn = (pte >> 10) & 0xfff_ffff_ffff; // 44-bit PPN (bits 53:10)
                if level > 0 && ppn & ((1 << (9 * level)) - 1) != 0 {
                    return Err(pf(access)); // misaligned superpage
                }
                // Set Accessed (and Dirty on store).
                let mut npte = pte | (1 << 6);
                if access == Access::Store {
                    npte |= 1 << 7;
                }
                if npte != pte {
                    self.store_phys(pte_addr, 8, npte)?;
                }
                let mask = (1u64 << (12 + 9 * level)) - 1;
                let pa = ((ppn << 12) & !mask) | (vaddr & mask);
                self.tlb_fill(vaddr, access, pa);
                return Ok(pa);
            }
            a = ((pte >> 10) & 0xfff_ffff_ffff) << 12;
            level -= 1;
            if level < 0 {
                return Err(pf(access));
            }
        }
    }

    // ── registers (x0 is hard-wired zero) ──

    fn rd(&self, i: u32) -> u64 {
        self.hart.x[i as usize]
    }

    fn wr(&mut self, i: u32, v: u64) {
        if i != 0 {
            self.hart.x[i as usize] = v;
        }
    }

    /// Fetch, decode, and execute one instruction. A processor exception (an
    /// illegal instruction, a breakpoint, or an access fault) is *raised* into
    /// the guest's trap handler when one is installed (`mtvec` set) — the correct
    /// privileged behaviour a kernel and the official `rv64mi`/`rv64si` tests
    /// rely on; a flat program with no handler instead stops with `Halt::Trap`.
    fn step(&mut self) -> Result<(), Halt> {
        let pc = self.hart.pc;
        // Fetch: a 16-bit parcel whose low two bits are `11` is the start of a
        // 32-bit instruction; otherwise it is a compressed (C extension)
        // instruction, expanded to its 32-bit equivalent (RISC-V ISA §16). A
        // fetch fault (e.g. an unmapped page) is delivered to the trap handler
        // like any other exception — the kernel relies on a deliberate fetch
        // page fault to switch into its virtual mapping (head.S trampoline).
        let parcel = match self.load(pc, 2, Access::Fetch) {
            Ok(v) => v as u16,
            Err(t) => return self.raise(t, pc),
        };
        let (inst, ilen) = if parcel & 3 == 3 {
            match self.load(pc, 4, Access::Fetch) {
                Ok(v) => (v as u32, 4u64),
                Err(t) => return self.raise(t, pc),
            }
        } else {
            match expand_rvc(parcel) {
                Some(i) => (i, 2),
                None => return self.raise(Trap::IllegalInstruction(u32::from(parcel)), pc),
            }
        };
        match self.exec(inst, pc, ilen) {
            Err(Halt::Trap(t)) => self.raise(t, pc),
            other => other,
        }
    }

    /// Raise a processor exception: trap into the installed handler (`mtvec`)
    /// with the cause and `mtval`, or — for a flat program with no handler —
    /// stop with `Halt::Trap`.
    fn raise(&mut self, trap: Trap, epc: u64) -> Result<(), Halt> {
        let (cause, tval) = match &trap {
            Trap::IllegalInstruction(i) => (2, u64::from(*i)),
            Trap::Breakpoint => (3, epc),
            Trap::AccessFault(a) => (5, *a),
            Trap::PageFault { cause, addr } => (*cause, *addr),
            Trap::UnknownSyscall(_) => return Err(Halt::Trap(trap)),
        };
        // A flat program installs no trap vector at all → terminate.
        if self.csr_read(csr::MTVEC) == 0 && self.csr_read(csr::STVEC) == 0 {
            return Err(Halt::Trap(trap));
        }
        self.trap(cause, tval, epc);
        Ok(())
    }

    /// Execute a decoded instruction (`Err(Halt::Trap)` signals an exception the
    /// caller routes through [`raise`](Self::raise)).
    fn exec(&mut self, inst: u32, pc: u64, ilen: u64) -> Result<(), Halt> {
        let opcode = inst & 0x7f;
        let rd = (inst >> 7) & 0x1f;
        let rs1 = (inst >> 15) & 0x1f;
        let rs2 = (inst >> 20) & 0x1f;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = (inst >> 25) & 0x7f;
        let mut next = pc.wrapping_add(ilen);

        match opcode {
            0x37 => self.wr(rd, sext(u_imm(inst), 32)), // LUI
            0x17 => self.wr(rd, pc.wrapping_add(sext(u_imm(inst), 32))), // AUIPC
            0x6f => {
                // JAL
                self.wr(rd, next);
                next = pc.wrapping_add(j_imm(inst));
            }
            0x67 if funct3 == 0 => {
                // JALR
                let t = next;
                next = self.rd(rs1).wrapping_add(i_imm(inst)) & !1;
                self.wr(rd, t);
            }
            0x63 => {
                // BRANCH
                let (a, b) = (self.rd(rs1), self.rd(rs2));
                let take = match funct3 {
                    0 => a == b,                   // BEQ
                    1 => a != b,                   // BNE
                    4 => (a as i64) < (b as i64),  // BLT
                    5 => (a as i64) >= (b as i64), // BGE
                    6 => a < b,                    // BLTU
                    7 => a >= b,                   // BGEU
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
                if take {
                    next = pc.wrapping_add(b_imm(inst));
                }
            }
            0x03 => {
                // LOAD
                let addr = self.rd(rs1).wrapping_add(i_imm(inst));
                let ld = |s: &mut Self, w| s.load(addr, w, Access::Load).map_err(Halt::Trap);
                let v = match funct3 {
                    0 => sext(ld(self, 1)?, 8),  // LB
                    1 => sext(ld(self, 2)?, 16), // LH
                    2 => sext(ld(self, 4)?, 32), // LW
                    3 => ld(self, 8)?,           // LD
                    4 => ld(self, 1)?,           // LBU
                    5 => ld(self, 2)?,           // LHU
                    6 => ld(self, 4)?,           // LWU
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
                self.wr(rd, v);
            }
            0x23 => {
                // STORE
                let addr = self.rd(rs1).wrapping_add(s_imm(inst));
                let v = self.rd(rs2);
                // HTIF tohost (the riscv-tests / Linux SBI console channel): a
                // store to the configured address signals exit (bit0=1 ⇒ exit
                // code = value>>1; a console putchar otherwise).
                if self.htif == Some(addr) {
                    if v & 1 != 0 {
                        return Err(Halt::Exit(v >> 1));
                    }
                    return Ok(());
                }
                let width = match funct3 {
                    0 => 1, // SB
                    1 => 2, // SH
                    2 => 4, // SW
                    3 => 8, // SD
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
                self.store(addr, width, v).map_err(Halt::Trap)?;
            }
            0x13 => {
                // OP-IMM
                let a = self.rd(rs1);
                let imm = i_imm(inst);
                let v = match funct3 {
                    0 => a.wrapping_add(imm),                // ADDI
                    2 => ((a as i64) < (imm as i64)) as u64, // SLTI
                    3 => (a < imm) as u64,                   // SLTIU
                    4 => a ^ imm,                            // XORI
                    6 => a | imm,                            // ORI
                    7 => a & imm,                            // ANDI
                    1 => a << ((inst >> 20) & 0x3f),         // SLLI (6-bit shamt, RV64)
                    5 => {
                        let shamt = (inst >> 20) & 0x3f;
                        if funct7 & 0x20 != 0 {
                            ((a as i64) >> shamt) as u64 // SRAI
                        } else {
                            a >> shamt // SRLI
                        }
                    }
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
                self.wr(rd, v);
            }
            0x1b => {
                // OP-IMM-32 (word ops, result sign-extended from 32)
                let a = self.rd(rs1) as u32;
                let v = match funct3 {
                    0 => sext((a.wrapping_add(i_imm(inst) as u32)) as u64, 32), // ADDIW
                    1 => sext((a << ((inst >> 20) & 0x1f)) as u64, 32),         // SLLIW
                    5 => {
                        let shamt = (inst >> 20) & 0x1f;
                        if funct7 & 0x20 != 0 {
                            sext(((a as i32) >> shamt) as u64, 32) // SRAIW
                        } else {
                            sext((a >> shamt) as u64, 32) // SRLIW
                        }
                    }
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
                self.wr(rd, v);
            }
            0x33 => {
                // OP (RV64I + M)
                let (a, b) = (self.rd(rs1), self.rd(rs2));
                let v = if funct7 == 0x01 {
                    self.muldiv(funct3, a, b)
                } else {
                    match (funct3, funct7) {
                        (0, 0x00) => a.wrapping_add(b),                 // ADD
                        (0, 0x20) => a.wrapping_sub(b),                 // SUB
                        (1, 0x00) => a << (b & 0x3f),                   // SLL
                        (2, 0x00) => ((a as i64) < (b as i64)) as u64,  // SLT
                        (3, 0x00) => (a < b) as u64,                    // SLTU
                        (4, 0x00) => a ^ b,                             // XOR
                        (5, 0x00) => a >> (b & 0x3f),                   // SRL
                        (5, 0x20) => ((a as i64) >> (b & 0x3f)) as u64, // SRA
                        (6, 0x00) => a | b,                             // OR
                        (7, 0x00) => a & b,                             // AND
                        _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                    }
                };
                self.wr(rd, v);
            }
            0x3b => {
                // OP-32 (word ops + M, result sign-extended from 32)
                let (a, b) = (self.rd(rs1) as u32, self.rd(rs2) as u32);
                let v = if funct7 == 0x01 {
                    self.muldivw(funct3, a, b)
                } else {
                    match (funct3, funct7) {
                        (0, 0x00) => sext(a.wrapping_add(b) as u64, 32), // ADDW
                        (0, 0x20) => sext(a.wrapping_sub(b) as u64, 32), // SUBW
                        (1, 0x00) => sext((a << (b & 0x1f)) as u64, 32), // SLLW
                        (5, 0x00) => sext((a >> (b & 0x1f)) as u64, 32), // SRLW
                        (5, 0x20) => sext(((a as i32) >> (b & 0x1f)) as u64, 32), // SRAW
                        _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                    }
                };
                self.wr(rd, v);
            }
            0x2f => {
                // AMO (A extension): LR / SC / atomic read-modify-write.
                let width = match funct3 {
                    2 => 4,
                    3 => 8,
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
                let funct5 = funct7 >> 2;
                let addr = self.rd(rs1);
                match funct5 {
                    0x02 => {
                        // LR: load + set the reservation.
                        let v = self.load(addr, width, Access::Load).map_err(Halt::Trap)?;
                        self.reservation = Some(addr);
                        self.wr(rd, amo_extend(v, width));
                    }
                    0x03 => {
                        // SC: store iff the reservation holds; rd = 0 (ok) / 1 (fail).
                        if self.reservation == Some(addr) {
                            self.store(addr, width, self.rd(rs2)).map_err(Halt::Trap)?;
                            self.wr(rd, 0);
                        } else {
                            self.wr(rd, 1);
                        }
                        self.reservation = None;
                    }
                    _ => {
                        let old = self.load(addr, width, Access::Store).map_err(Halt::Trap)?;
                        let res = amo_op(funct5, old, self.rd(rs2), width);
                        self.store(addr, width, res).map_err(Halt::Trap)?;
                        self.wr(rd, amo_extend(old, width));
                    }
                }
            }
            0x07 => {
                // LOAD-FP: FLW (32) / FLD (64) — requires the FP unit on.
                if !self.fp_enabled() {
                    return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
                }
                let addr = self.rd(rs1).wrapping_add(i_imm(inst));
                match funct3 {
                    2 => {
                        let bits = self.load(addr, 4, Access::Load).map_err(Halt::Trap)?;
                        self.fwr(rd, 0xffff_ffff_0000_0000 | (bits & 0xffff_ffff));
                    }
                    3 => {
                        let bits = self.load(addr, 8, Access::Load).map_err(Halt::Trap)?;
                        self.fwr(rd, bits);
                    }
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                }
            }
            0x27 => {
                // STORE-FP: FSW (32) / FSD (64) — requires the FP unit on.
                if !self.fp_enabled() {
                    return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
                }
                let addr = self.rd(rs1).wrapping_add(s_imm(inst));
                match funct3 {
                    2 => self
                        .store(addr, 4, self.frd(rs2) & 0xffff_ffff)
                        .map_err(Halt::Trap)?,
                    3 => self.store(addr, 8, self.frd(rs2)).map_err(Halt::Trap)?,
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                }
            }
            0x53 | 0x43 | 0x47 | 0x4b | 0x4f if !self.fp_enabled() => {
                // OP-FP / FMADD family with the FP unit off → illegal.
                return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
            }
            0x53 => return self.fp_op(inst),
            0x43 | 0x47 | 0x4b | 0x4f => return self.fp_madd(inst, opcode),
            0x0f => { /* FENCE / FENCE.I — ordering no-op on this model */ }
            0x73 if funct3 == 0 => {
                // SYSTEM — ECALL / EBREAK / xRET set their own PC and return;
                // WFI / SFENCE.VMA are no-ops here and fall through to advance.
                match inst {
                    0x0000_0073 => return self.ecall(),                      // ECALL
                    0x0010_0073 => return Err(Halt::Trap(Trap::Breakpoint)), // EBREAK
                    0x3020_0073 => {
                        self.mret();
                        return Ok(());
                    } // MRET
                    0x1020_0073 => {
                        // SRET traps in S-mode when mstatus.TSR is set (RISC-V
                        // Privileged ISA §3.1.6.5).
                        if self.priv_level == PRIV_S && (self.raw_csr(csr::MSTATUS) >> 22) & 1 == 1
                        {
                            return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
                        }
                        self.sret();
                        return Ok(());
                    } // SRET
                    0x1050_0073 => {
                        // WFI: wait until an interrupt is pending. Rather than
                        // spin the idle loop (which would burn millions of host
                        // cycles advancing `mtime` one tick at a time), skip
                        // straight to the next armed timer compare when nothing
                        // else is pending — the next `tick` then latches the
                        // timer interrupt. A standard emulator idle optimization.
                        let pending = self.raw_csr(csr::MIP) & self.raw_csr(csr::MIE);
                        if pending == 0 && self.mtimecmp > self.mtime {
                            self.mtime = self.mtimecmp - 1;
                        }
                    }
                    _ if (inst >> 25) == 0x09 => {
                        // SFENCE.VMA traps in S-mode when mstatus.TVM is set
                        // (RISC-V Privileged ISA §3.1.6.5).
                        if self.priv_level == PRIV_S && (self.raw_csr(csr::MSTATUS) >> 20) & 1 == 1
                        {
                            return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
                        }
                        // The guest edited page tables and is ordering the cached
                        // translations to be discarded. A full flush is always a
                        // spec-legal superset of any rs1/rs2-selective flush, so we
                        // flush the whole software TLB regardless of the operands.
                        self.tlb_flush();
                    }
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                }
            }
            0x73 => {
                // SYSTEM — Zicsr: CSRRW/S/C and their immediate forms. The source
                // is a register (funct3 1-3) or a 5-bit zimm (funct3 5-7).
                let csr = (inst >> 20) & 0xfff;
                // The FP CSRs are accessible only when the FP unit is on.
                if matches!(csr, csr::FFLAGS | csr::FRM | csr::FCSR) && !self.fp_enabled() {
                    return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
                }
                // `satp` access traps in S-mode when mstatus.TVM is set (RISC-V
                // Privileged ISA §3.1.6.5).
                if csr == 0x180
                    && self.priv_level == PRIV_S
                    && (self.raw_csr(csr::MSTATUS) >> 20) & 1 == 1
                {
                    return Err(Halt::Trap(Trap::IllegalInstruction(inst)));
                }
                let old = self.csr_read(csr);
                let src = if funct3 & 0x4 != 0 {
                    u64::from(rs1) // zimm (the rs1 field)
                } else {
                    self.rd(rs1)
                };
                let write = match funct3 & 0x3 {
                    1 => Some(src),                    // CSRRW(I)
                    2 if rs1 != 0 => Some(old | src),  // CSRRS(I)
                    3 if rs1 != 0 => Some(old & !src), // CSRRC(I)
                    _ => None, // CSRRS/C with a zero source: no write (no side effects)
                };
                if let Some(v) = write {
                    self.csr_write(csr, v);
                }
                self.wr(rd, old);
            }
            _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
        }

        self.hart.pc = next;
        Ok(())
    }

    fn muldiv(&self, funct3: u32, a: u64, b: u64) -> u64 {
        match funct3 {
            0 => a.wrapping_mul(b),                                         // MUL
            1 => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,  // MULH
            2 => (((a as i64 as i128) * (b as u128 as i128)) >> 64) as u64, // MULHSU
            3 => (((a as u128) * (b as u128)) >> 64) as u64,                // MULHU
            4 => {
                if b == 0 {
                    u64::MAX
                } else if a == i64::MIN as u64 && b == u64::MAX {
                    a
                } else {
                    ((a as i64).wrapping_div(b as i64)) as u64
                }
            } // DIV
            5 => a.checked_div(b).unwrap_or(u64::MAX),                      // DIVU (÷0 ⇒ all ones)
            6 => {
                if b == 0 {
                    a
                } else if a == i64::MIN as u64 && b == u64::MAX {
                    0
                } else {
                    ((a as i64).wrapping_rem(b as i64)) as u64
                }
            } // REM
            7 => {
                if b == 0 {
                    a
                } else {
                    a % b
                }
            } // REMU
            _ => 0,
        }
    }

    fn muldivw(&self, funct3: u32, a: u32, b: u32) -> u64 {
        match funct3 {
            0 => sext(a.wrapping_mul(b) as u64, 32), // MULW
            4 => sext(
                if b == 0 {
                    u32::MAX as u64
                } else {
                    (a as i32).wrapping_div(b as i32) as u32 as u64
                },
                32,
            ), // DIVW
            5 => sext(a.checked_div(b).unwrap_or(u32::MAX) as u64, 32), // DIVUW (÷0 ⇒ all ones)
            6 => sext(
                if b == 0 {
                    a as u64
                } else {
                    (a as i32).wrapping_rem(b as i32) as u32 as u64
                },
                32,
            ), // REMW
            7 => sext(if b == 0 { a } else { a % b } as u64, 32), // REMUW
            _ => 0,
        }
    }

    /// The `ecall` boundary: a tiny Linux syscall surface (write / exit).
    fn ecall(&mut self) -> Result<(), Halt> {
        // In firmware (SBI) mode, a supervisor `ecall` is an SBI call serviced by
        // the emulator-as-SEE and returns to S-mode (the kernel boot path).
        if self.provide_sbi && self.priv_level == PRIV_S {
            return self.sbi_call();
        }
        // If the guest installed a machine-mode trap vector, `ecall` is a real
        // trap into its handler (a kernel / the riscv-tests environment): cause
        // 8/9/11 by the originating privilege. Otherwise it is the host syscall
        // boundary a flat program uses (write / exit).
        if self.csr_read(csr::MTVEC) != 0 || self.csr_read(csr::STVEC) != 0 {
            let cause = match self.priv_level {
                3 => 11, // ECALL from M
                1 => 9,  // ECALL from S
                _ => 8,  // ECALL from U
            };
            self.trap(cause, 0, self.hart.pc);
            return Ok(());
        }
        let num = self.rd(17); // a7
        match num {
            syscall::EXIT | syscall::EXIT_GROUP => Err(Halt::Exit(self.rd(10))),
            syscall::WRITE => {
                // write(fd=a0, buf=a1, len=a2) — fd 1/2 go to the console.
                let (_fd, buf, len) = (self.rd(10), self.rd(11), self.rd(12));
                for i in 0..len {
                    let byte = self
                        .load(buf.wrapping_add(i), 1, Access::Load)
                        .map_err(Halt::Trap)? as u8;
                    self.console.push(byte);
                }
                self.wr(10, len); // return bytes written
                self.hart.pc = self.hart.pc.wrapping_add(4);
                Ok(())
            }
            other => Err(Halt::Trap(Trap::UnknownSyscall(other))),
        }
    }

    /// Service a Supervisor Binary Interface (SBI) call from the S-mode kernel
    /// (the RISC-V SBI specification). `a7` is the extension ID, `a6` the
    /// function ID, `a0..a5` the arguments; the call returns in `a0` (error) and
    /// `a1` (value), and execution resumes after the `ecall` in S-mode. The
    /// console, timer, and shutdown services a minimal kernel needs are provided.
    fn sbi_call(&mut self) -> Result<(), Halt> {
        let (eid, fid) = (self.rd(17), self.rd(16));
        let (a0, a1, a2) = (self.rd(10), self.rd(11), self.rd(12));
        let mut err: u64 = 0; // SBI_SUCCESS
        let mut val: u64 = 0;
        match eid {
            // ── Legacy extensions (return only in a0) ──
            0x01 => self.console.push(a0 as u8), // console_putchar
            0x02 => {
                // console_getchar — return the next input byte (in a0) or -1.
                err = match self.console_getchar() {
                    Some(b) => u64::from(b),
                    None => u64::MAX,
                };
            }
            0x00 => self.set_timer(a0),        // set_timer
            0x08 => return Err(Halt::Exit(0)), // shutdown
            // ── Base extension (probe / identity) ──
            0x10 => match fid {
                0 => val = 0x0200_0000,                       // spec version 2.0
                3 => val = u64::from(self.sbi_supported(a0)), // probe_extension
                _ => {}                                       // impl id / mvendorid / … → 0
            },
            // ── TIME extension ──
            0x5449_4d45 => {
                if fid == 0 {
                    self.set_timer(a0);
                }
            }
            // ── Debug console (DBCN) ──
            0x4442_434e => match fid {
                0 => {
                    // console_write(num_bytes=a0, base_lo=a1, base_hi=a2)
                    let base = a1 | (a2 << 32);
                    for i in 0..a0 {
                        if let Ok(b) = self.load(base.wrapping_add(i), 1, Access::Load) {
                            self.console.push(b as u8);
                        }
                    }
                    val = a0;
                }
                1 => {
                    // console_read(num_bytes=a0, base_lo=a1, base_hi=a2) — fill
                    // guest memory with pending input, return the count read.
                    let base = a1 | (a2 << 32);
                    let mut read = 0u64;
                    while read < a0 {
                        let Some(b) = self.console_getchar() else {
                            break;
                        };
                        if self
                            .store(base.wrapping_add(read), 1, u64::from(b))
                            .is_err()
                        {
                            break;
                        }
                        read += 1;
                    }
                    val = read;
                }
                2 => self.console.push(a0 as u8), // console_write_byte
                _ => {}
            },
            // ── System reset ──
            0x5352_5354 => return Err(Halt::Exit(0)),
            // ── IPI / RFENCE / HSM (single hart): acknowledge success ──
            0x0073_5049 | 0x5246_4e43 | 0x4853_4d00..=0x4853_4dff => {}
            _ => err = (-2_i64) as u64, // ERR_NOT_SUPPORTED
        }
        self.wr(10, err);
        self.wr(11, val);
        self.hart.pc = self.hart.pc.wrapping_add(4);
        Ok(())
    }

    /// SBI timer: program `mtimecmp` and clear the pending supervisor timer
    /// interrupt (the kernel re-arms it each tick).
    fn set_timer(&mut self, when: u64) {
        self.mtimecmp = when;
        let mip = self.raw_csr(csr::MIP) & !(1 << csr::STIP);
        self.csrs.insert(csr::MIP, mip);
    }

    /// Whether an SBI extension ID is provided (for `probe_extension`).
    fn sbi_supported(&self, eid: u64) -> bool {
        matches!(
            eid,
            0x00 | 0x01 | 0x02 | 0x08 | 0x10 | 0x5449_4d45 | 0x4442_434e | 0x5352_5354
        )
    }

    /// Execute an OP-FP instruction (RISC-V F/D extension). The single-precision
    /// (`fmt`=0) and double-precision (`fmt`=1) forms share the structure; the
    /// arithmetic uses the native IEEE-754 ops and libm (the no_std float
    /// foundation hologram's `mathf` is built on) — deterministic across peers.
    fn fp_op(&mut self, inst: u32) -> Result<(), Halt> {
        let rd = (inst >> 7) & 0x1f;
        let rs1 = (inst >> 15) & 0x1f;
        let rs2 = (inst >> 20) & 0x1f;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = inst >> 25;
        let fmt = funct7 & 3; // 0 = single, 1 = double
        let funct5 = funct7 >> 2;
        let rm = self.rounding_mode(inst);
        let illegal = || Err(Halt::Trap(Trap::IllegalInstruction(inst)));
        match funct5 {
            0x00..=0x03 => {
                if fmt == 0 {
                    let (a, b) = (self.frd32(rs1), self.frd32(rs2));
                    let (r, f) = f32_binop(funct5, a, b, rm);
                    self.fwr32(rd, r);
                    self.set_fflags(f);
                } else {
                    let (a, b) = (self.frd64(rs1), self.frd64(rs2));
                    let (r, f) = f64_binop(funct5, a, b, rm);
                    self.fwr64(rd, r);
                    self.set_fflags(f);
                }
            }
            0x0b => {
                // FSQRT
                if fmt == 0 {
                    let (r, f) = f32_sqrt(self.frd32(rs1), rm);
                    self.fwr32(rd, r);
                    self.set_fflags(f);
                } else {
                    let (r, f) = f64_sqrt(self.frd64(rs1), rm);
                    self.fwr64(rd, r);
                    self.set_fflags(f);
                }
            }
            0x04 => {
                // FSGNJ / FSGNJN / FSGNJX — on the NaN-box-aware value (a
                // non-NaN-boxed single input reads as the canonical NaN).
                let sign = if fmt == 0 { 1u64 << 31 } else { 1u64 << 63 };
                let (a, b) = if fmt == 0 {
                    (
                        u64::from(self.frd32(rs1).to_bits()),
                        u64::from(self.frd32(rs2).to_bits()),
                    )
                } else {
                    (self.frd(rs1), self.frd(rs2))
                };
                let res = match funct3 {
                    0 => (a & !sign) | (b & sign),
                    1 => (a & !sign) | ((b & sign) ^ sign),
                    2 => a ^ (b & sign),
                    _ => return illegal(),
                };
                if fmt == 0 {
                    self.fwr(rd, 0xffff_ffff_0000_0000 | (res & 0xffff_ffff));
                } else {
                    self.fwr(rd, res);
                }
            }
            0x05 => {
                // FMIN / FMAX — a signaling NaN operand sets the invalid flag.
                if fmt == 0 {
                    let (a, b) = (self.frd32(rs1), self.frd32(rs2));
                    self.fwr32(rd, fp_minmax32(funct3 == 1, a, b));
                    self.set_fflags(if is_snan32(a) || is_snan32(b) {
                        fflag::NV
                    } else {
                        0
                    });
                } else {
                    let (a, b) = (self.frd64(rs1), self.frd64(rs2));
                    self.fwr64(rd, fp_minmax64(funct3 == 1, a, b));
                    self.set_fflags(if is_snan64(a) || is_snan64(b) {
                        fflag::NV
                    } else {
                        0
                    });
                }
            }
            0x14 => {
                // FLE / FLT / FEQ → integer register
                let (r, nv) = if fmt == 0 {
                    let (a, b) = (self.frd32(rs1), self.frd32(rs2));
                    let nv = cmp_nv(
                        funct3 == 2,
                        a.is_nan(),
                        b.is_nan(),
                        is_snan32(a),
                        is_snan32(b),
                    );
                    let r = match funct3 {
                        0 => a <= b,
                        1 => a < b,
                        2 => a == b,
                        _ => return illegal(),
                    };
                    (r, nv)
                } else {
                    let (a, b) = (self.frd64(rs1), self.frd64(rs2));
                    let nv = cmp_nv(
                        funct3 == 2,
                        a.is_nan(),
                        b.is_nan(),
                        is_snan64(a),
                        is_snan64(b),
                    );
                    let r = match funct3 {
                        0 => a <= b,
                        1 => a < b,
                        2 => a == b,
                        _ => return illegal(),
                    };
                    (r, nv)
                };
                self.wr(rd, u64::from(r));
                self.set_fflags(nv);
            }
            0x18 => {
                // FCVT integer ← float (rs2: 0 W, 1 WU, 2 L, 3 LU)
                let (x, nan) = if fmt == 0 {
                    let v = self.frd32(rs1);
                    (f64::from(v), v.is_nan())
                } else {
                    let v = self.frd64(rs1);
                    (v, v.is_nan())
                };
                let (v, f) = fp_to_int(x, rs2, rm, nan);
                self.wr(rd, v);
                self.set_fflags(f);
            }
            0x1a => {
                // FCVT float ← integer (rs2: 0 W, 1 WU, 2 L, 3 LU)
                let src = self.rd(rs1);
                if fmt == 0 {
                    let (r, f) = int_to_f32(src, rs2);
                    self.fwr32(rd, r);
                    self.set_fflags(f);
                } else {
                    let (r, f) = int_to_f64_flags(src, rs2, rm);
                    self.fwr64(rd, r);
                    self.set_fflags(f);
                }
            }
            0x08 => {
                // FCVT.S.D (fmt=0) / FCVT.D.S (fmt=1)
                if fmt == 0 {
                    let a = self.frd64(rs1);
                    if a.is_nan() {
                        self.fwr32(rd, canonical_nan32());
                        self.set_fflags(if is_snan64(a) { fflag::NV } else { 0 });
                    } else {
                        let (r, f) = round_to_f32(a, rm);
                        self.fwr32(rd, r);
                        self.set_fflags(f);
                    }
                } else {
                    // widening S→D is always exact (only NV on a signaling NaN).
                    let a = self.frd32(rs1);
                    if a.is_nan() {
                        self.fwr64(rd, canonical_nan64());
                        self.set_fflags(if is_snan32(a) { fflag::NV } else { 0 });
                    } else {
                        self.fwr64(rd, f64::from(a));
                    }
                }
            }
            0x1c => {
                // FMV.X.W/D (funct3=0) or FCLASS (funct3=1) → integer register
                if funct3 == 0 {
                    let v = if fmt == 0 {
                        sext(self.frd(rs1) & 0xffff_ffff, 32)
                    } else {
                        self.frd(rs1)
                    };
                    self.wr(rd, v);
                } else {
                    let v = if fmt == 0 {
                        fclass32(self.frd32(rs1))
                    } else {
                        fclass64(self.frd64(rs1))
                    };
                    self.wr(rd, v);
                }
            }
            0x1e => {
                // FMV.W.X / FMV.D.X : float register ← integer bits
                if fmt == 0 {
                    self.fwr(rd, 0xffff_ffff_0000_0000 | (self.rd(rs1) & 0xffff_ffff));
                } else {
                    self.fwr(rd, self.rd(rs1));
                }
            }
            _ => return illegal(),
        }
        self.hart.pc = self.hart.pc.wrapping_add(4);
        Ok(())
    }

    /// Execute a fused multiply-add (FMADD/FMSUB/FNMSUB/FNMADD) — the fused
    /// `a*b±c` a kernel and libc rely on, via libm's `fma` (correctly rounded,
    /// deterministic).
    fn fp_madd(&mut self, inst: u32, opcode: u32) -> Result<(), Halt> {
        let rd = (inst >> 7) & 0x1f;
        let rs1 = (inst >> 15) & 0x1f;
        let rs2 = (inst >> 20) & 0x1f;
        let funct7 = inst >> 25;
        let fmt = funct7 & 3;
        let rs3 = funct7 >> 2;
        let rm = self.rounding_mode(inst);
        // The two sign flips per form: FMADD a*b+c, FMSUB a*b−c, FNMSUB −(a*b)+c,
        // FNMADD −(a*b)−c.
        let (neg_ab, neg_c) = match opcode {
            0x43 => (false, false),
            0x47 => (false, true),
            0x4b => (true, false),
            _ => (true, true),
        };
        if fmt == 0 {
            let (mut a, b, mut c) = (self.frd32(rs1), self.frd32(rs2), self.frd32(rs3));
            if neg_ab {
                a = -a;
            }
            if neg_c {
                c = -c;
            }
            let (r, f) = f32_fma(a, b, c, rm);
            self.fwr32(rd, r);
            self.set_fflags(f);
        } else {
            let (mut a, b, mut c) = (self.frd64(rs1), self.frd64(rs2), self.frd64(rs3));
            if neg_ab {
                a = -a;
            }
            if neg_c {
                c = -c;
            }
            let (r, f) = f64_fma(a, b, c, rm);
            self.fwr64(rd, r);
            self.set_fflags(f);
        }
        self.hart.pc = self.hart.pc.wrapping_add(4);
        Ok(())
    }
}

// ── compressed instructions (RISC-V ISA §16) — expand a 16-bit parcel to its
//    32-bit equivalent, which the base decoder then executes ──

/// Sign-extend the low `bits` of `v` to a 32-bit value.
fn se(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

// 32-bit instruction-word builders (the base encodings the decoder reads back).
fn i_(imm: i32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    ((imm as u32 & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}
fn r_(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}
fn s_(imm: i32, rs2: u32, rs1: u32, f3: u32, op: u32) -> u32 {
    let i = imm as u32 & 0xfff;
    ((i >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | ((i & 0x1f) << 7) | op
}
fn b_(imm: i32, rs2: u32, rs1: u32, f3: u32, op: u32) -> u32 {
    let i = imm as u32;
    (((i >> 12) & 1) << 31)
        | (((i >> 5) & 0x3f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (f3 << 12)
        | (((i >> 1) & 0xf) << 8)
        | (((i >> 11) & 1) << 7)
        | op
}
fn j_(imm: i32, rd: u32, op: u32) -> u32 {
    let i = imm as u32;
    (((i >> 20) & 1) << 31)
        | (((i >> 1) & 0x3ff) << 21)
        | (((i >> 11) & 1) << 20)
        | (((i >> 12) & 0xff) << 12)
        | (rd << 7)
        | op
}

/// Expand a compressed (RVC) parcel to its 32-bit equivalent, or `None` if it is
/// reserved / a float form this core does not implement.
/// The RISC-V kernel `Image` load offset, read from the image's own header
/// (`Documentation/riscv/boot-image-header.rst`): `text_offset` is the
/// little-endian `u64` at byte 8, and the header carries the magic `RSC\x05` at
/// byte 56. Returns `None` when the bytes are not a RISC-V `Image` (so the boot
/// fails cleanly rather than loading at a guessed address).
fn image_text_offset(image: &[u8]) -> Option<u64> {
    let header = image.get(..64)?;
    if &header[56..60] != b"RSC\x05" {
        return None;
    }
    Some(u64::from_le_bytes(
        <[u8; 8]>::try_from(&header[8..16]).ok()?,
    ))
}

fn expand_rvc(half: u16) -> Option<u32> {
    let h = u32::from(half);
    let funct3 = (h >> 13) & 7;
    let rd = (h >> 7) & 0x1f; // also rs1 (CR/CI)
    let rs2 = (h >> 2) & 0x1f; // CR/CSS
    let rdp = ((h >> 7) & 7) + 8; // rd'/rs1'
    let rs2p = ((h >> 2) & 7) + 8; // rs2'
    match (h & 3, funct3) {
        // ── Quadrant 0 ──
        (0, 0) => {
            // C.ADDI4SPN → addi rd', x2, nzuimm
            let imm = (((h >> 7) & 0xf) << 6)
                | (((h >> 11) & 3) << 4)
                | (((h >> 5) & 1) << 3)
                | (((h >> 6) & 1) << 2);
            (imm != 0).then(|| i_(imm as i32, 2, 0, rs2p, 0x13))
        }
        (0, 1) => {
            // C.FLD → fld rd', off(rs1') (RV64GC compressed double load)
            let off = (((h >> 10) & 7) << 3) | (((h >> 5) & 3) << 6);
            Some(i_(off as i32, rdp, 3, rs2p, 0x07))
        }
        (0, 2) => {
            // C.LW → lw rd', off(rs1')
            let off = (((h >> 10) & 7) << 3) | (((h >> 6) & 1) << 2) | (((h >> 5) & 1) << 6);
            Some(i_(off as i32, rdp, 2, rs2p, 0x03))
        }
        (0, 3) => {
            // C.LD → ld rd', off(rs1')
            let off = (((h >> 10) & 7) << 3) | (((h >> 5) & 3) << 6);
            Some(i_(off as i32, rdp, 3, rs2p, 0x03))
        }
        (0, 5) => {
            // C.FSD → fsd rs2', off(rs1') (RV64GC compressed double store)
            let off = (((h >> 10) & 7) << 3) | (((h >> 5) & 3) << 6);
            Some(s_(off as i32, rs2p, rdp, 3, 0x27))
        }
        (0, 6) => {
            // C.SW → sw rs2', off(rs1')
            let off = (((h >> 10) & 7) << 3) | (((h >> 6) & 1) << 2) | (((h >> 5) & 1) << 6);
            Some(s_(off as i32, rs2p, rdp, 2, 0x23))
        }
        (0, 7) => {
            // C.SD → sd rs2', off(rs1')
            let off = (((h >> 10) & 7) << 3) | (((h >> 5) & 3) << 6);
            Some(s_(off as i32, rs2p, rdp, 3, 0x23))
        }
        // ── Quadrant 1 ──
        (1, 0) => {
            // C.ADDI (rd==0 ⇒ C.NOP) → addi rd, rd, imm
            let imm = se((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
            Some(i_(imm, rd, 0, rd, 0x13))
        }
        (1, 1) => {
            // C.ADDIW → addiw rd, rd, imm (rd != 0)
            let imm = se((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
            (rd != 0).then(|| i_(imm, rd, 0, rd, 0x1b))
        }
        (1, 2) => {
            // C.LI → addi rd, x0, imm
            let imm = se((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
            Some(i_(imm, 0, 0, rd, 0x13))
        }
        (1, 3) if rd == 2 => {
            // C.ADDI16SP → addi x2, x2, nzimm
            let imm = se(
                (((h >> 12) & 1) << 9)
                    | (((h >> 3) & 3) << 7)
                    | (((h >> 5) & 1) << 6)
                    | (((h >> 2) & 1) << 5)
                    | (((h >> 6) & 1) << 4),
                10,
            );
            (imm != 0).then(|| i_(imm, 2, 0, 2, 0x13))
        }
        (1, 3) => {
            // C.LUI → lui rd, nzimm
            let imm = se((((h >> 12) & 1) << 17) | (((h >> 2) & 0x1f) << 12), 18);
            (imm != 0 && rd != 0).then_some((imm as u32 & 0xffff_f000) | (rd << 7) | 0x37)
        }
        (1, 4) => {
            // MISC-ALU on rd'
            let funct2 = (h >> 10) & 3;
            match funct2 {
                0 => {
                    // C.SRLI
                    let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f);
                    Some(i_(shamt as i32, rdp, 5, rdp, 0x13))
                }
                1 => {
                    // C.SRAI (funct7 0x20)
                    let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f);
                    Some(i_((0x400 | shamt) as i32, rdp, 5, rdp, 0x13))
                }
                2 => {
                    // C.ANDI
                    let imm = se((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
                    Some(i_(imm, rdp, 7, rdp, 0x13))
                }
                _ => {
                    // register-register
                    let bit12 = (h >> 12) & 1;
                    match (bit12, (h >> 5) & 3) {
                        (0, 0) => Some(r_(0x20, rs2p, rdp, 0, rdp, 0x33)), // C.SUB
                        (0, 1) => Some(r_(0, rs2p, rdp, 4, rdp, 0x33)),    // C.XOR
                        (0, 2) => Some(r_(0, rs2p, rdp, 6, rdp, 0x33)),    // C.OR
                        (0, 3) => Some(r_(0, rs2p, rdp, 7, rdp, 0x33)),    // C.AND
                        (1, 0) => Some(r_(0x20, rs2p, rdp, 0, rdp, 0x3b)), // C.SUBW
                        (1, 1) => Some(r_(0, rs2p, rdp, 0, rdp, 0x3b)),    // C.ADDW
                        _ => None,
                    }
                }
            }
        }
        (1, 5) => {
            // C.J → jal x0, off
            let off = se(
                (((h >> 12) & 1) << 11)
                    | (((h >> 11) & 1) << 4)
                    | (((h >> 9) & 3) << 8)
                    | (((h >> 8) & 1) << 10)
                    | (((h >> 7) & 1) << 6)
                    | (((h >> 6) & 1) << 7)
                    | (((h >> 3) & 7) << 1)
                    | (((h >> 2) & 1) << 5),
                12,
            );
            Some(j_(off, 0, 0x6f))
        }
        (1, 6) | (1, 7) => {
            // C.BEQZ / C.BNEZ → beq/bne rs1', x0, off
            let off = se(
                (((h >> 12) & 1) << 8)
                    | (((h >> 10) & 3) << 3)
                    | (((h >> 5) & 3) << 6)
                    | (((h >> 3) & 3) << 1)
                    | (((h >> 2) & 1) << 5),
                9,
            );
            let f3 = if funct3 == 6 { 0 } else { 1 };
            Some(b_(off, 0, rdp, f3, 0x63))
        }
        // ── Quadrant 2 ──
        (2, 0) => {
            // C.SLLI → slli rd, rd, shamt
            let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f);
            Some(i_(shamt as i32, rd, 1, rd, 0x13))
        }
        (2, 1) => {
            // C.FLDSP → fld rd, off(x2) (RV64GC compressed double load from SP)
            let off = (((h >> 12) & 1) << 5) | (((h >> 5) & 3) << 3) | (((h >> 2) & 7) << 6);
            Some(i_(off as i32, 2, 3, rd, 0x07))
        }
        (2, 2) => {
            // C.LWSP → lw rd, off(x2)
            let off = (((h >> 12) & 1) << 5) | (((h >> 4) & 7) << 2) | (((h >> 2) & 3) << 6);
            (rd != 0).then(|| i_(off as i32, 2, 2, rd, 0x03))
        }
        (2, 3) => {
            // C.LDSP → ld rd, off(x2)
            let off = (((h >> 12) & 1) << 5) | (((h >> 5) & 3) << 3) | (((h >> 2) & 7) << 6);
            (rd != 0).then(|| i_(off as i32, 2, 3, rd, 0x03))
        }
        (2, 4) => {
            if (h >> 12) & 1 == 0 {
                if rs2 == 0 {
                    (rd != 0).then(|| i_(0, rd, 0, 0, 0x67)) // C.JR
                } else {
                    Some(r_(0, rs2, 0, 0, rd, 0x33)) // C.MV
                }
            } else if rd == 0 && rs2 == 0 {
                Some(0x0010_0073) // C.EBREAK
            } else if rs2 == 0 {
                Some(i_(0, rd, 0, 1, 0x67)) // C.JALR
            } else {
                Some(r_(0, rs2, rd, 0, rd, 0x33)) // C.ADD
            }
        }
        (2, 5) => {
            // C.FSDSP → fsd rs2, off(x2) (RV64GC compressed double store to SP)
            let off = (((h >> 10) & 7) << 3) | (((h >> 7) & 7) << 6);
            Some(s_(off as i32, rs2, 2, 3, 0x27))
        }
        (2, 6) => {
            // C.SWSP → sw rs2, off(x2)
            let off = (((h >> 9) & 0xf) << 2) | (((h >> 7) & 3) << 6);
            Some(s_(off as i32, rs2, 2, 2, 0x23))
        }
        (2, 7) => {
            // C.SDSP → sd rs2, off(x2)
            let off = (((h >> 10) & 7) << 3) | (((h >> 7) & 7) << 6);
            Some(s_(off as i32, rs2, 2, 3, 0x23))
        }
        _ => None,
    }
}

// ── floating-point helpers (F/D extension) ──
//
// The arithmetic computes correctly-rounded results and the IEEE-754 accrued
// exception flags on the libm foundation hologram's float kernels use: single
// precision is computed exactly in `f64` (`f32` add/sub/mul/fma are exact there)
// then rounded to `f32` with the instruction's rounding mode; double precision
// computes the round-to-nearest result natively and recovers the exact rounding
// residual via an error-free transformation (`fma`), giving the inexact flag and
// the direction for the directed rounding modes. Deterministic across peers.

/// Exception-flag bits (RISC-V Unprivileged ISA §11.2).
mod fflag {
    pub const NX: u8 = 0x01; // inexact
    pub const UF: u8 = 0x02; // underflow
    pub const OF: u8 = 0x04; // overflow
    pub const DZ: u8 = 0x08; // divide by zero
    pub const NV: u8 = 0x10; // invalid operation
}

const RNE: u32 = 0;
const RTZ: u32 = 1;
const RDN: u32 = 2;
const RUP: u32 = 3;
const RMM: u32 = 4;

fn canonical_nan32() -> f32 {
    f32::from_bits(0x7fc0_0000)
}
fn canonical_nan64() -> f64 {
    f64::from_bits(0x7ff8_0000_0000_0000)
}
fn is_snan32(x: f32) -> bool {
    let b = x.to_bits();
    b & 0x7f80_0000 == 0x7f80_0000 && b & 0x007f_ffff != 0 && b & 0x0040_0000 == 0
}
fn is_snan64(x: f64) -> bool {
    let b = x.to_bits();
    b & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000
        && b & 0x000f_ffff_ffff_ffff != 0
        && b & 0x0008_0000_0000_0000 == 0
}

/// Round an *exact* real value (held in an `f64`) to `f32` with rounding mode
/// `rm`, returning the value and the accrued flags (NX/OF/UF).
fn round_to_f32(exact: f64, rm: u32) -> (f32, u8) {
    if exact == 0.0 {
        return (exact as f32, 0);
    }
    let rne = exact as f32; // native round-to-nearest-even
    if exact.is_infinite() {
        return (rne, 0);
    }
    if f64::from(rne) == exact {
        return (rne, 0); // exact
    }
    let toward = if exact > f64::from(rne) {
        libm::nextafterf(rne, f32::INFINITY)
    } else {
        libm::nextafterf(rne, f32::NEG_INFINITY)
    };
    let res = pick_directed(
        f64::from(rne),
        rne,
        f64::from(toward),
        toward,
        exact,
        exact > 0.0,
        rm,
    );
    (res, round_flags_f32(res, exact))
}

fn round_flags_f32(res: f32, exact: f64) -> u8 {
    let mut f = fflag::NX;
    if res.is_infinite() {
        f |= fflag::OF;
    } else if (res == 0.0 && exact != 0.0) || (res != 0.0 && res.abs() < f32::MIN_POSITIVE) {
        f |= fflag::UF;
    }
    f
}

/// Pick the directed-rounding result from the two `f32` candidates bracketing
/// the exact value (`r` = round-to-nearest, `toward` = the adjacent value toward
/// the exact value).
fn pick_directed(
    rv: f64,
    r: f32,
    tv: f64,
    toward: f32,
    exact: f64,
    positive: bool,
    rm: u32,
) -> f32 {
    let (lo, hi) = if rv < tv { (r, toward) } else { (toward, r) };
    match rm {
        RTZ => {
            if positive {
                lo
            } else {
                hi
            }
        }
        RDN => lo,
        RUP => hi,
        RMM => {
            // ties-to-max-magnitude: only differs from RNE on an exact tie.
            let (elo, ehi) = (f64::from(lo), f64::from(hi));
            if exact - elo == ehi - exact {
                if lo.abs() > hi.abs() {
                    lo
                } else {
                    hi
                }
            } else {
                r
            }
        }
        _ => r, // RNE
    }
}

/// Single-precision add/sub/mul/div with flags (computed exactly in `f64`).
fn f32_binop(funct5: u32, a: f32, b: f32, rm: u32) -> (f32, u8) {
    if a.is_nan() || b.is_nan() {
        let nv = if is_snan32(a) || is_snan32(b) {
            fflag::NV
        } else {
            0
        };
        return (canonical_nan32(), nv);
    }
    let (af, bf) = (f64::from(a), f64::from(b));
    match funct5 {
        3 => {
            if b == 0.0 {
                return if a == 0.0 {
                    (canonical_nan32(), fflag::NV) // 0/0
                } else {
                    let s = (a.is_sign_negative() ^ b.is_sign_negative()) as u32;
                    (f32::from_bits((s << 31) | 0x7f80_0000), fflag::DZ) // x/0 = ±inf
                };
            }
            round_to_f32(af / bf, rm)
        }
        2 if (a == 0.0 && b.is_infinite()) || (a.is_infinite() && b == 0.0) => {
            (canonical_nan32(), fflag::NV) // 0*inf
        }
        1 | 0 if a.is_infinite() && b.is_infinite() && ((funct5 == 1) == (a == b)) => {
            (canonical_nan32(), fflag::NV) // inf-inf / (-inf)+inf
        }
        _ => {
            let exact = match funct5 {
                0 => af + bf,
                1 => af - bf,
                _ => af * bf,
            };
            round_to_f32(exact, rm)
        }
    }
}

/// Double-precision add/sub/mul/div with flags. The round-to-nearest result is
/// native; the exact rounding residual (for inexact + directed rounding) comes
/// from an error-free transformation.
fn f64_binop(funct5: u32, a: f64, b: f64, rm: u32) -> (f64, u8) {
    if a.is_nan() || b.is_nan() {
        let nv = if is_snan64(a) || is_snan64(b) {
            fflag::NV
        } else {
            0
        };
        return (canonical_nan64(), nv);
    }
    match funct5 {
        3 => {
            if b == 0.0 {
                return if a == 0.0 {
                    (canonical_nan64(), fflag::NV)
                } else {
                    let s = (a.is_sign_negative() ^ b.is_sign_negative()) as u64;
                    (f64::from_bits((s << 63) | 0x7ff0_0000_0000_0000), fflag::DZ)
                };
            }
            let r = a / b;
            let residual = libm::fma(-r, b, a); // a - r*b (the rounding direction)
            finalize_f64(r, residual, rm)
        }
        2 if (a == 0.0 && b.is_infinite()) || (a.is_infinite() && b == 0.0) => {
            (canonical_nan64(), fflag::NV)
        }
        1 | 0 if a.is_infinite() && b.is_infinite() && ((funct5 == 1) == (a == b)) => {
            (canonical_nan64(), fflag::NV)
        }
        0 => {
            let r = a + b;
            let residual = two_sum_err(a, b, r);
            finalize_f64(r, residual, rm)
        }
        1 => {
            let r = a - b;
            let residual = two_sum_err(a, -b, r);
            finalize_f64(r, residual, rm)
        }
        _ => {
            let r = a * b;
            let residual = if r.is_finite() {
                libm::fma(a, b, -r)
            } else {
                0.0
            };
            finalize_f64(r, residual, rm)
        }
    }
}

/// The exact rounding error of `a+b` (Knuth's TwoSum): `(a+b) = r + err`.
fn two_sum_err(a: f64, b: f64, r: f64) -> f64 {
    if !r.is_finite() {
        return 0.0;
    }
    let bv = r - a;
    let av = r - bv;
    (a - av) + (b - bv)
}

/// Finalize a double-precision result from its round-to-nearest value `r` and
/// the (sign of the) exact residual, applying the rounding mode and flags.
fn finalize_f64(r: f64, residual: f64, rm: u32) -> (f64, u8) {
    if residual == 0.0 || !r.is_finite() {
        let f = if r.is_infinite() && residual != 0.0 {
            fflag::OF | fflag::NX
        } else {
            0
        };
        return (r, f);
    }
    let toward = if residual > 0.0 {
        libm::nextafter(r, f64::INFINITY)
    } else {
        libm::nextafter(r, f64::NEG_INFINITY)
    };
    let positive = r > 0.0 || (r == 0.0 && residual > 0.0);
    let (lo, hi) = if r < toward { (r, toward) } else { (toward, r) };
    let res = match rm {
        RTZ => {
            if positive {
                lo
            } else {
                hi
            }
        }
        RDN => lo,
        RUP => hi,
        RMM => {
            // tie (residual is exactly half an ulp) → max magnitude.
            if libm::fabs(residual) * 2.0 == libm::fabs(toward - r) {
                if libm::fabs(lo) > libm::fabs(hi) {
                    lo
                } else {
                    hi
                }
            } else {
                r
            }
        }
        _ => r,
    };
    let mut f = fflag::NX;
    if res.is_infinite() {
        f |= fflag::OF;
    } else if res != 0.0 && libm::fabs(res) < f64::MIN_POSITIVE {
        f |= fflag::UF;
    }
    (res, f)
}

fn f32_sqrt(a: f32, rm: u32) -> (f32, u8) {
    if a.is_nan() {
        return (canonical_nan32(), if is_snan32(a) { fflag::NV } else { 0 });
    }
    if a < 0.0 {
        return (canonical_nan32(), fflag::NV);
    }
    if a == 0.0 {
        return (a, 0);
    }
    round_to_f32(libm::sqrt(f64::from(a)), rm)
}

fn f64_sqrt(a: f64, rm: u32) -> (f64, u8) {
    if a.is_nan() {
        return (canonical_nan64(), if is_snan64(a) { fflag::NV } else { 0 });
    }
    if a < 0.0 {
        return (canonical_nan64(), fflag::NV);
    }
    if a == 0.0 {
        return (a, 0);
    }
    let r = libm::sqrt(a);
    finalize_f64(r, libm::fma(-r, r, a), rm)
}

fn f32_fma(a: f32, b: f32, c: f32, rm: u32) -> (f32, u8) {
    if a.is_nan() || b.is_nan() || c.is_nan() {
        let nv = if is_snan32(a) || is_snan32(b) || is_snan32(c) {
            fflag::NV
        } else {
            0
        };
        return (canonical_nan32(), nv);
    }
    if (a == 0.0 && b.is_infinite()) || (a.is_infinite() && b == 0.0) {
        return (canonical_nan32(), fflag::NV); // 0 * inf
    }
    round_to_f32(f64::from(a) * f64::from(b) + f64::from(c), rm)
}

fn f64_fma(a: f64, b: f64, c: f64, rm: u32) -> (f64, u8) {
    if a.is_nan() || b.is_nan() || c.is_nan() {
        let nv = if is_snan64(a) || is_snan64(b) || is_snan64(c) {
            fflag::NV
        } else {
            0
        };
        return (canonical_nan64(), nv);
    }
    if (a == 0.0 && b.is_infinite()) || (a.is_infinite() && b == 0.0) {
        return (canonical_nan64(), fflag::NV);
    }
    let r = libm::fma(a, b, c);
    if !r.is_finite() {
        return (r, 0);
    }
    // a*b+c exactly = (p + pe) + c via error-free transformations; the residual
    // gives inexact and the directed-rounding direction.
    let p = a * b;
    let pe = if p.is_finite() {
        libm::fma(a, b, -p)
    } else {
        0.0
    };
    let s = p + c;
    let se = two_sum_err(p, c, s);
    finalize_f64(r, ((s - r) + se) + pe, rm)
}

/// Round a float to an integral-valued float by the rounding mode.
fn round_to_integer(v: f64, rm: u32) -> f64 {
    match rm {
        RTZ => libm::trunc(v),
        RDN => libm::floor(v),
        RUP => libm::ceil(v),
        RMM => libm::round(v), // ties away from zero
        _ => libm::rint(v),    // RNE: ties to even
    }
}

/// Float → integer convert with the rounding mode and flags (NX inexact, NV
/// out-of-range / NaN). `sel`: 0=W, 1=WU, 2=L, 3=LU.
fn fp_to_int(x: f64, sel: u32, rm: u32, nan: bool) -> (u64, u8) {
    if nan {
        let v = match sel {
            0 => sext(u64::from(i32::MAX as u32), 32),
            1 => sext(u64::from(u32::MAX), 32),
            2 => i64::MAX as u64,
            _ => u64::MAX,
        };
        return (v, fflag::NV);
    }
    let r = round_to_integer(x, rm);
    let inexact = if r == x { 0 } else { fflag::NX };
    let (lo, hi, vmin, vmax): (f64, f64, u64, u64) = match sel {
        0 => (
            i32::MIN as f64,
            i32::MAX as f64,
            sext(u64::from(i32::MIN as u32), 32),
            sext(u64::from(i32::MAX as u32), 32),
        ),
        1 => (0.0, u32::MAX as f64, 0, sext(u64::from(u32::MAX), 32)),
        2 => (
            i64::MIN as f64,
            i64::MAX as f64,
            i64::MIN as u64,
            i64::MAX as u64,
        ),
        _ => (0.0, u64::MAX as f64, 0, u64::MAX),
    };
    if r < lo {
        return (vmin, fflag::NV);
    }
    if r > hi {
        return (vmax, fflag::NV);
    }
    let v = match sel {
        0 => sext((r as i32) as u32 as u64, 32),
        1 => sext(u64::from(r as u32), 32),
        2 => r as i64 as u64,
        _ => r as u64,
    };
    (v, inexact)
}

/// Integer → float convert; sets inexact when the integer is not exactly
/// representable. `sel`: 0=W, 1=WU, 2=L, 3=LU.
fn int_to_f32(src: u64, sel: u32) -> (f32, u8) {
    let exact: f64 = match sel {
        0 => f64::from(src as i32),
        1 => f64::from(src as u32),
        2 => src as i64 as f64,
        _ => src as f64,
    };
    round_to_f32(exact, RNE)
}

fn int_to_f64_flags(src: u64, sel: u32, rm: u32) -> (f64, u8) {
    match sel {
        0 => (f64::from(src as i32), 0),
        1 => (f64::from(src as u32), 0),
        2 => {
            let i = src as i64;
            let r = i as f64;
            (r, if r as i64 == i { 0 } else { fflag::NX })
        }
        _ => {
            let r = src as f64;
            let residual = if r as u64 >= src {
                f64::from(u8::from(r as u64 != src))
            } else {
                -1.0
            };
            finalize_f64(r, residual, rm)
        }
    }
}

/// Comparison invalid flag: `feq` signals only on a signaling NaN; `flt`/`fle`
/// signal on any NaN.
fn cmp_nv(quiet: bool, a_nan: bool, b_nan: bool, a_snan: bool, b_snan: bool) -> u8 {
    let signal = if quiet {
        a_snan || b_snan
    } else {
        a_nan || b_nan
    };
    if signal {
        fflag::NV
    } else {
        0
    }
}

/// RISC-V `fmin`/`fmax`: a NaN operand is ignored (the other is returned), both
/// NaN gives the canonical NaN, and −0.0 is ordered below +0.0.
fn fp_minmax32(is_max: bool, a: f32, b: f32) -> f32 {
    if a.is_nan() && b.is_nan() {
        return f32::from_bits(0x7fc0_0000);
    }
    if a.is_nan() {
        return b;
    }
    if b.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 {
        let a_neg = a.is_sign_negative();
        return if is_max == a_neg { b } else { a };
    }
    if is_max == (a > b) {
        a
    } else {
        b
    }
}

fn fp_minmax64(is_max: bool, a: f64, b: f64) -> f64 {
    if a.is_nan() && b.is_nan() {
        return f64::from_bits(0x7ff8_0000_0000_0000);
    }
    if a.is_nan() {
        return b;
    }
    if b.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 {
        let a_neg = a.is_sign_negative();
        return if is_max == a_neg { b } else { a };
    }
    if is_max == (a > b) {
        a
    } else {
        b
    }
}

/// `fclass` — the 10-bit classification mask (RISC-V Unprivileged ISA §11.9).
fn fclass32(x: f32) -> u64 {
    let bits = x.to_bits();
    let (sign, exp, frac) = (bits >> 31, (bits >> 23) & 0xff, bits & 0x7f_ffff);
    fclass_bits(
        sign != 0,
        exp == 0xff,
        exp == 0,
        frac == 0,
        frac & 0x40_0000 != 0,
    )
}

fn fclass64(x: f64) -> u64 {
    let bits = x.to_bits();
    let (sign, exp, frac) = (bits >> 63, (bits >> 52) & 0x7ff, bits & 0xf_ffff_ffff_ffff);
    fclass_bits(
        sign != 0,
        exp == 0x7ff,
        exp == 0,
        frac == 0,
        frac & 0x8_0000_0000_0000 != 0,
    )
}

fn fclass_bits(neg: bool, max_exp: bool, zero_exp: bool, zero_frac: bool, quiet: bool) -> u64 {
    let idx = if max_exp && !zero_frac {
        if quiet {
            9
        } else {
            8
        } // qNaN / sNaN
    } else if max_exp {
        if neg {
            0
        } else {
            7
        } // ±inf
    } else if zero_exp && zero_frac {
        if neg {
            3
        } else {
            4
        } // ±0
    } else if zero_exp {
        if neg {
            2
        } else {
            5
        } // ±subnormal
    } else if neg {
        1
    } else {
        6
    }; // ±normal
    1 << idx
}

// ── immediate decoders (RISC-V Unprivileged ISA §2.3) ──

/// Sign-extend the low `bits` of `v` to 64 bits.
fn sext(v: u64, bits: u32) -> u64 {
    let shift = 64 - bits;
    (((v << shift) as i64) >> shift) as u64
}

/// The destination value of an AMO/LR (the loaded old value), sign-extended from
/// the access width per the A extension (`amo.w` returns a sign-extended word).
fn amo_extend(v: u64, width: usize) -> u64 {
    if width == 4 {
        sext(v, 32)
    } else {
        v
    }
}

/// Apply an atomic memory operation, returning the value to store (truncated to
/// the access width by `store`). RISC-V "A" extension, `funct5` from bits 31..27.
fn amo_op(funct5: u32, old: u64, val: u64, width: usize) -> u64 {
    if width == 4 {
        let (o, v) = (old as u32, val as u32);
        u64::from(match funct5 {
            0x00 => o.wrapping_add(v),                         // AMOADD.W
            0x01 => v,                                         // AMOSWAP.W
            0x04 => o ^ v,                                     // AMOXOR.W
            0x08 => o | v,                                     // AMOOR.W
            0x0c => o & v,                                     // AMOAND.W
            0x10 => core::cmp::min(o as i32, v as i32) as u32, // AMOMIN.W
            0x14 => core::cmp::max(o as i32, v as i32) as u32, // AMOMAX.W
            0x18 => core::cmp::min(o, v),                      // AMOMINU.W
            0x1c => core::cmp::max(o, v),                      // AMOMAXU.W
            _ => o,
        })
    } else {
        match funct5 {
            0x00 => old.wrapping_add(val),
            0x01 => val,
            0x04 => old ^ val,
            0x08 => old | val,
            0x0c => old & val,
            0x10 => core::cmp::min(old as i64, val as i64) as u64,
            0x14 => core::cmp::max(old as i64, val as i64) as u64,
            0x18 => core::cmp::min(old, val),
            0x1c => core::cmp::max(old, val),
            _ => old,
        }
    }
}

fn i_imm(inst: u32) -> u64 {
    sext((inst >> 20) as u64, 12)
}

fn s_imm(inst: u32) -> u64 {
    let imm = ((inst >> 25) << 5) | ((inst >> 7) & 0x1f);
    sext(imm as u64, 12)
}

fn b_imm(inst: u32) -> u64 {
    let imm = ((inst >> 31) & 1) << 12
        | ((inst >> 7) & 1) << 11
        | ((inst >> 25) & 0x3f) << 5
        | ((inst >> 8) & 0xf) << 1;
    sext(imm as u64, 13)
}

fn u_imm(inst: u32) -> u64 {
    (inst & 0xffff_f000) as u64
}

fn j_imm(inst: u32) -> u64 {
    let imm = ((inst >> 31) & 1) << 20
        | ((inst >> 12) & 0xff) << 12
        | ((inst >> 20) & 1) << 11
        | ((inst >> 21) & 0x3ff) << 1;
    sext(imm as u64, 21)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The κ-disk backing (`CC-7` over the substrate store) round-trips byte
    /// ranges faithfully, keeps all-zero sectors sparse, and dedups identical
    /// sectors to one κ — every read/write goes through the owned `KappaStore`.
    #[test]
    fn the_kappa_disk_backing_round_trips_through_the_store() {
        // An image with a non-zero sector 0, a sparse (all-zero) sector 1, and a
        // sector 2 identical to sector 0 (dedup).
        let mut image = vec![0u8; 3 * DISK_SECTOR];
        for (i, b) in image[0..DISK_SECTOR].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let sector0 = image[0..DISK_SECTOR].to_vec();
        image[2 * DISK_SECTOR..3 * DISK_SECTOR].copy_from_slice(&sector0);
        let mut disk = KappaBacking::from_image(&image);

        // Reconstruction is byte-identical, and the disk is content-addressed.
        assert_eq!(disk.to_image(), image, "the κ-disk reconstructs its image");
        assert_eq!(disk.len(), 3 * DISK_SECTOR);
        assert!(
            disk.index[1].is_none(),
            "the all-zero sector is sparse (unstored)"
        );
        assert_eq!(
            disk.index[0], disk.index[2],
            "identical sectors dedup to one κ (Law L1/L2)"
        );

        // A partial-sector write (read-modify-write spanning the sector-1 boundary)
        // is read back faithfully.
        let patch = b"HOLOSPACES-KAPPA-DISK";
        let off = DISK_SECTOR - 4; // straddles sectors 0 and 1
        disk.write_from(off, patch);
        let mut got = vec![0u8; patch.len()];
        disk.read_into(off, &mut got);
        assert_eq!(&got, patch, "a straddling partial-sector write round-trips");
        // Sector 1 is no longer fully zero (it received the tail of the patch).
        assert!(
            disk.index[1].is_some(),
            "the written sector is now content-addressed"
        );
        // Sector 2 (untouched) still shares sector 0's original κ.
        assert_eq!(
            disk.to_image()[2 * DISK_SECTOR..3 * DISK_SECTOR],
            image[0..DISK_SECTOR]
        );
    }

    fn run_to_exit(image: &[u8]) -> u64 {
        let mut emu = Emulator::new(0, 64 * 1024);
        emu.load_flat(image).unwrap();
        match emu.run(100_000) {
            Halt::Exit(code) => code,
            other => panic!("expected exit, got {other:?}"),
        }
    }

    // A couple of hand-encoded sanity programs (the assembled riscv-tests-style
    // battery is the CC-9 integration witness; these keep the core unit-tested).
    #[test]
    fn addi_then_exit() {
        // addi a0, x0, 42 ; addi a7, x0, 93 ; ecall
        let prog = [
            0x13, 0x05, 0xa0, 0x02, // addi a0,x0,42
            0x93, 0x08, 0xd0, 0x05, // addi a7,x0,93
            0x73, 0x00, 0x00, 0x00, // ecall
        ];
        assert_eq!(run_to_exit(&prog), 42);
    }

    #[test]
    fn out_of_budget_is_not_an_exit() {
        // jal x0, 0 — an infinite self-loop; bounded by the step budget.
        let prog = [0x6f, 0x00, 0x00, 0x00];
        let mut emu = Emulator::new(0, 4096);
        emu.load_flat(&prog).unwrap();
        assert_eq!(emu.run(1000), Halt::OutOfBudget);
    }

    /// A virtual access that straddles a 4 KiB page boundary must translate each
    /// page independently — the two pages can map to non-adjacent physical frames
    /// (a demand-paged userland), so the spilled bytes come from the *second*
    /// frame, not from physical bytes contiguous with the first. (Regression: a
    /// glibc userland faulted because a page-crossing `auipc` fetch drew its high
    /// half from the wrong frame; freestanding inits never straddle a mapping.)
    #[test]
    // `l2 + 0 * 8` / `l0 + 1 * 8` keep the page-table index explicit and aligned.
    #[allow(clippy::identity_op, clippy::erasing_op)]
    fn page_straddling_access_translates_each_page() {
        let mut emu = Emulator::new(0, 4 * 1024 * 1024);
        // A minimal Sv39 table: VA 0x1000→PA 0x200000, VA 0x2000→PA 0x100000
        // (deliberately *lower* than the first frame — non-adjacent). Both VAs
        // share VPN[2]=VPN[1]=0, so one L2, one L1, one L0 table suffice.
        let (l2, l1, l0) = (0x10000u64, 0x11000u64, 0x12000u64);
        let (frame_a, frame_b) = (0x200000u64, 0x100000u64);
        let ptr = |ppn: u64| (ppn << 10) | 0b0000_0001; // pointer PTE (V, no RWX)
        let leaf = |ppn: u64| (ppn << 10) | 0b0000_1111; // leaf PTE (V|R|W|X)
        emu.store_phys(l2 + 0 * 8, 8, ptr(l1 >> 12)).unwrap(); // L2[0] → L1
        emu.store_phys(l1 + 0 * 8, 8, ptr(l0 >> 12)).unwrap(); // L1[0] → L0
        emu.store_phys(l0 + 1 * 8, 8, leaf(frame_a >> 12)).unwrap(); // L0[1] → A
        emu.store_phys(l0 + 2 * 8, 8, leaf(frame_b >> 12)).unwrap(); // L0[2] → B
        emu.csrs.insert(0x180, (8u64 << 60) | (l2 >> 12)); // satp: Sv39, root=l2
        emu.priv_level = PRIV_S; // translate (S-mode into non-U leaves is allowed)

        // A 4-byte load at VA 0x1ffe spans the boundary: 2 bytes from frame A's
        // tail, 2 from frame B's head. Seed distinct bytes and read it back.
        emu.store_phys(frame_a + 0xffe, 2, 0xbbaa).unwrap();
        emu.store_phys(frame_b, 2, 0xddcc).unwrap();
        assert_eq!(
            emu.load(0x1ffe, 4, Access::Load).unwrap(),
            0xddcc_bbaa,
            "the high half must come from the second page's frame, not contiguous RAM"
        );

        // An 8-byte load straddling the same boundary (5 bytes A, 3 bytes B).
        emu.store_phys(frame_a + 0xffb, 5, 0x55_4433_2211).unwrap();
        emu.store_phys(frame_b, 3, 0x88_77_66).unwrap();
        assert_eq!(
            emu.load(0x1ffb, 8, Access::Load).unwrap(),
            0x8877_6655_4433_2211,
        );

        // A straddling *store* must land in both frames (and read back identically).
        emu.store(0x1ffe, 4, 0x1122_3344).unwrap();
        assert_eq!(emu.load_phys(frame_a + 0xffe, 2).unwrap(), 0x3344);
        assert_eq!(emu.load_phys(frame_b, 2).unwrap(), 0x1122);
        assert_eq!(emu.load(0x1ffe, 4, Access::Load).unwrap(), 0x1122_3344);

        // A within-page access still translates once and is unaffected.
        emu.store(0x1100, 4, 0xdead_beef).unwrap();
        assert_eq!(emu.load_phys(frame_a + 0x100, 4).unwrap(), 0xdead_beef);
    }

    #[test]
    fn snapshot_is_reproducible() {
        let prog = [
            0x13, 0x05, 0xa0, 0x02, 0x93, 0x08, 0xd0, 0x05, 0x73, 0x00, 0x00, 0x00,
        ];
        let snap = |()| {
            let mut e = Emulator::new(0, 4096);
            e.load_flat(&prog).unwrap();
            e.run(100);
            e.snapshot()
        };
        assert_eq!(
            snap(()),
            snap(()),
            "identical runs ⇒ identical snapshot (L1)"
        );
    }
}
