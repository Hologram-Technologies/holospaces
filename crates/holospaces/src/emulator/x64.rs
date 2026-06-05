//! **The x86-64 (AMD64 / Intel 64) core** — the system emulator's third ISA
//! target (`CC-43`), alongside the [RISC-V](super) `RV64GC` core and the
//! [`aarch64`](crate::emulator::aarch64) `ARMv8-A` core.
//!
//! x86-64 is the ubiquitous registry architecture: most container images publish
//! a `linux/amd64` variant, so this is the core that lets the browser peer boot
//! the largest share of real devcontainers. Like the other cores it is a CPU over
//! the **shared device bus** (`devbus` (`super::devbus`)) — the κ-disk (`virtio`), the
//! console, the userspace NAT — so the disk, networking, and workspace are not
//! re-implemented per ISA (Law L4). Deterministic: identical image + input yield
//! identical console output and final state (Law L1/L5), so a κ snapshot is
//! reproducible across peers.
//!
//! This module is the **long-mode integer core + platform** it boots on: the
//! 64-bit register file and `RFLAGS`, a flat 64-bit address space over guest RAM,
//! the legacy `16550` serial console (port `0x3f8`, the boot console), and the
//! decode/execute of the x86-64 integer instruction set (REX-prefixed,
//! ModRM/SIB-addressed). Conformance is built up instruction-by-instruction (the
//! analogue of the RISC-V core's riscv-tests, `CC-9`); the full real-mode→long-mode
//! Linux boot path composes on this core.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

/// Why the core stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Halt {
    /// The step budget was exhausted (the caller drives more).
    OutOfBudget,
    /// A `hlt` with interrupts disabled — the guest powered off / wedged.
    Halted,
    /// An unsupported or malformed instruction was hit (the core does not yet
    /// implement it) — carries the faulting `rip`.
    Undefined(u64),
}

/// `RFLAGS` bits the integer core maintains.
mod flag {
    pub const CF: u64 = 1 << 0;
    pub const PF: u64 = 1 << 2;
    pub const ZF: u64 = 1 << 6;
    pub const SF: u64 = 1 << 7;
    pub const OF: u64 = 1 << 11;
}

/// The legacy `16550` UART (the PC serial console at I/O port `0x3f8`) — the
/// boot console (`console=ttyS0`). Output is buffered for [`Cpu::console`]; input
/// is the terminal channel ([`Cpu::feed_console`], `CC-11`).
struct Uart {
    output: Vec<u8>,
    input: Vec<u8>,
    in_cursor: usize,
    /// Line Control Register — bit 7 (`DLAB`) selects the divisor latch over the
    /// data/IER registers; the kernel sets the word length here too.
    lcr: u8,
    /// Interrupt Enable Register (the kernel enables/disables UART IRQs).
    ier: u8,
    /// Modem Control Register — bit 4 (`LOOP`) puts the UART in the loopback the
    /// 8250 autodetection drives; the FIFO/scratch probes also run.
    mcr: u8,
    /// Scratch register (`0x3ff`) — the 8250 detection writes then reads it back
    /// to confirm a real UART is present.
    scratch: u8,
    /// FIFO Control Register shadow (the detection reads it back via IIR).
    fcr: u8,
    /// The divisor latch (the configured baud rate; written when `DLAB` is set).
    divisor: u16,
}

/// The shared platform the core drives: the console and the substrate devices —
/// the κ-disk, the shared workspace filesystem, and the userspace network — the
/// *same* [`VirtioBlk`](super::VirtioBlk) / [`Virtio9p`](super::Virtio9p) /
/// [`VirtioNet`](super::VirtioNet) the RISC-V and AArch64 machines boot, serviced
/// by the one shared `devbus` (Law L4: devices are shared, not per-ISA).
struct Sys {
    uart: Uart,
    /// The `virtio-blk` κ-disk rootfs (`CC-7`), when a disk is attached. The
    /// shared `devbus` services its queue against the κ-disk; the full long-mode
    /// boot path (`#12`) advertises the device to the guest.
    virtio: Option<super::VirtioBlk>,
    /// The `virtio-9p` device serving the shared workspace filesystem, when
    /// attached (`CC-15` parity); `None` otherwise.
    virtio9p: Option<super::Virtio9p>,
    /// The `virtio-net` device + the userspace TCP/IP NAT, when networking is
    /// attached (`CC-16` parity); `None` for an offline machine.
    virtionet: Option<super::VirtioNet>,
    /// The host side of the in-process loopback bridge (ADR-020, `CC-33`
    /// parity), when the workbench dials guest listeners; `None` until
    /// [`Cpu::enable_loopback`] attaches it.
    loopback: Option<super::net::LoopbackHandle>,
    /// The interrupt-descriptor table register (`IDTR`): `(base, limit)` of the
    /// 64-bit IDT the kernel installs with `LIDT` — the long-mode boot path's
    /// interrupt delivery (`#12`).
    idtr: (u64, u16),
    /// The global-descriptor table register (`GDTR`): `(base, limit)`. The loader
    /// installs the boot GDT; the kernel reloads its own with `LGDT`.
    gdtr: (u64, u16),
    /// The task register (`TR`) selector + the TSS base (loaded by `LTR`); the
    /// `RSP0` field of the TSS gives the kernel stack on a ring transition.
    tr_base: u64,
    /// The model-specific registers the boot path touches beyond `EFER`
    /// (`FS_BASE`, `GS_BASE`, `KERNEL_GS_BASE`, `STAR`/`LSTAR`/`SFMASK` for
    /// `syscall`, the APIC base, …), keyed by the MSR number.
    msr: BTreeMap<u32, u64>,
    /// The minimal interrupt controller (legacy 8259 PIC pair + the local APIC
    /// the kernel uses) — enough to vector the PIT timer tick and the `virtio`
    /// IRQs so the boot completes.
    pic: Pic,
    /// The 8254 PIT channel-0 timer: a free-running source whose terminal counts
    /// raise IRQ 0 (the boot time base the kernel calibrates against).
    pit: Pit,
    /// The local APIC: the kernel switches the timer/IPI path to it; only the
    /// registers a UP boot drives are modelled (`SVR`, `EOI`, the LVT timer, the
    /// timer initial/current count + divide).
    lapic: Lapic,
    /// The architected timestamp counter (`RDTSC`) — advanced every step so the
    /// kernel's delay loops and the TSC clocksource make progress.
    tsc: u64,
    /// `true` once the machine has halted (a `hlt` with interrupts masked, the
    /// guest power-off) — the run loop then returns [`Halt::Halted`].
    halted: bool,
}

impl Sys {
    fn new() -> Self {
        Sys {
            uart: Uart {
                output: Vec::new(),
                input: Vec::new(),
                in_cursor: 0,
                lcr: 0,
                ier: 0,
                mcr: 0,
                scratch: 0,
                fcr: 0,
                divisor: 1,
            },
            virtio: None,
            virtio9p: None,
            virtionet: None,
            loopback: None,
            idtr: (0, 0),
            gdtr: (0, 0),
            tr_base: 0,
            msr: BTreeMap::new(),
            pic: Pic::new(),
            pit: Pit::new(),
            lapic: Lapic::new(),
            tsc: 0,
            halted: false,
        }
    }
}

/// A segment register's architectural cache: the selector plus the hidden base /
/// limit / attribute the descriptor loaded. In 64-bit mode `CS.L` selects long
/// mode and most bases are ignored (treated as 0) except `FS`/`GS`, whose bases
/// come from the `FS_BASE`/`GS_BASE` MSRs.
#[derive(Clone, Copy, Default)]
struct Seg {
    selector: u16,
    base: u64,
    /// Bit set from the descriptor's `L` (long-mode code) attribute — `CS.L`.
    long: bool,
}

/// The legacy 8259A PIC pair (master + slave). The boot path programs them
/// (ICW1..ICW4, the IRQ masks) before switching to the APIC; modelling the mask +
/// the in-service/request latches lets IRQ 0 (the PIT) and the `virtio` lines
/// vector through the IDT during early boot.
struct Pic {
    /// IRQ mask (1 = masked), master IRQs 0..8 in the low byte, slave 8..16 high.
    mask: u16,
    /// Pending (requested) IRQ lines.
    request: u16,
    /// The vector offset the master/slave were remapped to (ICW2).
    base_master: u8,
    base_slave: u8,
    /// ICW initialization sequence step per chip (0 = idle).
    init_master: u8,
    init_slave: u8,
}

impl Pic {
    fn new() -> Self {
        Pic {
            mask: 0xffff,
            request: 0,
            base_master: 0x08,
            base_slave: 0x70,
            init_master: 0,
            init_slave: 0,
        }
    }
    /// Raise IRQ line `n` (0..16) — latch it as requested.
    fn raise(&mut self, n: u8) {
        if n < 16 {
            self.request |= 1 << n;
        }
    }
    /// The highest-priority unmasked pending IRQ and its vector, or `None`.
    fn pending(&self) -> Option<(u8, u8)> {
        let active = self.request & !self.mask;
        if active == 0 {
            return None;
        }
        let irq = active.trailing_zeros() as u8;
        let vec = if irq < 8 {
            self.base_master.wrapping_add(irq)
        } else {
            self.base_slave.wrapping_add(irq - 8)
        };
        Some((irq, vec))
    }
    fn ack(&mut self, irq: u8) {
        self.request &= !(1 << irq);
    }
}

/// The 8254 PIT channel-0 timer. Only what the kernel's PIT clock-event needs is
/// modelled: the reload value and a step-driven down-counter that raises IRQ 0
/// each time it wraps (the periodic tick the scheduler/clocksource run on).
struct Pit {
    reload: u16,
    counter: u32,
    /// The latched low/high write toggle (the PIT is two byte-wide accesses).
    write_hi: bool,
    enabled: bool,
}

impl Pit {
    fn new() -> Self {
        Pit {
            reload: 0,
            counter: 0,
            write_hi: false,
            enabled: false,
        }
    }
}

/// The local APIC (memory-mapped at the default `0xFEE0_0000`). Only the
/// registers a uniprocessor boot drives: the spurious-vector register (enable),
/// `EOI`, and the LVT timer + its initial/current count and divide — enough to
/// deliver the APIC-timer tick once the kernel migrates off the PIT.
struct Lapic {
    /// Spurious-interrupt vector register; bit 8 is the APIC software-enable.
    svr: u32,
    /// LVT timer entry: the vector (low 8 bits), mask (bit 16), mode (bit 17 =
    /// periodic).
    lvt_timer: u32,
    initial_count: u32,
    current_count: u32,
    divide: u32,
    /// The task-priority register — interrupts at or below `TPR` are deferred.
    tpr: u32,
    /// A latched APIC-timer interrupt vector waiting to be delivered (the LVT
    /// timer fired) — taken by [`Cpu::take_pending_interrupt`].
    pending_vector: Option<u8>,
}

impl Lapic {
    fn new() -> Self {
        Lapic {
            svr: 0xff, // disabled at reset; vector 0xff
            lvt_timer: 1 << 16,
            initial_count: 0,
            current_count: 0,
            divide: 1,
            tpr: 0,
            pending_vector: None,
        }
    }
    fn enabled(&self) -> bool {
        self.svr & (1 << 8) != 0
    }
}

// The `virtio-mmio` transport slots the x86-64 core exposes. They sit in a
// dedicated high MMIO window (above any guest RAM the boot core sizes), so a
// physical access there is unambiguously a device, never RAM — the x86-64
// analogue of QEMU `microvm`'s `virtio-mmio` region. Each slot is the standard
// `virtio-mmio` register block; the devices behind them are the shared
// [`devbus`](super::devbus), identical to the other two cores (Law L4). Only the
// transport (this window + the interrupt path) is per-ISA.
const VIRTIO_BLK_BASE: u64 = 0xD000_0000;
const VIRTIO_BLK_END: u64 = 0xD000_0200;
/// The second `virtio-mmio` slot — the VirtIO **9P** device (the shared
/// workspace filesystem, `CC-15`), serviced by the shared `devbus`.
const VIRTIO_9P_BASE: u64 = 0xD000_0200;
const VIRTIO_9P_END: u64 = 0xD000_0400;
/// The third `virtio-mmio` slot — the VirtIO **network** device (`CC-16`): the
/// userspace TCP/IP NAT, serviced by the shared `devbus`.
const VIRTIO_NET_BASE: u64 = 0xD000_0400;
const VIRTIO_NET_END: u64 = 0xD000_0600;

/// The base of guest RAM (a flat physical address space; the boot core runs with
/// paging off / identity-mapped until the kernel installs its own page tables).
const RAM_BASE: u64 = 0x0;

// The legacy ISA IRQ lines the `virtio-mmio` devices vector through (the values
// the kernel learns from the `virtio_mmio.device=<size>@<base>:<irq>` command
// line). They sit on the slave 8259 so the boot path's IRQ delivery reaches the
// device drivers (`CC-45`).
const VIRTIO_BLK_IRQ: u8 = 11;
const VIRTIO_9P_IRQ: u8 = 10;
const VIRTIO_NET_IRQ: u8 = 12;

/// The x86-64 long-mode integer core.
pub struct Cpu {
    /// The 16 general-purpose registers (`rax`,`rcx`,`rdx`,`rbx`,`rsp`,`rbp`,
    /// `rsi`,`rdi`,`r8`..`r15`).
    r: [u64; 16],
    /// The instruction pointer.
    rip: u64,
    /// The flags register (`RFLAGS`).
    rflags: u64,
    /// Control registers: `cr0` (paging/protection), `cr2` (page-fault address),
    /// `cr3` (the PML4 physical base), `cr4` (PAE et al.).
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    /// `IA32_EFER` — `LME`/`LMA` (long mode enabled/active) live here.
    efer: u64,
    /// The six segment registers (`ES,CS,SS,DS,FS,GS` — indexed by [`SegId`]).
    /// In long mode the bases are 0 except `FS`/`GS`; `CS.long` selects 64-bit.
    seg: [Seg; 6],
    /// The current privilege level (`CPL`, `CS.RPL`): 0 in the kernel, 3 in
    /// userspace — selects the syscall/interrupt stack switch.
    cpl: u8,
    /// The segment override in effect for the instruction being decoded (an `FS`
    /// (`0x64`) / `GS` (`0x65`) prefix), so a memory operand adds that segment's
    /// base — the kernel's per-CPU `%gs:` and the userspace `%fs:` accesses.
    /// Reset at the start of every instruction.
    cur_seg: Option<SegId>,
    /// Whether a `REX` prefix is present on the instruction being decoded. With
    /// no `REX`, a byte register operand `4..=7` selects the **high byte**
    /// (`AH`/`CH`/`DH`/`BH`); with `REX` it selects the low byte of
    /// `RSP`/`RBP`/`RSI`/`RDI` (`SPL`/`BPL`/`SIL`/`DIL`).
    rex_present: bool,
    /// Guest RAM (physical, based at [`RAM_BASE`]).
    ram: Vec<u8>,
    sys: Option<Box<Sys>>,
}

// Register indices.
const RAX: usize = 0;
const RCX: usize = 1;
const RDX: usize = 2;
const RSP: usize = 4;
const RBP: usize = 5;
const RSI: usize = 6;
const RDI: usize = 7;

/// Segment-register indices (the SReg field encoding: ES,CS,SS,DS,FS,GS).
#[derive(Clone, Copy)]
enum SegId {
    Es = 0,
    Cs = 1,
    Ss = 2,
    Ds = 3,
    Fs = 4,
    Gs = 5,
}

/// `RFLAGS.IF` — the interrupt-enable flag (`STI`/`CLI`).
const RFLAGS_IF: u64 = 1 << 9;
/// `RFLAGS.DF` — the direction flag (string-op increment/decrement).
const RFLAGS_DF: u64 = 1 << 10;

// The model-specific registers the long-mode boot path drives.
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
const MSR_APIC_BASE: u32 = 0x1B;

// The local-APIC MMIO window (the architectural default base).
const LAPIC_BASE: u64 = 0xFEE0_0000;
const LAPIC_END: u64 = 0xFEE0_1000;

impl Cpu {
    /// A fresh core with `ram_bytes` of zeroed RAM and `rip`/`rsp` reset.
    #[must_use]
    pub fn new(ram_bytes: usize) -> Self {
        Cpu {
            r: [0; 16],
            rip: RAM_BASE,
            rflags: 0x2, // bit 1 is reserved-1
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            seg: [Seg::default(); 6],
            cpl: 0,
            cur_seg: None,
            rex_present: false,
            ram: vec![0u8; ram_bytes],
            sys: Some(Box::new(Sys::new())),
        }
    }

    /// Whether 4-level paging is active (long mode: `CR0.PG` & `CR4.PAE` &
    /// `EFER.LME`). When off, virtual addresses are physical (the boot core runs
    /// identity-mapped until the kernel installs `CR3`).
    fn paging(&self) -> bool {
        const PG: u64 = 1 << 31;
        const PAE: u64 = 1 << 5;
        const LME: u64 = 1 << 8;
        self.cr0 & PG != 0 && self.cr4 & PAE != 0 && self.efer & LME != 0
    }

    /// Translate a linear address to a physical one through the 4-level page
    /// tables (`PML4 → PDPT → PD → PT`), honouring 2 MiB and 1 GiB large pages.
    /// Returns the linear address unchanged when paging is off. A not-present
    /// entry records `CR2` and falls back to the linear address (the boot core
    /// has no `#PF` handler yet — the continued build adds fault delivery).
    fn translate(&self, vaddr: u64) -> u64 {
        if !self.paging() {
            return vaddr;
        }
        let pml4 = self.cr3 & 0x000f_ffff_ffff_f000;
        let idx = |lvl: u32| ((vaddr >> (12 + 9 * lvl)) & 0x1ff) * 8;
        let ent = |base: u64, i: u64| self.rd_phys(base + i, 8);
        let present = |e: u64| e & 1 != 0;
        let next = |e: u64| e & 0x000f_ffff_ffff_f000;

        let e4 = ent(pml4, idx(3));
        if !present(e4) {
            return vaddr;
        }
        let e3 = ent(next(e4), idx(2));
        if !present(e3) {
            return vaddr;
        }
        if e3 & (1 << 7) != 0 {
            // 1 GiB page
            return (e3 & 0x000f_ffff_c000_0000) | (vaddr & 0x3fff_ffff);
        }
        let e2 = ent(next(e3), idx(1));
        if !present(e2) {
            return vaddr;
        }
        if e2 & (1 << 7) != 0 {
            // 2 MiB page
            return (e2 & 0x000f_ffff_ffe0_0000) | (vaddr & 0x1f_ffff);
        }
        let e1 = ent(next(e2), idx(0));
        if !present(e1) {
            return vaddr;
        }
        (e1 & 0x000f_ffff_ffff_f000) | (vaddr & 0xfff)
    }

    /// Read control register `idx` (0/2/3/4; others read 0).
    fn cr(&self, idx: usize) -> u64 {
        match idx {
            0 => self.cr0,
            2 => self.cr2,
            3 => self.cr3,
            4 => self.cr4,
            _ => 0,
        }
    }

    /// Write control register `idx`.
    fn set_cr(&mut self, idx: usize, val: u64) {
        match idx {
            0 => self.cr0 = val,
            2 => self.cr2 = val,
            3 => self.cr3 = val,
            4 => self.cr4 = val,
            _ => {}
        }
    }

    /// A raw physical read (the page-table walk reads physical memory directly).
    fn rd_phys(&self, addr: u64, size: u8) -> u64 {
        let a = addr as usize;
        let mut v = 0u64;
        for i in 0..size as usize {
            v |= u64::from(*self.ram.get(a + i).unwrap_or(&0)) << (8 * i);
        }
        v
    }

    /// Load a flat code/image blob at guest physical `addr` and set `rip` to it —
    /// the instruction-conformance entry (the analogue of loading a riscv-test).
    pub fn load_at(&mut self, addr: u64, image: &[u8]) {
        let a = addr as usize;
        let n = image.len().min(self.ram.len().saturating_sub(a));
        self.ram[a..a + n].copy_from_slice(&image[..n]);
        self.rip = addr;
        self.r[RSP] = self.ram.len() as u64; // a stack at the top of RAM
    }

    /// The console bytes the guest has written to the serial port.
    #[must_use]
    pub fn console(&self) -> &[u8] {
        self.sys.as_ref().map_or(&[], |s| &s.uart.output)
    }

    /// Feed terminal input to the guest's serial console (readable at `0x3f8`).
    pub fn feed_console(&mut self, bytes: &[u8]) {
        if let Some(sys) = self.sys.as_mut() {
            sys.uart.input.extend_from_slice(bytes);
        }
    }

    /// Read a general-purpose register (for tests / introspection).
    #[must_use]
    pub fn reg(&self, i: usize) -> u64 {
        self.r[i & 15]
    }

    /// `RFLAGS` (for tests / introspection).
    #[must_use]
    pub fn rflags(&self) -> u64 {
        self.rflags
    }

    /// `rip` (for tests / introspection).
    #[must_use]
    pub fn rip(&self) -> u64 {
        self.rip
    }

    // ── Memory ───────────────────────────────────────────────────────────────
    fn rd(&mut self, addr: u64, size: u8) -> u64 {
        let pa = self.translate(addr);
        if (VIRTIO_BLK_BASE..VIRTIO_NET_END).contains(&pa) {
            return self.mmio_read(pa, size as usize);
        }
        if (LAPIC_BASE..LAPIC_END).contains(&pa) {
            return u64::from(self.lapic_read((pa - LAPIC_BASE) as u32));
        }
        let mut v = 0u64;
        for i in 0..u64::from(size) {
            let p = self.translate(addr.wrapping_add(i)) as usize;
            v |= u64::from(*self.ram.get(p).unwrap_or(&0)) << (8 * i);
        }
        v
    }

    fn wr(&mut self, addr: u64, size: u8, val: u64) {
        let pa = self.translate(addr);
        if (VIRTIO_BLK_BASE..VIRTIO_NET_END).contains(&pa) {
            self.mmio_write(pa, size as usize, val);
            return;
        }
        if (LAPIC_BASE..LAPIC_END).contains(&pa) {
            self.lapic_write((pa - LAPIC_BASE) as u32, val as u32);
            return;
        }
        for i in 0..u64::from(size) {
            let p = self.translate(addr.wrapping_add(i)) as usize;
            if let Some(b) = self.ram.get_mut(p) {
                *b = (val >> (8 * i)) as u8;
            }
        }
    }

    fn fetch_u8(&mut self) -> u8 {
        let p = self.translate(self.rip) as usize;
        let b = *self.ram.get(p).unwrap_or(&0);
        self.rip = self.rip.wrapping_add(1);
        b
    }

    fn fetch(&mut self, n: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(self.fetch_u8()) << (8 * i);
        }
        v
    }

    /// Fetch the operand-size immediate (`imm16`/`imm32`, sign-extended to the
    /// operand `size`): 2 bytes under a `0x66` prefix, otherwise 4 bytes (a
    /// 64-bit operand takes a sign-extended `imm32`). The common immediate form of
    /// `MOV`/`ADD`/`TEST`/… r/m, imm.
    fn fetch_imm_z(&mut self, size: u8) -> u64 {
        if size == 2 {
            self.fetch(2)
        } else {
            let i = self.fetch(4);
            if size == 8 {
                i as i32 as i64 as u64
            } else {
                i
            }
        }
    }

    // ── Flags ────────────────────────────────────────────────────────────────
    fn set(&mut self, bit: u64, on: bool) {
        if on {
            self.rflags |= bit;
        } else {
            self.rflags &= !bit;
        }
    }

    fn mask(size: u8) -> u64 {
        if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (8 * size as u32)) - 1
        }
    }

    fn sign(v: u64, size: u8) -> bool {
        (v >> (8 * size as u32 - 1)) & 1 == 1
    }

    /// Set ZF/SF/PF (and clear CF/OF) from a logical result.
    fn flags_logic(&mut self, res: u64, size: u8) {
        let m = Self::mask(size);
        let r = res & m;
        self.set(flag::ZF, r == 0);
        self.set(flag::SF, Self::sign(r, size));
        self.set(flag::PF, (r as u8).count_ones().is_multiple_of(2));
        self.set(flag::CF, false);
        self.set(flag::OF, false);
    }

    /// Set all arithmetic flags for `a (+/-) b = res` (`sub` selects subtraction).
    fn flags_arith(&mut self, a: u64, b: u64, res: u64, size: u8, sub: bool) {
        let m = Self::mask(size);
        let r = res & m;
        self.set(flag::ZF, r == 0);
        self.set(flag::SF, Self::sign(r, size));
        self.set(flag::PF, (r as u8).count_ones().is_multiple_of(2));
        if sub {
            self.set(flag::CF, (a & m) < (b & m));
            let of = (Self::sign(a, size) != Self::sign(b, size))
                && (Self::sign(a, size) != Self::sign(r, size));
            self.set(flag::OF, of);
        } else {
            self.set(flag::CF, r < (a & m));
            let of = (Self::sign(a, size) == Self::sign(b, size))
                && (Self::sign(a, size) != Self::sign(r, size));
            self.set(flag::OF, of);
        }
    }

    /// Run up to `max_steps` instructions; returns why it stopped.
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for i in 0..max_steps {
            // Pump the network periodically so host-side data and connection
            // events reach the guest without it having to transmit first (the
            // `virtio-net` receive path; `CC-16` parity, `CC-46`) — the same
            // shared `devbus` pump the other cores drive from their run loops.
            if i & 0x3ff == 0 && self.sys.as_ref().is_some_and(|s| s.virtionet.is_some()) {
                self.virtio_net_pump();
            }
            // Advance the platform timers (PIT + APIC timer + TSC) and latch a
            // tick when one expires, then deliver any pending interrupt through
            // the IDT — the interrupt path a real Linux boot needs (`#12`).
            self.sys_tick();
            if self.sys.as_ref().is_some_and(|s| s.halted) {
                return Halt::Halted;
            }
            self.take_pending_interrupt();
            match self.step() {
                Ok(()) => {}
                Err(h) => return h,
            }
        }
        Halt::OutOfBudget
    }

    // ── ModRM / effective-address decode ──────────────────────────────────────
    /// Decode the ModRM (and SIB/displacement), returning `(reg, rm)` where `reg`
    /// is the register field (REX.R-extended) and `rm` is the operand location.
    fn modrm(&mut self, rex: u8) -> (usize, Rm) {
        let modrm = self.fetch_u8();
        let md = modrm >> 6;
        let reg = ((modrm >> 3) & 7) as usize | (((rex >> 2) & 1) as usize) << 3; // REX.R
        let rm_field = (modrm & 7) as usize;
        if md == 3 {
            let r = rm_field | (((rex & 1) as usize) << 3); // REX.B
            return (reg, Rm::Reg(r));
        }
        // Memory operand.
        let mut base_disp: i64;
        let mut addr: u64;
        if rm_field == 4 {
            // SIB.
            let sib = self.fetch_u8();
            let scale = sib >> 6;
            let index = ((sib >> 3) & 7) as usize | (((rex >> 1) & 1) as usize) << 3; // REX.X
            let base = (sib & 7) as usize | (((rex & 1) as usize) << 3); // REX.B
            let idx_val = if index == 4 {
                0
            } else {
                self.r[index] << scale
            };
            if (sib & 7) == 5 && md == 0 {
                addr = idx_val; // disp32, no base
                base_disp = i64::from(self.fetch(4) as i32);
            } else {
                addr = self.r[base].wrapping_add(idx_val);
                base_disp = 0;
            }
        } else if rm_field == 5 && md == 0 {
            // RIP-relative: disp32 relative to the address of the *next*
            // instruction. The end-of-instruction rip is only known once any
            // trailing immediate is consumed, so this is resolved lazily (the
            // operand is accessed after the full instruction has decoded).
            let disp = i64::from(self.fetch(4) as i32);
            let seg = self.seg_base();
            return (reg, Rm::RipRel(disp, seg));
        } else {
            addr = self.r[rm_field | (((rex & 1) as usize) << 3)];
            base_disp = 0;
        }
        match md {
            1 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(1) as i8)),
            2 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(4) as i32)),
            _ => {}
        }
        addr = addr
            .wrapping_add(base_disp as u64)
            .wrapping_add(self.seg_base());
        (reg, Rm::Mem(addr))
    }

    /// The base of the segment overriding this instruction's memory operand (the
    /// `FS`/`GS` base from the corresponding MSR), or 0 with no override / in the
    /// flat segments. The kernel's per-CPU `%gs:` and userspace `%fs:` accesses
    /// need this; every other segment is flat (base 0) in long mode.
    fn seg_base(&self) -> u64 {
        match self.cur_seg {
            Some(SegId::Fs) => self.seg[SegId::Fs as usize].base,
            Some(SegId::Gs) => self.seg[SegId::Gs as usize].base,
            _ => 0,
        }
    }

    /// Resolve a decoded r/m operand to its effective address (for memory forms);
    /// `None` for a register operand. Used by `LEA` and the string/atomic ops.
    fn rm_addr(&self, rm: Rm) -> Option<u64> {
        match rm {
            Rm::Reg(_) => None,
            Rm::Mem(a) => Some(a),
            Rm::RipRel(disp, seg) => Some(self.rip.wrapping_add(disp as u64).wrapping_add(seg)),
        }
    }

    /// Read an 8-bit register operand named by the ModRM `reg` field, honouring
    /// the AH/CH/DH/BH high-byte encoding when no `REX` prefix is present.
    fn reg8(&self, reg: usize) -> u64 {
        if !self.rex_present && (4..8).contains(&reg) {
            (self.r[reg - 4] >> 8) & 0xff
        } else {
            self.r[reg] & 0xff
        }
    }

    fn load_rm(&mut self, rm: Rm, size: u8) -> u64 {
        match rm {
            Rm::Reg(i) => {
                if size == 1 && !self.rex_present && (4..8).contains(&i) {
                    // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                    (self.r[i - 4] >> 8) & 0xff
                } else {
                    self.r[i] & Self::mask(size)
                }
            }
            Rm::Mem(a) => self.rd(a, size),
            Rm::RipRel(disp, seg) => {
                let a = self.rip.wrapping_add(disp as u64).wrapping_add(seg);
                self.rd(a, size)
            }
        }
    }

    fn store_rm(&mut self, rm: Rm, size: u8, val: u64) {
        match rm {
            Rm::Reg(i) => {
                if size == 1 && !self.rex_present && (4..8).contains(&i) {
                    // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                    let r = i - 4;
                    self.r[r] = (self.r[r] & !0xff00) | ((val & 0xff) << 8);
                } else if size >= 4 {
                    // 32-bit writes zero the upper 32 bits; 64-bit is full.
                    self.r[i] = val & Self::mask(size);
                } else {
                    let m = Self::mask(size);
                    self.r[i] = (self.r[i] & !m) | (val & m);
                }
            }
            Rm::Mem(a) => self.wr(a, size, val),
            Rm::RipRel(disp, seg) => {
                let a = self.rip.wrapping_add(disp as u64).wrapping_add(seg);
                self.wr(a, size, val);
            }
        }
    }

    fn push(&mut self, val: u64) {
        self.r[RSP] = self.r[RSP].wrapping_sub(8);
        let sp = self.r[RSP];
        self.wr(sp, 8, val);
    }

    fn pop(&mut self) -> u64 {
        let sp = self.r[RSP];
        let v = self.rd(sp, 8);
        self.r[RSP] = sp.wrapping_add(8);
        v
    }

    /// Pop `size` bytes off the stack (the far-return slot width).
    fn pop_sized(&mut self, size: u8) -> u64 {
        let sp = self.r[RSP];
        let v = self.rd(sp, size);
        self.r[RSP] = sp.wrapping_add(u64::from(size));
        v
    }

    /// One of the eight ALU group-1 operations on `(a, b)` → result, setting flags.
    fn alu(&mut self, op: u8, a: u64, b: u64, size: u8) -> u64 {
        let m = Self::mask(size);
        match op {
            0 => {
                let r = a.wrapping_add(b);
                self.flags_arith(a, b, r, size, false);
                r & m
            } // ADD
            1 => {
                let r = a | b;
                self.flags_logic(r, size);
                r & m
            } // OR
            4 => {
                let r = a & b;
                self.flags_logic(r, size);
                r & m
            } // AND
            5 => {
                let r = a.wrapping_sub(b);
                self.flags_arith(a, b, r, size, true);
                r & m
            } // SUB
            6 => {
                let r = a ^ b;
                self.flags_logic(r, size);
                r & m
            } // XOR
            7 => {
                let r = a.wrapping_sub(b);
                self.flags_arith(a, b, r, size, true);
                a & m
            } // CMP (discards result)
            2 => {
                // ADC: a + b + CF, with carry/overflow accounting for the carry-in.
                let cin = u64::from(self.rflags & flag::CF != 0);
                let r = a.wrapping_add(b).wrapping_add(cin);
                let rm = r & m;
                self.set(flag::ZF, rm == 0);
                self.set(flag::SF, Self::sign(rm, size));
                self.set(flag::PF, (rm as u8).count_ones().is_multiple_of(2));
                self.set(flag::CF, rm < (a & m) || (cin == 1 && rm == (a & m)));
                let of = (Self::sign(a, size) == Self::sign(b, size))
                    && (Self::sign(a, size) != Self::sign(rm, size));
                self.set(flag::OF, of);
                rm
            }
            3 => {
                // SBB: a - b - CF.
                let cin = u64::from(self.rflags & flag::CF != 0);
                let r = a.wrapping_sub(b).wrapping_sub(cin);
                let rm = r & m;
                self.set(flag::ZF, rm == 0);
                self.set(flag::SF, Self::sign(rm, size));
                self.set(flag::PF, (rm as u8).count_ones().is_multiple_of(2));
                self.set(
                    flag::CF,
                    (a & m) < (b & m).wrapping_add(cin) || (b & m) == m && cin == 1,
                );
                let of = (Self::sign(a, size) != Self::sign(b, size))
                    && (Self::sign(a, size) != Self::sign(rm, size));
                self.set(flag::OF, of);
                rm
            }
            _ => unreachable!("alu op {op} out of range"),
        }
    }

    /// Evaluate a condition code (the low nibble of a `Jcc`/`SETcc` opcode).
    fn cond(&self, cc: u8) -> bool {
        let f = self.rflags;
        let zf = f & flag::ZF != 0;
        let cf = f & flag::CF != 0;
        let sf = f & flag::SF != 0;
        let of = f & flag::OF != 0;
        let pf = f & flag::PF != 0;
        let base = match cc >> 1 {
            0 => of,               // O
            1 => cf,               // B/C
            2 => zf,               // E/Z
            3 => cf || zf,         // BE
            4 => sf,               // S
            5 => pf,               // P
            6 => sf != of,         // L
            _ => (sf != of) || zf, // LE
        };
        if cc & 1 == 1 {
            !base
        } else {
            base
        }
    }

    /// Port output. The boot path drives the `16550` serial console (`0x3f8`),
    /// the 8259 PIC pair (`0x20`/`0x21`, `0xa0`/`0xa1`), and the 8254 PIT
    /// (`0x40`/`0x43`); other ports are ignored (the kernel probes many).
    fn port_out(&mut self, port: u16, val: u8) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        let dlab = sys.uart.lcr & 0x80 != 0;
        match port {
            0x3f8 if dlab => sys.uart.divisor = (sys.uart.divisor & 0xff00) | u16::from(val),
            0x3f8 if sys.uart.mcr & 0x10 != 0 => {
                // Loopback (the 8250 autodetection): the transmitted byte loops
                // back to the receive register rather than reaching the console.
                sys.uart.input.push(val);
            }
            0x3f8 => sys.uart.output.push(val),
            0x3f9 if dlab => {
                sys.uart.divisor = (sys.uart.divisor & 0x00ff) | (u16::from(val) << 8);
            }
            0x3f9 => sys.uart.ier = val,
            0x3fa => sys.uart.fcr = val, // FCR (write side of IIR/FCR)
            0x3fb => sys.uart.lcr = val,
            0x3fc => sys.uart.mcr = val,
            0x3ff => sys.uart.scratch = val,
            // 8259 master/slave command + data ports (ICW/OCW programming).
            0x20 | 0xa0 => Self::pic_command(&mut sys.pic, port == 0xa0, val),
            0x21 | 0xa1 => Self::pic_data(&mut sys.pic, port == 0xa1, val),
            // 8254 PIT: 0x43 = mode/command, 0x40 = channel-0 counter.
            0x43 => {
                // A channel-0 write (bits 7:6 == 00) latches a new reload.
                if val >> 6 == 0 {
                    sys.pit.write_hi = false;
                }
            }
            0x40 => {
                if sys.pit.write_hi {
                    sys.pit.reload = (sys.pit.reload & 0x00ff) | (u16::from(val) << 8);
                    sys.pit.enabled = true;
                } else {
                    sys.pit.reload = (sys.pit.reload & 0xff00) | u16::from(val);
                }
                sys.pit.write_hi = !sys.pit.write_hi;
            }
            _ => {}
        }
    }

    /// Program an 8259 command port (`0x20`/`0xa0`): ICW1 begins init; an `EOI`
    /// (`0x20`) ends the in-service interrupt.
    fn pic_command(pic: &mut Pic, slave: bool, val: u8) {
        if val & 0x10 != 0 {
            // ICW1 — start the init sequence (ICW2 next).
            if slave {
                pic.init_slave = 1;
            } else {
                pic.init_master = 1;
            }
        }
        // 0x20 = non-specific EOI (handled by the ack in take_pending_interrupt).
    }

    /// Program an 8259 data port (`0x21`/`0xa1`): ICW2 (vector base), ICW3/ICW4
    /// during init, otherwise the IRQ mask (OCW1).
    fn pic_data(pic: &mut Pic, slave: bool, val: u8) {
        let init = if slave {
            &mut pic.init_slave
        } else {
            &mut pic.init_master
        };
        if *init == 1 {
            // ICW2: the vector base (the IRQ → vector remap).
            if slave {
                pic.base_slave = val & 0xf8;
            } else {
                pic.base_master = val & 0xf8;
            }
            *init = 2; // ICW3 next
        } else if *init >= 2 && *init <= 3 {
            *init += 1;
            if *init > 3 {
                *init = 0; // ICW4 consumed → init done
            }
        } else {
            // OCW1: the IRQ mask.
            if slave {
                pic.mask = (pic.mask & 0x00ff) | (u16::from(val) << 8);
            } else {
                pic.mask = (pic.mask & 0xff00) | u16::from(val);
            }
        }
    }

    fn port_in(&mut self, port: u16) -> u8 {
        if let Some(sys) = self.sys.as_mut() {
            let dlab = sys.uart.lcr & 0x80 != 0;
            match port {
                0x3f8 if dlab => return (sys.uart.divisor & 0xff) as u8, // DLL
                0x3f8 if sys.uart.in_cursor < sys.uart.input.len() => {
                    let b = sys.uart.input[sys.uart.in_cursor];
                    sys.uart.in_cursor += 1;
                    return b;
                }
                0x3f8 => return 0, // RBR, no data
                0x3f9 if dlab => return (sys.uart.divisor >> 8) as u8, // DLM
                0x3f9 => return sys.uart.ier,
                0x3fa => {
                    // IIR: no interrupt pending (bit0=1); FIFO bits reflect FCR.
                    return 0x01 | (sys.uart.fcr & 0xc0);
                }
                0x3fb => return sys.uart.lcr,
                0x3fc => return sys.uart.mcr,
                0x3fd => {
                    // Line Status Register: THR-empty (0x20) + transmitter-empty
                    // (0x40) always set; data-ready (0x01) when input is pending.
                    let dr = u8::from(sys.uart.in_cursor < sys.uart.input.len());
                    return 0x60 | dr;
                }
                0x3fe => {
                    // Modem Status Register. In loopback (MCR bit4) the modem
                    // inputs reflect the MCR outputs — the 8250 autodetection test.
                    if sys.uart.mcr & 0x10 != 0 {
                        let m = sys.uart.mcr;
                        // CTS<-RTS(bit1->4), DSR<-DTR(bit0->5), RI<-OUT1(bit2->6),
                        // DCD<-OUT2(bit3->7).
                        return ((m & 0x02) << 3)
                            | ((m & 0x01) << 5)
                            | ((m & 0x04) << 4)
                            | ((m & 0x08) << 4);
                    }
                    return 0xb0; // CTS|DSR|DCD asserted (a present, ready line)
                }
                0x3ff => return sys.uart.scratch,
                0x21 => return (sys.pic.mask & 0xff) as u8,
                0xa1 => return (sys.pic.mask >> 8) as u8,
                _ => {}
            }
        }
        0
    }

    /// A word/dword (`size` 2/4) port read. The PCI config-data port
    /// (`0xcfc`..`0xcff`) returns all-ones — no PCI host bridge is present, so the
    /// kernel's PCI scan finds no devices (it uses `virtio-mmio`, not PCI).
    /// Other ports compose from byte reads.
    fn port_in_wide(&mut self, port: u16, size: u8) -> u64 {
        if (0xcfc..=0xcff).contains(&port) {
            return Self::mask(size); // no device
        }
        let mut v = 0u64;
        for i in 0..u64::from(size) {
            v |= u64::from(self.port_in(port.wrapping_add(i as u16))) << (8 * i);
        }
        v
    }

    /// A word/dword port write. The PCI config-address port (`0xcf8`) and config
    /// data are accepted and discarded (no PCI bridge); other ports compose into
    /// byte writes.
    fn port_out_wide(&mut self, port: u16, size: u8, val: u64) {
        if port == 0xcf8 || (0xcfc..=0xcff).contains(&port) {
            return; // PCI config — no host bridge
        }
        for i in 0..u64::from(size) {
            self.port_out(port.wrapping_add(i as u16), (val >> (8 * i)) as u8);
        }
    }

    // ── local APIC MMIO (the long-mode boot path; #12) ─────────────────────────

    /// Read a local-APIC register at byte offset `off` (the MMIO window at
    /// [`LAPIC_BASE`]). Only the registers a UP boot reads are modelled.
    fn lapic_read(&mut self, off: u32) -> u32 {
        let l = &self.sys().lapic;
        match off {
            0x020 => 0,               // APIC ID (CPU 0)
            0x030 => 0x0005_0014,     // version
            0x080 => l.tpr,           // TPR
            0x0f0 => l.svr,           // spurious vector
            0x320 => l.lvt_timer,     // LVT timer
            0x380 => l.initial_count, // timer initial count
            0x390 => l.current_count, // timer current count
            0x3e0 => l.divide,        // divide config
            _ => 0,
        }
    }

    /// Write a local-APIC register. Drives the spurious-vector (enable), the LVT
    /// timer, the timer count/divide, the TPR, and `EOI` (offset `0xb0`).
    fn lapic_write(&mut self, off: u32, val: u32) {
        let l = &mut self.sys_mut().lapic;
        match off {
            0x080 => l.tpr = val,
            0x0b0 => l.pending_vector = None, // EOI
            0x0f0 => l.svr = val,
            0x320 => l.lvt_timer = val,
            0x380 => {
                l.initial_count = val;
                l.current_count = val;
            }
            0x3e0 => l.divide = val,
            _ => {}
        }
    }

    // ── shared device bus (virtio-mmio transport; CC-46) ─────────────────────

    /// Read a `virtio-mmio` register of one of the shared `devbus` devices at
    /// guest-physical `pa` — the x86-64 transport's side of the *same* devbus the
    /// RISC-V and AArch64 cores drive (Law L4; only the MMIO window differs).
    fn mmio_read(&mut self, pa: u64, _width: usize) -> u64 {
        if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            return super::devbus::blk_mmio_read(self.sys().virtio.as_ref(), pa - VIRTIO_BLK_BASE);
        }
        if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            return super::devbus::p9_mmio_read(self.sys().virtio9p.as_ref(), pa - VIRTIO_9P_BASE);
        }
        if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            return super::devbus::net_mmio_read(
                self.sys().virtionet.as_ref(),
                pa - VIRTIO_NET_BASE,
            );
        }
        0
    }

    /// Write a `virtio-mmio` register of one of the shared `devbus` devices. A
    /// `QueueNotify` services the queue through the shared `devbus` over a
    /// [`GuestRam`](super::devbus::GuestRam) view of x86-64 RAM. Interrupt
    /// delivery is deferred to the long-mode boot path (`#12`, the APIC); the
    /// device-level witness drains the used ring directly (the same way the
    /// AArch64 witness reads the device's reply without taking the SPI).
    fn mmio_write(&mut self, pa: u64, _width: usize, value: u64) {
        if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            self.virtio_blk_write(pa - VIRTIO_BLK_BASE, value as u32);
        } else if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            self.virtio_9p_write(pa - VIRTIO_9P_BASE, value as u32);
        } else if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            self.virtio_net_write(pa - VIRTIO_NET_BASE, value as u32);
        }
    }

    #[doc(hidden)]
    pub fn vv_dbg(&self, vaddr: u64) {
        let pml4 = self.cr3 & 0x000f_ffff_ffff_f000;
        let idx = |lvl: u32| ((vaddr >> (12 + 9 * lvl)) & 0x1ff) * 8;
        let e4 = self.rd_phys(pml4 + idx(3), 8);
        std::eprintln!(
            "cr3={:#x} PML4[{}]={:#x}",
            self.cr3,
            (vaddr >> 39) & 0x1ff,
            e4
        );
        if e4 & 1 == 0 {
            std::eprintln!("  PML4 not present");
            return;
        }
        let e3 = self.rd_phys((e4 & 0x000f_ffff_ffff_f000) + idx(2), 8);
        std::eprintln!("  PDPT[{}]={:#x}", (vaddr >> 30) & 0x1ff, e3);
    }

    /// A guest-RAM view for the shared devbus to walk virtqueues over (x86-64 RAM
    /// is based at [`RAM_BASE`] = 0).
    fn guest_ram(&mut self) -> super::devbus::GuestRam<'_> {
        super::devbus::GuestRam {
            ram: &mut self.ram,
            base: RAM_BASE,
        }
    }

    /// A `virtio-blk` MMIO register write; a `QueueNotify` services the queue
    /// against the κ-disk through the shared `devbus` and raises the device's IRQ
    /// (`VIRTIO_BLK_IRQ`) so the guest's driver completes the request (`CC-45`).
    fn virtio_blk_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio.take() else {
            return;
        };
        let mut raise = false;
        if super::devbus::blk_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = super::devbus::blk_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_BLK_IRQ);
        }
    }

    /// A `virtio-9p` MMIO register write; a `QueueNotify` services the workspace
    /// filesystem queue through the shared `devbus` — the same servicing the
    /// other cores drive (`CC-46`).
    fn virtio_9p_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio9p.take() else {
            return;
        };
        let mut raise = false;
        if super::devbus::p9_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = super::devbus::p9_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio9p = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_9P_IRQ);
        }
    }

    /// A `virtio-net` MMIO register write; a `QueueNotify` services the transmit
    /// queue or pumps the NAT through the shared `devbus` (`CC-46`).
    fn virtio_net_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut raise = false;
        match super::devbus::net_mmio_write(&mut dev, off, value) {
            super::devbus::NetNotify::Transmit => {
                let mut mem = self.guest_ram();
                raise |= super::devbus::net_service_tx(&mut mem, &mut dev);
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::Receive => {
                let mut mem = self.guest_ram();
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::None => {}
        }
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_NET_IRQ);
        }
    }

    /// Pump the NAT and deliver pending receive frames into the guest's receive
    /// queue — called periodically from the run loop so host-side data arrives
    /// without the guest having to transmit first (the same shared `devbus` pump
    /// the other cores drive).
    fn virtio_net_pump(&mut self) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut mem = self.guest_ram();
        let raise = super::devbus::net_pump(&mut mem, &mut dev);
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_NET_IRQ);
        }
    }

    #[inline]
    fn sys(&self) -> &Sys {
        self.sys.as_ref().expect("system mode")
    }
    #[inline]
    fn sys_mut(&mut self) -> &mut Sys {
        self.sys.as_mut().expect("system mode")
    }

    // ── device attach + the shared workspace / network surface (CC-46) ───────

    /// Attach a `virtio-blk` κ-disk rootfs (`CC-7`) — the same κ-disk the other
    /// cores boot, serviced by the shared `devbus`.
    pub fn attach_disk(&mut self, rootfs: Vec<u8>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtio = Some(super::VirtioBlk::new(rootfs));
        }
    }

    /// Attach a shared **workspace filesystem** to the `virtio-9p` device
    /// (`CC-15` parity). `seed` is the files holospaces places on the share; the
    /// guest mounts it (tag `hsworkspace`) and the editor and the running OS read
    /// and write the *same* files over the shared `devbus`.
    pub fn attach_workspace(&mut self, seed: &[(&str, &[u8])]) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        let mut fs = super::ninep::Fs9p::new();
        for (name, data) in seed {
            fs.seed_file(name, data);
        }
        sys.virtio9p = Some(super::Virtio9p::new(fs, super::WORKSPACE_TAG));
    }

    /// Read a file from the shared workspace filesystem — how holospaces observes
    /// the edits the guest made over 9P (`CC-15`).
    #[must_use]
    pub fn workspace_file(&self, name: &str) -> Option<&[u8]> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio9p.as_ref())
            .and_then(|d| d.fs.read_file(name))
    }

    /// Write a file into the shared workspace — the editor saving content the
    /// running OS reads over `virtio-9p` (one content, Law L1; `CC-17`).
    pub fn workspace_write(&mut self, name: &str, data: &[u8]) {
        if let Some(d) = self.sys.as_mut().and_then(|s| s.virtio9p.as_mut()) {
            d.fs.write_file(name, data);
        }
    }

    /// Attach the **VirtIO network device** + the userspace TCP/IP NAT (`CC-16`
    /// parity): the guest drives a real NIC, its frames terminate in the shared
    /// [`net`](super::net) NAT and stream out over `egress`.
    pub fn attach_net(&mut self, egress: Box<dyn super::net::Egress>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtionet = Some(super::VirtioNet::new(
                egress,
                Box::new(super::net::NoIngress),
            ));
        }
    }

    /// Enable the **in-process loopback bridge** (ADR-020, `CC-33` parity) on the
    /// already-attached network device: the workbench (same process) can
    /// `dial`/`send`/`recv`/`close` a connection to a server *inside* the guest.
    /// Returns `false` if no network device is attached.
    pub fn enable_loopback(&mut self) -> bool {
        let Some(net) = self.sys.as_mut().and_then(|s| s.virtionet.as_mut()) else {
            return false;
        };
        let (ingress, handle) = super::net::LoopbackIngress::new();
        net.ingress = Box::new(ingress);
        self.sys_mut().loopback = Some(handle);
        true
    }

    /// Dial an in-process connection to the guest's listening `guest_port` over
    /// the loopback bridge (`CC-33`). Returns the connection id, or `None` if the
    /// loopback ingress is not enabled.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .map(|h| h.dial(guest_port))
    }

    /// Write host bytes toward the guest server on a loopback connection.
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        if let Some(h) = self.sys.as_ref().and_then(|s| s.loopback.as_ref()) {
            h.send(id, data);
        }
    }

    /// Drain the guest server's reply bytes on a loopback connection.
    #[must_use]
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .map(|h| h.recv(id))
            .unwrap_or_default()
    }

    /// Close the host side of a loopback connection.
    pub fn guest_close(&mut self, id: u32) {
        if let Some(h) = self.sys.as_ref().and_then(|s| s.loopback.as_ref()) {
            h.close(id);
        }
    }

    /// Whether a loopback connection is still usable.
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .is_some_and(|h| h.is_open(id))
    }

    // ── V&V device-driver hooks (CC-46) ──────────────────────────────────────

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO store at the
    /// guest-physical address `pa`, exactly as a guest's `virtio` driver would —
    /// the same `mmio_write` the executing CPU routes device stores through. A
    /// conformance witness uses it to drive the shared `devbus` devices over the
    /// x86-64 MMIO transport without booting a full guest kernel.
    #[doc(hidden)]
    pub fn vv_mmio_write(&mut self, pa: u64, width: usize, value: u64) {
        self.mmio_write(pa, width, value);
    }

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO load at `pa` —
    /// the same path the executing CPU routes device loads through.
    #[doc(hidden)]
    pub fn vv_mmio_read(&mut self, pa: u64, width: usize) -> u64 {
        self.mmio_read(pa, width)
    }

    /// **V&V hook** (`CC-46`): write `bytes` into guest RAM at guest-physical
    /// `pa` — a witness lays out the virtqueue and the T-message buffers a guest
    /// driver would build in RAM.
    #[doc(hidden)]
    pub fn vv_ram_write(&mut self, pa: u64, bytes: &[u8]) {
        let o = (pa - RAM_BASE) as usize;
        self.ram[o..o + bytes.len()].copy_from_slice(bytes);
    }

    /// **V&V hook** (`CC-46`): read `len` bytes of guest RAM at guest-physical
    /// `pa` — a witness reads back the R-message the device scattered and the
    /// used-ring the device updated.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_ram_read(&self, pa: u64, len: usize) -> Vec<u8> {
        let o = (pa - RAM_BASE) as usize;
        self.ram[o..o + len].to_vec()
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-9p` MMIO
    /// slot — a witness drives the device at the same address the x86-64 platform
    /// advertises.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_9p_base() -> u64 {
        VIRTIO_9P_BASE
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-net` slot.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_net_base() -> u64 {
        VIRTIO_NET_BASE
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-blk` slot.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_blk_base() -> u64 {
        VIRTIO_BLK_BASE
    }

    /// Decode + execute one instruction.
    #[allow(clippy::too_many_lines)]
    fn step(&mut self) -> Result<(), Halt> {
        let start = self.rip;
        let mut rex = 0u8;
        let mut opsz = false; // 0x66 operand-size override
        let mut rep = RepKind::None; // F3 (REP/REPE) / F2 (REPNE)
        self.cur_seg = None;
        self.rex_present = false;
        loop {
            let p = self.translate(self.rip) as usize;
            let b = *self.ram.get(p).unwrap_or(&0);
            match b {
                0x66 => opsz = true,
                0xf3 => rep = RepKind::Rep,
                0xf2 => rep = RepKind::Repne,
                0x64 => self.cur_seg = Some(SegId::Fs),
                0x65 => self.cur_seg = Some(SegId::Gs),
                0x67 | 0xf0 | 0x2e | 0x36 | 0x3e | 0x26 => {}
                0x40..=0x4f => {
                    rex = b; // REX (last prefix)
                    self.rex_present = true;
                }
                _ => break,
            }
            self.rip = self.rip.wrapping_add(1);
            if (0x40..=0x4f).contains(&b) {
                break; // REX must be the final prefix
            }
        }
        let op = self.fetch_u8();
        let size: u8 = if rex & 8 != 0 {
            8
        } else if opsz {
            2
        } else {
            4
        };
        match op {
            // ── ALU group (add/or/adc/sbb/and/sub/xor/cmp), all six forms ──
            0x00 | 0x08 | 0x10 | 0x18 | 0x20 | 0x28 | 0x30 | 0x38 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.reg8(reg));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x01 | 0x09 | 0x11 | 0x19 | 0x21 | 0x29 | 0x31 | 0x39 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x02 | 0x0a | 0x12 | 0x1a | 0x22 | 0x2a | 0x32 | 0x3a => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.reg8(reg), self.load_rm(rm, 1));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(reg), 1, res);
                }
            }
            0x03 | 0x0b | 0x13 | 0x1b | 0x23 | 0x2b | 0x33 | 0x3b => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.r[reg] & Self::mask(size), self.load_rm(rm, size));
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(reg), size, res);
                }
            }
            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(0), 1, res);
                }
            }
            0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d => {
                let b = self.fetch_imm_z(size);
                let a = self.r[0] & Self::mask(size);
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(0), size, res);
                }
            }
            0x50..=0x57 => {
                let r = (op - 0x50) as usize | (((rex & 1) as usize) << 3);
                let v = self.r[r];
                self.push(v);
            }
            0x58..=0x5f => {
                let r = (op - 0x58) as usize | (((rex & 1) as usize) << 3);
                let v = self.pop();
                self.r[r] = v;
            }
            0x68 => {
                let imm = self.fetch(4) as i32 as i64 as u64;
                self.push(imm);
            }
            0x6a => {
                let imm = self.fetch(1) as i8 as i64 as u64;
                self.push(imm);
            }
            0x70..=0x7f => {
                let rel = self.fetch(1) as i8 as i64;
                if self.cond(op - 0x70) {
                    self.rip = self.rip.wrapping_add(rel as u64);
                }
            }
            0x80 => {
                // The immediate is fetched *before* the operand is touched so a
                // RIP-relative `rm` resolves against the instruction-end `rip`
                // (the same address for the load and the store).
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch(1);
                let a = self.load_rm(rm, 1);
                let res = self.alu((ext & 7) as u8, a, b, 1);
                if ext & 7 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x81 => {
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch_imm_z(size);
                let a = self.load_rm(rm, size);
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x83 => {
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch(1) as i8 as i64 as u64;
                let a = self.load_rm(rm, size);
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x84 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.reg8(reg));
                self.flags_logic(a & b, 1);
            }
            0x85 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                self.flags_logic(a & b, size);
            }
            0x88 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.reg8(reg);
                self.store_rm(rm, 1, v);
            }
            0x89 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.r[reg] & Self::mask(size);
                self.store_rm(rm, size, v);
            }
            0x8a => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 1);
                self.store_rm(Rm::Reg(reg), 1, v);
            }
            0x8b => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, size);
                self.store_rm(Rm::Reg(reg), size, v);
            }
            0x8c => {
                // MOV r/m16, Sreg — store a segment selector.
                let (reg, rm) = self.modrm(rex);
                let sel = u64::from(self.seg.get(reg).map_or(0, |s| s.selector));
                self.store_rm(rm, 2, sel);
            }
            0x8d => {
                let (reg, rm) = self.modrm(rex);
                if let Some(a) = self.rm_addr(rm) {
                    self.store_rm(Rm::Reg(reg), size, a & Self::mask(size));
                }
            }
            0x8e => {
                // MOV Sreg, r/m16 — load a segment selector. In long mode the
                // descriptor base/limit are ignored for DS/ES/SS/CS (flat); only
                // the selector is recorded (FS/GS bases come from their MSRs).
                let (reg, rm) = self.modrm(rex);
                let sel = self.load_rm(rm, 2) as u16;
                if let Some(s) = self.seg.get_mut(reg) {
                    s.selector = sel;
                }
            }
            0x90 => {} // nop
            0xa8 => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                self.flags_logic(a & b, 1);
            }
            0xa9 => {
                let a = self.r[0] & Self::mask(size);
                let b = self.fetch_imm_z(size);
                self.flags_logic(a & b, size);
            }
            0xb0..=0xb7 => {
                let r = (op - 0xb0) as usize | (((rex & 1) as usize) << 3);
                let imm = self.fetch(1);
                self.store_rm(Rm::Reg(r), 1, imm);
            }
            0xb8..=0xbf => {
                let r = (op - 0xb8) as usize | (((rex & 1) as usize) << 3);
                let imm = self.fetch(size); // imm16 / imm32 / imm64 by operand size
                self.store_rm(Rm::Reg(r), size, imm);
            }
            0x63 => {
                // MOVSXD r64, r/m32 — sign-extend a 32-bit operand into 64 bits.
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 4) as u32 as i32 as i64 as u64;
                self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
            }
            0x69 => {
                // IMUL r, r/m, imm (imm16 under 0x66, else imm32 sign-extended).
                let (reg, rm) = self.modrm(rex);
                let imm = if size == 2 {
                    self.fetch(2) as i16 as i64
                } else {
                    self.fetch(4) as i32 as i64
                };
                let a = sign_extend(self.load_rm(rm, size), size);
                let r = a.wrapping_mul(imm) as u64;
                self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
            }
            0x6b => {
                // IMUL r, r/m, imm8.
                let (reg, rm) = self.modrm(rex);
                let imm = self.fetch(1) as i8 as i64;
                let a = sign_extend(self.load_rm(rm, size), size);
                let r = a.wrapping_mul(imm) as u64;
                self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
            }
            0x86 => {
                // XCHG r/m8, r8.
                let (reg, rm) = self.modrm(rex);
                let a = self.load_rm(rm, 1);
                let b = self.reg8(reg);
                self.store_rm(rm, 1, b);
                self.store_rm(Rm::Reg(reg), 1, a);
            }
            0x87 => {
                // XCHG r/m, r.
                let (reg, rm) = self.modrm(rex);
                let a = self.load_rm(rm, size);
                let b = self.r[reg] & Self::mask(size);
                self.store_rm(rm, size, b);
                self.store_rm(Rm::Reg(reg), size, a);
            }
            0x8f => {
                // POP r/m64.
                let (_e, rm) = self.modrm(rex);
                let v = self.pop();
                self.store_rm(rm, 8, v);
            }
            0x98 => {
                // CBW/CWDE/CDQE — sign-extend AL/AX/EAX to the operand size.
                let v = match size {
                    8 => self.r[RAX] as i32 as i64 as u64,
                    2 => (self.r[RAX] as i8 as i16 as u16) as u64,
                    _ => self.r[RAX] as i16 as i32 as u32 as u64,
                };
                self.store_rm(Rm::Reg(RAX), size, v);
            }
            0x99 => {
                // CWD/CDQ/CQO — sign-extend rax into rdx:rax.
                let neg = Self::sign(self.r[RAX], size);
                let ext = if neg { Self::mask(size) } else { 0 };
                self.store_rm(Rm::Reg(RDX), size, ext);
            }
            0x9c => {
                // PUSHFQ.
                self.push(self.rflags);
            }
            0x9d => {
                // POPFQ.
                self.rflags = (self.pop() & 0x0024_4dd5) | 0x2;
            }
            0xa4 => self.string_op(StringOp::Movs, 1, rep),
            0xa5 => self.string_op(StringOp::Movs, size, rep),
            0xa6 => self.string_op(StringOp::Cmps, 1, rep),
            0xa7 => self.string_op(StringOp::Cmps, size, rep),
            0xaa => self.string_op(StringOp::Stos, 1, rep),
            0xab => self.string_op(StringOp::Stos, size, rep),
            0xac => self.string_op(StringOp::Lods, 1, rep),
            0xad => self.string_op(StringOp::Lods, size, rep),
            0xae => self.string_op(StringOp::Scas, 1, rep),
            0xaf => self.string_op(StringOp::Scas, size, rep),
            0xc0 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = self.fetch(1) as u8;
                self.shift_rotate((ext & 7) as u8, rm, 1, cnt);
            }
            0xc1 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = self.fetch(1) as u8;
                self.shift_rotate((ext & 7) as u8, rm, size, cnt);
            }
            0xd0 => {
                let (ext, rm) = self.modrm(rex);
                self.shift_rotate((ext & 7) as u8, rm, 1, 1);
            }
            0xd1 => {
                let (ext, rm) = self.modrm(rex);
                self.shift_rotate((ext & 7) as u8, rm, size, 1);
            }
            0xd2 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = (self.r[RCX] & 0xff) as u8;
                self.shift_rotate((ext & 7) as u8, rm, 1, cnt);
            }
            0xd3 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = (self.r[RCX] & 0xff) as u8;
                self.shift_rotate((ext & 7) as u8, rm, size, cnt);
            }
            0xc2 => {
                // RET imm16 — pop the return address, then pop `imm16` arg bytes.
                let n = self.fetch(2);
                let v = self.pop();
                self.rip = v;
                self.r[RSP] = self.r[RSP].wrapping_add(n);
            }
            0xc9 => {
                // LEAVE: rsp = rbp; rbp = pop().
                self.r[RSP] = self.r[RBP];
                let v = self.pop();
                self.r[RBP] = v;
            }
            0xcf => {
                // IRETQ — return from an interrupt: pop RIP, CS, RFLAGS, RSP, SS.
                let rip = self.pop();
                let cs = self.pop();
                let rflags = self.pop();
                let rsp = self.pop();
                let ss = self.pop();
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.cpl = (cs & 3) as u8;
                self.rflags = (rflags & 0x0024_4dd5) | 0x2;
                self.r[RSP] = rsp;
                self.seg[SegId::Ss as usize].selector = ss as u16;
            }
            0xe3 => {
                // JRCXZ rel8.
                let rel = self.fetch(1) as i8 as i64;
                if self.r[RCX] == 0 {
                    self.rip = self.rip.wrapping_add(rel as u64);
                }
            }
            0xf5 => self.set(flag::CF, self.rflags & flag::CF == 0), // CMC
            0xf6 | 0xf7 => {
                let osz = if op == 0xf6 { 1 } else { size };
                self.group3(rex, osz, start)?;
            }
            0xf8 => self.set(flag::CF, false), // CLC
            0xf9 => self.set(flag::CF, true),  // STC
            0xfa => self.rflags &= !RFLAGS_IF, // CLI
            0xfb => self.rflags |= RFLAGS_IF,  // STI
            0xfc => self.rflags &= !RFLAGS_DF, // CLD
            0xfd => self.rflags |= RFLAGS_DF,  // STD
            0xc3 => {
                let v = self.pop();
                self.rip = v;
            }
            0xcb => {
                // LRET (far return): pop RIP then CS. The operand size (REX.W /
                // default) sets the slot width; the selector is the low 16 bits.
                let osz = if rex & 8 != 0 { 8 } else { 4 };
                let rip = self.pop_sized(osz);
                let cs = self.pop_sized(osz);
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.cpl = (cs & 3) as u8;
            }
            0xca => {
                // LRET imm16 (far return, freeing imm16 stack bytes).
                let n = self.fetch(2);
                let osz = if rex & 8 != 0 { 8 } else { 4 };
                let rip = self.pop_sized(osz);
                let cs = self.pop_sized(osz);
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.r[RSP] = self.r[RSP].wrapping_add(n);
            }
            0xc6 => {
                let (_e, rm) = self.modrm(rex);
                let imm = self.fetch(1);
                self.store_rm(rm, 1, imm);
            }
            0xc7 => {
                let (_e, rm) = self.modrm(rex);
                let v = self.fetch_imm_z(size);
                self.store_rm(rm, size, v);
            }
            0xe4 => {
                let port = self.fetch(1) as u16;
                let v = self.port_in(port);
                self.store_rm(Rm::Reg(0), 1, u64::from(v));
            }
            0xe6 => {
                let port = self.fetch(1) as u16;
                let v = (self.r[0] & 0xff) as u8;
                self.port_out(port, v);
            }
            0xe5 => {
                // IN eAX, imm8 — a word/dword port read (operand size).
                let port = self.fetch(1) as u16;
                let v = self.port_in_wide(port, size);
                self.store_rm(Rm::Reg(0), size, v);
            }
            0xe7 => {
                // OUT imm8, eAX — a word/dword port write.
                let port = self.fetch(1) as u16;
                let v = self.r[0] & Self::mask(size);
                self.port_out_wide(port, size, v);
            }
            0xed => {
                // IN eAX, dx — a word/dword port read.
                let port = (self.r[RDX] & 0xffff) as u16;
                let v = self.port_in_wide(port, size);
                self.store_rm(Rm::Reg(0), size, v);
            }
            0xef => {
                // OUT dx, eAX — a word/dword port write.
                let port = (self.r[RDX] & 0xffff) as u16;
                let v = self.r[0] & Self::mask(size);
                self.port_out_wide(port, size, v);
            }
            0xe8 => {
                let rel = self.fetch(4) as i32 as i64;
                let ret = self.rip;
                self.push(ret);
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xe9 => {
                let rel = self.fetch(4) as i32 as i64;
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xeb => {
                let rel = self.fetch(1) as i8 as i64;
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xec => {
                let port = (self.r[2] & 0xffff) as u16;
                let v = self.port_in(port);
                self.store_rm(Rm::Reg(0), 1, u64::from(v));
            }
            0xee => {
                let port = (self.r[2] & 0xffff) as u16;
                let v = (self.r[0] & 0xff) as u8;
                self.port_out(port, v);
            }
            0xf4 => {
                // HLT: with interrupts enabled the CPU idles until the next
                // interrupt — fast-forward the platform timers so a tick is
                // delivered rather than busy-spinning a step at a time. With
                // interrupts masked it is a true stop (the guest power-off).
                if self.rflags & RFLAGS_IF == 0 {
                    if let Some(sys) = self.sys.as_mut() {
                        sys.halted = true;
                    }
                    return Err(Halt::Halted);
                }
                self.rip = start; // re-execute HLT until an interrupt arrives
                self.idle_until_interrupt();
            }
            0xfe => {
                let (ext, rm) = self.modrm(rex);
                let a = self.load_rm(rm, 1);
                let sub = ext & 7 == 1;
                let r = if sub {
                    a.wrapping_sub(1)
                } else {
                    a.wrapping_add(1)
                };
                let cf = self.rflags & flag::CF;
                self.flags_arith(a, 1, r, 1, sub);
                self.rflags = (self.rflags & !flag::CF) | cf; // inc/dec preserve CF
                self.store_rm(rm, 1, r);
            }
            0xff => {
                let (ext, rm) = self.modrm(rex);
                match ext & 7 {
                    0 | 1 => {
                        let a = self.load_rm(rm, size);
                        let sub = ext & 7 == 1;
                        let r = if sub {
                            a.wrapping_sub(1)
                        } else {
                            a.wrapping_add(1)
                        };
                        let cf = self.rflags & flag::CF;
                        self.flags_arith(a, 1, r, size, sub);
                        self.rflags = (self.rflags & !flag::CF) | cf;
                        self.store_rm(rm, size, r);
                    }
                    2 => {
                        let t = self.load_rm(rm, 8);
                        let ret = self.rip;
                        self.push(ret);
                        self.rip = t;
                    }
                    4 => self.rip = self.load_rm(rm, 8),
                    6 => {
                        let v = self.load_rm(rm, 8);
                        self.push(v);
                    }
                    _ => return Err(Halt::Undefined(start)),
                }
            }
            0x0f => {
                let op2 = self.fetch_u8();
                match op2 {
                    0x80..=0x8f => {
                        let rel = self.fetch(4) as i32 as i64;
                        if self.cond(op2 - 0x80) {
                            self.rip = self.rip.wrapping_add(rel as u64);
                        }
                    }
                    0x90..=0x9f => {
                        let cc = op2 - 0x90;
                        let (_e, rm) = self.modrm(rex);
                        let v = u64::from(self.cond(cc));
                        self.store_rm(rm, 1, v);
                    }
                    0xb6 => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1);
                        self.store_rm(Rm::Reg(reg), size, v);
                    }
                    0xb7 => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 2);
                        self.store_rm(Rm::Reg(reg), size, v);
                    }
                    0xbe => {
                        // MOVSX r, r/m8.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1) as i8 as i64 as u64;
                        self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
                    }
                    0xbf => {
                        // MOVSX r, r/m16.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 2) as i16 as i64 as u64;
                        self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
                    }
                    0xaf => {
                        let (reg, rm) = self.modrm(rex);
                        let (a, b) = (self.r[reg] & Self::mask(size), self.load_rm(rm, size));
                        self.store_rm(Rm::Reg(reg), size, a.wrapping_mul(b));
                    }
                    0x20 => {
                        // MOV r64, CRn (mod is ignored — rm is a register).
                        let (cr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.r[g] = self.cr(cr_idx);
                        }
                    }
                    0x22 => {
                        // MOV CRn, r64 — install paging (CR3), enable PG/PAE, etc.
                        let (cr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            let v = self.r[g];
                            self.set_cr(cr_idx, v);
                        }
                    }
                    0x30 => {
                        // WRMSR: MSR[ecx] = edx:eax.
                        let ecx = (self.r[RCX] & 0xffff_ffff) as u32;
                        let val = ((self.r[RDX] & 0xffff_ffff) << 32) | (self.r[RAX] & 0xffff_ffff);
                        self.wrmsr(ecx, val);
                    }
                    0x32 => {
                        // RDMSR: edx:eax = MSR[ecx].
                        let ecx = (self.r[RCX] & 0xffff_ffff) as u32;
                        let val = self.rdmsr(ecx);
                        self.r[RAX] = val & 0xffff_ffff;
                        self.r[RDX] = val >> 32;
                    }
                    0x05 => self.syscall_enter(),
                    0x07 => self.sysret(),
                    0x01 => self.group7(rex, start)?,
                    0x00 => {
                        // Group 6: LLDT(/2), LTR(/3), VERR/VERW — load the task
                        // register from the GDT descriptor `LTR` selects.
                        let (ext, rm) = self.modrm(rex);
                        let sel = self.load_rm(rm, 2);
                        if ext & 7 == 3 {
                            self.load_tr(sel as u16);
                        }
                    }
                    0xa2 => self.cpuid(),
                    0x31 => {
                        let tsc = self.sys().tsc;
                        self.r[RAX] = tsc & 0xffff_ffff;
                        self.r[RDX] = tsc >> 32;
                    }
                    0x09 | 0x0e | 0x18..=0x1f | 0x77 | 0xae => {
                        // WBINVD/FEMMS/NOP(prefetch/hint)/EMMS/fences+fxsave — no
                        // architectural effect the integer boot path observes.
                        // 0x18..0x1f take a ModRM; 0xae usually does too.
                        if matches!(op2, 0x18..=0x1f | 0xae) {
                            let _ = self.modrm(rex);
                        }
                    }
                    0x0b => return Err(Halt::Undefined(start)), // UD2
                    0x40..=0x4f => {
                        // CMOVcc r, r/m.
                        let cc = op2 - 0x40;
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size);
                        if self.cond(cc) {
                            self.store_rm(Rm::Reg(reg), size, v);
                        }
                    }
                    0xa3 | 0xab | 0xb3 | 0xbb => {
                        // BT / BTS / BTR / BTC r/m, r.
                        let (reg, rm) = self.modrm(rex);
                        let bit = self.r[reg] & Self::mask(size);
                        self.bit_test(rm, size, bit, (op2 >> 3) & 3);
                    }
                    0xba => {
                        // Group 8: BT/BTS/BTR/BTC r/m, imm8.
                        let (ext, rm) = self.modrm(rex);
                        let bit = u64::from(self.fetch(1) as u8);
                        self.bit_test(rm, size, bit, ((ext & 7).wrapping_sub(4) & 3) as u8);
                    }
                    0xbc => {
                        // BSF r, r/m — index of the lowest set bit; ZF if zero.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        self.set(flag::ZF, v == 0);
                        if v != 0 {
                            self.store_rm(Rm::Reg(reg), size, u64::from(v.trailing_zeros()));
                        }
                    }
                    0xbd => {
                        // BSR r, r/m — index of the highest set bit.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        self.set(flag::ZF, v == 0);
                        if v != 0 {
                            let idx = 63 - v.leading_zeros();
                            self.store_rm(Rm::Reg(reg), size, u64::from(idx));
                        }
                    }
                    0xb0 => self.cmpxchg(rex, 1),
                    0xb1 => self.cmpxchg(rex, size),
                    0xc0 => self.xadd(rex, 1),
                    0xc1 => self.xadd(rex, size),
                    0xa4 | 0xac => {
                        // SHLD/SHRD r/m, r, imm8.
                        let (reg, rm) = self.modrm(rex);
                        let cnt = self.fetch(1) as u8;
                        self.shld_shrd(rm, reg, size, cnt, op2 == 0xac);
                    }
                    0xa5 | 0xad => {
                        // SHLD/SHRD r/m, r, CL.
                        let (reg, rm) = self.modrm(rex);
                        let cnt = (self.r[RCX] & 0xff) as u8;
                        self.shld_shrd(rm, reg, size, cnt, op2 == 0xad);
                    }
                    0xc7 => {
                        // Group 9: CMPXCHG8B/16B r/m.
                        let (ext, rm) = self.modrm(rex);
                        if ext & 7 == 1 {
                            self.cmpxchg16b(rm, rex & 8 != 0);
                        }
                    }
                    _ => return Err(Halt::Undefined(start)),
                }
            }
            _ => return Err(Halt::Undefined(start)),
        }
        Ok(())
    }
}

// The guest-physical layout the 64-bit boot protocol uses (a low region below
// the kernel's 16 MiB load address, mirroring the firecracker/kvmtool loaders).
const ZERO_PAGE: u64 = 0x7000; // struct boot_params (the "zero page")
const CMDLINE_ADDR: u64 = 0x20000; // the kernel command line
const BOOT_GDT: u64 = 0x500; // the boot GDT (3 flat descriptors)
const BOOT_PML4: u64 = 0x9000; // the loader's identity page tables
const BOOT_PDPT: u64 = 0xa000;

impl Cpu {
    /// Boot a real, unmodified x86-64 Linux `vmlinux` ELF to userspace via the
    /// 64-bit Linux boot protocol (`#12`, the x86-64 realization of `CC-36`).
    /// `kernel` is the *uncompressed* ELF; the loader lays out its `PT_LOAD`
    /// segments at their physical addresses, builds `boot_params` (the zero page,
    /// with an e820 map + the command line), a flat GDT, and identity page tables,
    /// enables long mode, and enters `startup_64` with `rsi` = the zero page —
    /// no real-mode setup, no in-guest decompressor. Drive it with [`Cpu::run`].
    #[must_use]
    pub fn boot_linux(ram_bytes: usize, kernel: &[u8], cmdline: &str) -> Self {
        Self::boot_linux_inner(ram_bytes, kernel, cmdline, None)
    }

    /// Boot like [`Cpu::boot_linux`], additionally attaching a **`virtio-blk`
    /// root filesystem** (`CC-45`): `rootfs` is the assembled image taken as
    /// κ-addressed content into the κ-disk (`CC-7`); the guest mounts it over
    /// `/dev/vda`. The shared `devbus` services the device — the same κ-disk the
    /// other cores boot. Use a `cmdline` with `root=/dev/vda`.
    #[must_use]
    pub fn boot_linux_disk(
        ram_bytes: usize,
        kernel: &[u8],
        rootfs: Vec<u8>,
        cmdline: &str,
    ) -> Self {
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::new(rootfs)),
        )
    }

    fn boot_linux_inner(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        disk: Option<super::VirtioBlk>,
    ) -> Self {
        let mut cpu = Cpu::new(ram_bytes);
        let entry = cpu.load_elf64(kernel);

        // The command line + the zero page (boot_params).
        let cl = cmdline.as_bytes();
        cpu.ram[CMDLINE_ADDR as usize..CMDLINE_ADDR as usize + cl.len()].copy_from_slice(cl);
        cpu.ram[CMDLINE_ADDR as usize + cl.len()] = 0;
        cpu.build_boot_params(cmdline, ram_bytes as u64, disk.is_some());

        // The boot GDT: a null descriptor, a 64-bit code segment (__BOOT_CS,
        // selector 0x10), and a flat data segment (__BOOT_DS, selector 0x18) —
        // the segments the 64-bit boot protocol requires.
        cpu.build_boot_gdt();

        // Identity page tables: a single PML4 → PDPT mapping the low 512 GiB via
        // 1 GiB pages, so the entered kernel (and the zero page / command line /
        // its own code at 16 MiB) is reachable until startup_64 installs its own.
        cpu.build_boot_paging();

        // Enter long mode: PAE + PG + EFER.LME/LMA, CS = the long-mode code
        // segment, the data segments flat, rsi = the zero page, rip = startup_64.
        cpu.cr3 = BOOT_PML4;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = (1 << 8) | (1 << 10); // LME | LMA
        cpu.cr0 = (1 << 0) | (1 << 31); // PE | PG
        cpu.seg[SegId::Cs as usize] = Seg {
            selector: 0x10,
            base: 0,
            long: true,
        };
        for s in [SegId::Ds, SegId::Es, SegId::Ss, SegId::Fs, SegId::Gs] {
            cpu.seg[s as usize] = Seg {
                selector: 0x18,
                base: 0,
                long: false,
            };
        }
        cpu.sys_mut().gdtr = (BOOT_GDT, 0x17);
        cpu.r[RSI] = ZERO_PAGE;
        cpu.rip = entry;
        cpu.rflags = 0x2; // interrupts off, as on entry
        cpu.cpl = 0;
        cpu.sys_mut().virtio = disk;
        cpu
    }

    /// Load an ELF64 executable's `PT_LOAD` segments at their physical addresses
    /// (zeroing `bss` up to `p_memsz`) and return the entry point. The kernel's
    /// `vmlinux` is `ET_EXEC`; its segments load at the physical column of each
    /// program header.
    fn load_elf64(&mut self, elf: &[u8]) -> u64 {
        let rd16 = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]);
        let rd32 = |o: usize| u32::from_le_bytes([elf[o], elf[o + 1], elf[o + 2], elf[o + 3]]);
        let rd64 = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&elf[o..o + 8]);
            u64::from_le_bytes(b)
        };
        assert_eq!(&elf[0..4], b"\x7fELF", "vmlinux is an ELF");
        let entry = rd64(24);
        let phoff = rd64(32) as usize;
        let phentsize = rd16(54) as usize;
        let phnum = rd16(56) as usize;
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if rd32(ph) != 1 {
                continue; // PT_LOAD only
            }
            let offset = rd64(ph + 8) as usize;
            let paddr = rd64(ph + 24);
            let filesz = rd64(ph + 32) as usize;
            let memsz = rd64(ph + 40) as usize;
            let dst = paddr as usize;
            let n = filesz.min(elf.len().saturating_sub(offset));
            if dst + memsz <= self.ram.len() {
                self.ram[dst..dst + n].copy_from_slice(&elf[offset..offset + n]);
                for b in &mut self.ram[dst + n..dst + memsz] {
                    *b = 0;
                }
            }
        }
        entry
    }

    /// Build the boot GDT in low RAM: null, `__BOOT_CS` (0x10, 64-bit code), and
    /// `__BOOT_DS` (0x18, flat data) — the descriptors the entered kernel runs on
    /// until it reloads its own with `LGDT`.
    fn build_boot_gdt(&mut self) {
        let put = |ram: &mut [u8], sel: u64, desc: u64| {
            ram[BOOT_GDT as usize + sel as usize..BOOT_GDT as usize + sel as usize + 8]
                .copy_from_slice(&desc.to_le_bytes());
        };
        put(&mut self.ram, 0x00, 0);
        // 64-bit code: present, ring 0, code, executable/readable, L=1 (long).
        put(&mut self.ram, 0x10, 0x00af_9b00_0000_ffff);
        // Flat data: present, ring 0, data, read/write.
        put(&mut self.ram, 0x18, 0x00cf_9300_0000_ffff);
    }

    /// Build identity page tables: one PML4 entry → one PDPT whose first 512
    /// entries are 1 GiB pages mapping the low 512 GiB linearly. Enough that the
    /// entered kernel, the zero page, the command line, and the kernel's own load
    /// region are all reachable until `startup_64` installs its own tables.
    fn build_boot_paging(&mut self) {
        // PML4[0] → PDPT.
        self.ram[BOOT_PML4 as usize..BOOT_PML4 as usize + 8]
            .copy_from_slice(&(BOOT_PDPT | 0x3).to_le_bytes());
        for i in 0..512u64 {
            // 1 GiB pages: present | rw | page-size (bit 7).
            let e = (i << 30) | 0x83;
            let off = BOOT_PDPT as usize + i as usize * 8;
            self.ram[off..off + 8].copy_from_slice(&e.to_le_bytes());
        }
    }

    /// Build `struct boot_params` (the zero page): the setup-header fields the
    /// 64-bit protocol requires (`type_of_loader`, `loadflags`, `cmd_line_ptr`,
    /// the heap/ramdisk fields) and an **e820 memory map** so the kernel discovers
    /// RAM, the MMIO/APIC holes, and reserved low memory.
    fn build_boot_params(&mut self, _cmdline: &str, ram_bytes: u64, _has_disk: bool) {
        let zp = ZERO_PAGE as usize;
        let put32 = |ram: &mut [u8], off: usize, v: u32| {
            ram[zp + off..zp + off + 4].copy_from_slice(&v.to_le_bytes());
        };
        let put16 = |ram: &mut [u8], off: usize, v: u16| {
            ram[zp + off..zp + off + 2].copy_from_slice(&v.to_le_bytes());
        };
        let put8 = |ram: &mut [u8], off: usize, v: u8| ram[zp + off] = v;
        // struct setup_header (within boot_params at 0x1f1). The boot-protocol
        // signature + version the kernel checks before honouring cmd_line_ptr.
        put8(&mut self.ram, 0x1f1, 0); // setup_sects (unused on the 64-bit entry)
                                       // "HdrS" header magic at 0x202; boot-protocol version 2.15 at 0x206.
        self.ram[zp + 0x202..zp + 0x206].copy_from_slice(b"HdrS");
        put16(&mut self.ram, 0x206, 0x020f);
        // type_of_loader (0x210): 0xFF = undefined/unknown loader (accepted).
        put8(&mut self.ram, 0x210, 0xff);
        // loadflags (0x211): LOADED_HIGH (bit0) | CAN_USE_HEAP (bit7).
        put8(&mut self.ram, 0x211, 0x81);
        // cmd_line_ptr (0x228) — the 32-bit physical address of the command line.
        put32(&mut self.ram, 0x228, CMDLINE_ADDR as u32);
        // cmdline_size (0x238) — the maximum command-line length.
        put32(&mut self.ram, 0x238, 0x800);
        // ramdisk_image / ramdisk_size (0x218 / 0x21c): none (initramfs embedded).
        put32(&mut self.ram, 0x218, 0);
        put32(&mut self.ram, 0x21c, 0);

        // The e820 map (boot_params.e820_entries at 0x1e8, e820_table at 0x2d0;
        // each entry is 20 bytes: u64 addr, u64 size, u32 type). type 1 = RAM,
        // type 2 = reserved.
        let mut entries: Vec<(u64, u64, u32)> = Vec::new();
        // Low RAM below the legacy 640 KiB / 1 MiB region: usable up to 0x9fc00,
        // reserved EBDA/BIOS to 1 MiB.
        entries.push((0x0000_0000, 0x0009_fc00, 1));
        entries.push((0x0009_fc00, 0x0000_0400, 2));
        entries.push((0x000f_0000, 0x0001_0000, 2));
        // Main RAM from 1 MiB up to the MMIO window (kept below 0xD000_0000).
        let main_end = ram_bytes.min(VIRTIO_BLK_BASE);
        entries.push((0x0010_0000, main_end - 0x0010_0000, 1));
        // The virtio-mmio + APIC windows are reserved (not RAM).
        entries.push((VIRTIO_BLK_BASE, LAPIC_END - VIRTIO_BLK_BASE, 2));
        // Any RAM above 4 GiB (if sized that large) is usable.
        if ram_bytes > 0x1_0000_0000 {
            entries.push((0x1_0000_0000, ram_bytes - 0x1_0000_0000, 1));
        }
        let n = entries.len().min(128) as u8;
        self.ram[zp + 0x1e8] = n;
        for (i, (addr, size, ty)) in entries.iter().take(128).enumerate() {
            let off = zp + 0x2d0 + i * 20;
            self.ram[off..off + 8].copy_from_slice(&addr.to_le_bytes());
            self.ram[off + 8..off + 16].copy_from_slice(&size.to_le_bytes());
            self.ram[off + 16..off + 20].copy_from_slice(&ty.to_le_bytes());
        }
    }

    // ── platform timers + interrupt delivery (the long-mode boot path; #12) ────

    /// Advance the architected timestamp counter and the platform timers one
    /// step; latch a timer interrupt (PIT IRQ 0 or the APIC-timer LVT vector)
    /// when one expires. The kernel calibrates against these and runs its
    /// periodic tick on them.
    fn sys_tick(&mut self) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        sys.tsc = sys.tsc.wrapping_add(64);
        // The 8254 PIT channel-0 down-counter.
        if sys.pit.enabled && sys.pit.reload != 0 {
            if sys.pit.counter == 0 {
                sys.pit.counter = u32::from(sys.pit.reload);
            }
            sys.pit.counter = sys.pit.counter.saturating_sub(1);
            if sys.pit.counter == 0 {
                sys.pit.counter = u32::from(sys.pit.reload);
                sys.pic.raise(0);
            }
        }
        // The local-APIC timer.
        if sys.lapic.enabled()
            && sys.lapic.initial_count != 0
            && sys.lapic.lvt_timer & (1 << 16) == 0
        {
            if sys.lapic.current_count == 0 {
                sys.lapic.current_count = sys.lapic.initial_count;
            }
            sys.lapic.current_count = sys.lapic.current_count.saturating_sub(1);
            if sys.lapic.current_count == 0 {
                if sys.lapic.lvt_timer & (1 << 17) != 0 {
                    sys.lapic.current_count = sys.lapic.initial_count; // periodic
                }
                // The APIC-timer vector is delivered directly through the IDT (it
                // is a local-APIC source, not a PIC line).
                sys.lapic.pending_vector = Some((sys.lapic.lvt_timer & 0xff) as u8);
            }
        }
    }

    /// Fast-forward the timers until a deliverable interrupt is latched — called
    /// from `HLT` with interrupts enabled so the idle CPU resumes on the next
    /// tick rather than re-executing `HLT` for millions of steps.
    fn idle_until_interrupt(&mut self) {
        for _ in 0..5_000_000u64 {
            self.sys_tick();
            if self.has_pending_interrupt() {
                return;
            }
        }
    }

    /// Whether an interrupt is pending and deliverable (`RFLAGS.IF` set).
    fn has_pending_interrupt(&self) -> bool {
        if self.rflags & RFLAGS_IF == 0 {
            return false;
        }
        self.sys
            .as_ref()
            .is_some_and(|s| s.lapic.pending_vector.is_some() || s.pic.pending().is_some())
    }

    /// Deliver a pending external interrupt through the IDT if `RFLAGS.IF` is set
    /// and the kernel has installed an IDT. Returns `true` if an interrupt was
    /// taken (the caller then re-enters the run loop at the handler).
    fn take_pending_interrupt(&mut self) -> bool {
        if self.rflags & RFLAGS_IF == 0 {
            return false;
        }
        let (vector, irq) = {
            let Some(sys) = self.sys.as_ref() else {
                return false;
            };
            if sys.idtr.1 == 0 {
                return false;
            }
            if let Some(v) = sys.lapic.pending_vector {
                (v, None)
            } else if let Some((irq, vec)) = sys.pic.pending() {
                (vec, Some(irq))
            } else {
                return false;
            }
        };
        // Acknowledge the source.
        if let Some(sys) = self.sys.as_mut() {
            if let Some(irq) = irq {
                sys.pic.ack(irq);
            } else {
                sys.lapic.pending_vector = None;
            }
        }
        self.deliver_interrupt(vector, 0, false);
        true
    }

    /// Vector through the 64-bit IDT: push the interrupt frame (`SS,RSP,RFLAGS,
    /// CS,RIP`, and the error code for the faults that carry one), switch to the
    /// gate's stack on a ring change, clear `IF`, and jump to the handler.
    fn deliver_interrupt(&mut self, vector: u8, error: u64, has_error: bool) {
        let (idt_base, _) = self.sys().idtr;
        let desc = idt_base + u64::from(vector) * 16;
        let lo = self.rd_phys(desc, 8);
        let hi = self.rd_phys(desc + 8, 8);
        let off = (lo & 0xffff) | ((lo >> 32) & 0xffff_0000) | ((hi & 0xffff_ffff) << 32);
        let ist = (lo >> 32) & 0x7;
        let old_cs = self.seg[SegId::Cs as usize].selector;
        let old_ss = self.seg[SegId::Ss as usize].selector;
        let old_rsp = self.r[RSP];
        let old_cpl = self.cpl;

        // A ring transition (CPL 3 → 0) loads RSP0 from the TSS; an IST index
        // selects an IST stack. For the kernel's own interrupts (CPL 0) the stack
        // is unchanged unless an IST is set.
        if old_cpl != 0 || ist != 0 {
            let rsp0 = self.tss_stack(ist);
            if rsp0 != 0 {
                self.r[RSP] = rsp0;
            }
            self.cpl = 0;
            self.seg[SegId::Cs as usize].selector = 0x10;
            self.seg[SegId::Cs as usize].long = true;
            self.seg[SegId::Ss as usize].selector = 0x18;
        }
        let rflags = self.rflags;
        self.push(u64::from(old_ss));
        self.push(old_rsp);
        self.push(rflags);
        self.push(u64::from(old_cs));
        self.push(self.rip);
        if has_error {
            self.push(error);
        }
        self.rflags &= !RFLAGS_IF; // an interrupt gate clears IF
        self.rip = off;
    }

    /// The kernel stack for a ring transition: `RSP0` (IST 0) or the indexed IST
    /// entry from the TSS the kernel loaded with `LTR`.
    fn tss_stack(&self, ist: u64) -> u64 {
        let base = self.sys().tr_base;
        if base == 0 {
            return 0;
        }
        if ist == 0 {
            self.rd_phys(base + 4, 8) // RSP0 at TSS offset 4
        } else {
            self.rd_phys(base + 0x24 + (ist - 1) * 8, 8) // IST1.. at offset 0x24
        }
    }

    /// Deliver a CPU exception (a fault/abort) through the IDT — `#GP`, `#PF`
    /// (with `CR2` already set), `#UD`, etc. Mirrors the AArch64 core taking an
    /// EL1 exception instead of halting the flat core.
    fn raise_exception(&mut self, vector: u8, error: u64, has_error: bool) {
        if self.sys.as_ref().is_some_and(|s| s.idtr.1 != 0) {
            self.deliver_interrupt(vector, error, has_error);
        }
    }

    // ── the instruction tail the boot path hits (#12) ──────────────────────────

    /// Execute a string instruction (`MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS`) of
    /// element size `osz`, honouring a `REP`/`REPE`/`REPNE` prefix and the
    /// direction flag. `RSI`/`RDI` advance by ±`osz`; `RCX` counts the repeats.
    fn string_op(&mut self, kind: StringOp, osz: u8, rep: RepKind) {
        let step: i64 = if self.rflags & RFLAGS_DF != 0 {
            -i64::from(osz)
        } else {
            i64::from(osz)
        };
        let mut count = if rep == RepKind::None { 1 } else { self.r[RCX] };
        while count != 0 {
            match kind {
                StringOp::Movs => {
                    let v = self.rd(self.r[RSI], osz);
                    self.wr(self.r[RDI], osz, v);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Stos => {
                    self.wr(self.r[RDI], osz, self.r[RAX] & Self::mask(osz));
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Lods => {
                    let v = self.rd(self.r[RSI], osz);
                    self.store_rm(Rm::Reg(RAX), osz, v);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                }
                StringOp::Scas => {
                    let a = self.r[RAX] & Self::mask(osz);
                    let b = self.rd(self.r[RDI], osz);
                    let r = a.wrapping_sub(b);
                    self.flags_arith(a, b, r, osz, true);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Cmps => {
                    let a = self.rd(self.r[RSI], osz);
                    let b = self.rd(self.r[RDI], osz);
                    let r = a.wrapping_sub(b);
                    self.flags_arith(a, b, r, osz, true);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
            }
            if rep != RepKind::None {
                count -= 1;
                self.r[RCX] = count;
                // REPE/REPNE on SCAS/CMPS also terminate on the ZF condition.
                if matches!(kind, StringOp::Scas | StringOp::Cmps) {
                    let zf = self.rflags & flag::ZF != 0;
                    if (rep == RepKind::Rep && !zf) || (rep == RepKind::Repne && zf) {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Execute a shift/rotate (`ROL`/`ROR`/`RCL`/`RCR`/`SHL`/`SHR`/`SAR`) on a
    /// decoded operand, masking the count to 6 bits (5 for sub-64-bit) and setting
    /// the carry/zero/sign flags as the architecture defines.
    fn shift_rotate(&mut self, ext: u8, rm: Rm, size: u8, cnt: u8) {
        let bits = u32::from(size) * 8;
        let cnt = u32::from(cnt) & if size == 8 { 63 } else { 31 };
        if cnt == 0 {
            return;
        }
        let m = Self::mask(size);
        let a = self.load_rm(rm, size) & m;
        let (res, cf) = match ext {
            0 => {
                // ROL
                let r = ((a << cnt) | (a >> (bits - cnt))) & m;
                (r, r & 1)
            }
            1 => {
                // ROR
                let r = ((a >> cnt) | (a << (bits - cnt))) & m;
                (r, (r >> (bits - 1)) & 1)
            }
            4 | 6 => {
                // SHL / SAL
                let r = (a << cnt) & m;
                (r, (a >> (bits - cnt)) & 1)
            }
            5 => {
                // SHR
                let r = a >> cnt;
                (r, (a >> (cnt - 1)) & 1)
            }
            7 => {
                // SAR (arithmetic)
                let sa = (a as i64) << (64 - bits) >> (64 - bits); // sign-extend
                let r = ((sa >> cnt) as u64) & m;
                (r, (a >> (cnt - 1)) & 1)
            }
            2 | 3 => {
                // RCL / RCR through CF — uncommon on the boot path; approximate
                // with the corresponding rotate (the kernel does not depend on the
                // carry-through for correctness here).
                let r = if ext == 2 {
                    ((a << cnt) | (a >> (bits - cnt))) & m
                } else {
                    ((a >> cnt) | (a << (bits - cnt))) & m
                };
                (r, r & 1)
            }
            _ => (a, 0),
        };
        self.store_rm(rm, size, res);
        self.set(flag::CF, cf != 0);
        if ext >= 4 {
            // The logical/arithmetic shifts update SF/ZF/PF.
            self.set(flag::ZF, res == 0);
            self.set(flag::SF, Self::sign(res, size));
            self.set(flag::PF, (res as u8).count_ones().is_multiple_of(2));
        }
    }

    /// `SHLD`/`SHRD` — a double-precision shift of `dst` with bits shifted in from
    /// `src`.
    fn shld_shrd(&mut self, rm: Rm, reg: usize, size: u8, cnt: u8, right: bool) {
        let bits = u32::from(size) * 8;
        let cnt = u32::from(cnt) & if size == 8 { 63 } else { 31 };
        if cnt == 0 {
            return;
        }
        let m = Self::mask(size);
        let dst = self.load_rm(rm, size) & m;
        let src = self.r[reg] & m;
        let res = if right {
            ((dst >> cnt) | (src << (bits - cnt))) & m
        } else {
            ((dst << cnt) | (src >> (bits - cnt))) & m
        };
        self.store_rm(rm, size, res);
        self.set(flag::ZF, res == 0);
        self.set(flag::SF, Self::sign(res, size));
        self.set(flag::PF, (res as u8).count_ones().is_multiple_of(2));
    }

    /// Group 3 (`0xf6`/`0xf7`): `TEST`/`NOT`/`NEG`/`MUL`/`IMUL`/`DIV`/`IDIV`.
    fn group3(&mut self, rex: u8, size: u8, start: u64) -> Result<(), Halt> {
        let (ext, rm) = self.modrm(rex);
        let m = Self::mask(size);
        match ext & 7 {
            0 | 1 => {
                // TEST r/m, imm. The immediate is fetched first so a RIP-relative
                // `rm` resolves against the instruction-end `rip`.
                let imm = if size == 1 {
                    self.fetch(1)
                } else {
                    self.fetch_imm_z(size)
                };
                let a = self.load_rm(rm, size);
                self.flags_logic(a & imm, size);
            }
            2 => {
                // NOT (no flags).
                let a = self.load_rm(rm, size);
                self.store_rm(rm, size, !a & m);
            }
            3 => {
                // NEG.
                let a = self.load_rm(rm, size) & m;
                let r = a.wrapping_neg() & m;
                self.flags_arith(0, a, r, size, true);
                self.set(flag::CF, a != 0);
                self.store_rm(rm, size, r);
            }
            4 => {
                // MUL (unsigned): rdx:rax = rax * r/m.
                let a = self.r[RAX] & m;
                let b = self.load_rm(rm, size) & m;
                let prod = u128::from(a) * u128::from(b);
                self.store_mul_result(prod, size);
            }
            5 => {
                // IMUL (signed): rdx:rax = rax * r/m.
                let a = sign_extend(self.r[RAX] & m, size) as i128;
                let b = sign_extend(self.load_rm(rm, size) & m, size) as i128;
                let prod = (a * b) as u128;
                self.store_mul_result(prod, size);
            }
            6 => {
                // DIV (unsigned).
                let divisor = self.load_rm(rm, size) & m;
                if divisor == 0 {
                    self.raise_exception(0, 0, false); // #DE
                    return Ok(());
                }
                let dividend = self.dividend(size);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                self.store_div_result(q as u64, r as u64, size);
            }
            7 => {
                // IDIV (signed).
                let divisor = sign_extend(self.load_rm(rm, size) & m, size);
                if divisor == 0 {
                    self.raise_exception(0, 0, false);
                    return Ok(());
                }
                let dividend = self.dividend_signed(size);
                let q = dividend / i128::from(divisor);
                let r = dividend % i128::from(divisor);
                self.store_div_result(q as u64, r as u64, size);
            }
            _ => return Err(Halt::Undefined(start)),
        }
        Ok(())
    }

    fn store_mul_result(&mut self, prod: u128, size: u8) {
        let m = Self::mask(size);
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (prod as u64 & 0xffff);
        } else {
            self.store_rm(Rm::Reg(RAX), size, prod as u64 & m);
            self.store_rm(
                Rm::Reg(RDX),
                size,
                (prod >> (u32::from(size) * 8)) as u64 & m,
            );
        }
        let hi = prod >> (u32::from(size) * 8);
        let overflow = hi != 0;
        self.set(flag::CF, overflow);
        self.set(flag::OF, overflow);
    }

    fn dividend(&self, size: u8) -> u128 {
        let m = Self::mask(size);
        if size == 1 {
            u128::from(self.r[RAX] & 0xffff)
        } else {
            (u128::from(self.r[RDX] & m) << (u32::from(size) * 8)) | u128::from(self.r[RAX] & m)
        }
    }

    fn dividend_signed(&self, size: u8) -> i128 {
        if size == 1 {
            i128::from(self.r[RAX] as i16)
        } else {
            let m = Self::mask(size);
            let hi = sign_extend(self.r[RDX] & m, size) as i128;
            let lo = (self.r[RAX] & m) as i128;
            (hi << (u32::from(size) * 8)) | lo
        }
    }

    fn store_div_result(&mut self, q: u64, r: u64, size: u8) {
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (q & 0xff) | ((r & 0xff) << 8);
        } else {
            self.store_rm(Rm::Reg(RAX), size, q & Self::mask(size));
            self.store_rm(Rm::Reg(RDX), size, r & Self::mask(size));
        }
    }

    /// `BT`/`BTS`/`BTR`/`BTC` — test (and optionally set/reset/complement) bit
    /// `bit` of the operand; the prior bit value goes to `CF`. `op` selects the
    /// variant (0=BT, 1=BTS, 2=BTR, 3=BTC).
    fn bit_test(&mut self, rm: Rm, size: u8, bit: u64, op: u8) {
        let nbits = u64::from(size) * 8;
        // For a memory operand the bit index addresses beyond the operand; for a
        // register it wraps to the operand width.
        match rm {
            Rm::Reg(_) => {
                let b = bit % nbits;
                let v = self.load_rm(rm, size);
                self.set(flag::CF, (v >> b) & 1 != 0);
                let nv = match op {
                    1 => v | (1 << b),
                    2 => v & !(1 << b),
                    3 => v ^ (1 << b),
                    _ => v,
                };
                if op != 0 {
                    self.store_rm(rm, size, nv);
                }
            }
            _ => {
                let base = self.rm_addr(rm).unwrap();
                let byte = base.wrapping_add(bit / 8);
                let b = bit % 8;
                let v = self.rd(byte, 1);
                self.set(flag::CF, (v >> b) & 1 != 0);
                let nv = match op {
                    1 => v | (1 << b),
                    2 => v & !(1 << b),
                    3 => v ^ (1 << b),
                    _ => v,
                };
                if op != 0 {
                    self.wr(byte, 1, nv);
                }
            }
        }
    }

    /// `CMPXCHG`: compare `RAX` with the destination; if equal, store the source,
    /// else load the destination into `RAX`. Sets `ZF` on success.
    fn cmpxchg(&mut self, rex: u8, size: u8) {
        let (reg, rm) = self.modrm(rex);
        let dst = self.load_rm(rm, size) & Self::mask(size);
        let acc = self.r[RAX] & Self::mask(size);
        let r = acc.wrapping_sub(dst);
        self.flags_arith(acc, dst, r, size, true);
        if acc == dst {
            let src = self.r[reg] & Self::mask(size);
            self.store_rm(rm, size, src);
        } else {
            self.store_rm(Rm::Reg(RAX), size, dst);
        }
    }

    /// `CMPXCHG8B`/`CMPXCHG16B`: compare `EDX:EAX`/`RDX:RAX` with the 8/16-byte
    /// destination; swap in `ECX:EBX`/`RCX:RBX` on a match. Sets `ZF`.
    fn cmpxchg16b(&mut self, rm: Rm, wide: bool) {
        let Some(addr) = self.rm_addr(rm) else {
            return;
        };
        if wide {
            let lo = self.rd(addr, 8);
            let hi = self.rd(addr + 8, 8);
            if lo == self.r[RAX] && hi == self.r[RDX] {
                self.wr(addr, 8, self.r[1]); // RBX
                self.wr(addr + 8, 8, self.r[RCX]);
                self.set(flag::ZF, true);
            } else {
                self.r[RAX] = lo;
                self.r[RDX] = hi;
                self.set(flag::ZF, false);
            }
        } else {
            let lo = self.rd(addr, 4);
            let hi = self.rd(addr + 4, 4);
            if lo == (self.r[RAX] & 0xffff_ffff) && hi == (self.r[RDX] & 0xffff_ffff) {
                self.wr(addr, 4, self.r[1] & 0xffff_ffff);
                self.wr(addr + 4, 4, self.r[RCX] & 0xffff_ffff);
                self.set(flag::ZF, true);
            } else {
                self.r[RAX] = lo;
                self.r[RDX] = hi;
                self.set(flag::ZF, false);
            }
        }
    }

    /// `XADD`: exchange-and-add — the destination and source swap, then their sum
    /// is stored to the destination, setting the arithmetic flags.
    fn xadd(&mut self, rex: u8, size: u8) {
        let (reg, rm) = self.modrm(rex);
        let dst = self.load_rm(rm, size) & Self::mask(size);
        let src = self.r[reg] & Self::mask(size);
        let sum = dst.wrapping_add(src);
        self.flags_arith(dst, src, sum, size, false);
        self.store_rm(Rm::Reg(reg), size, dst);
        self.store_rm(rm, size, sum & Self::mask(size));
    }

    /// `CPUID` — report the minimal feature set the kernel's early boot checks
    /// require: long mode, the standard feature bits, and a vendor string.
    fn cpuid(&mut self) {
        let leaf = (self.r[RAX] & 0xffff_ffff) as u32;
        let (a, b, c, d): (u32, u32, u32, u32) = match leaf {
            0 => (0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65), // max leaf, "AuthenticAMD"
            1 => {
                // Family/model in EAX; EDX features: FPU,TSC,MSR,PAE,CX8,APIC,
                // SEP,MTRR,PGE,CMOV,PAT,PSE36,CLFSH,MMX,FXSR,SSE,SSE2.
                (0x0000_0600, 0, 0x0000_0001, 0x078b_fbff)
            }
            7 => (0, 0, 0, 0),
            0x8000_0000 => (0x8000_0008, 0x6874_7541, 0x444d_4163, 0x6974_6e65),
            0x8000_0001 => {
                // EDX: SYSCALL (bit 11), NX (bit 20), Long Mode (bit 29), …
                (0, 0, 0x0000_0001, 0x2010_0800)
            }
            0x8000_0002..=0x8000_0004 => (0x6f6c_6f48, 0x6170_7373, 0x2d65_7363, 0x3436_7858),
            0x8000_0008 => (0x0000_3028, 0, 0, 0), // 48-bit virt, 40-bit phys
            _ => (0, 0, 0, 0),
        };
        self.r[RAX] = u64::from(a);
        self.r[1] = u64::from(b); // RBX
        self.r[RCX] = u64::from(c);
        self.r[RDX] = u64::from(d);
    }

    /// Read a model-specific register the boot path uses.
    fn rdmsr(&self, ecx: u32) -> u64 {
        match ecx {
            MSR_EFER => self.efer,
            MSR_FS_BASE => self.seg[SegId::Fs as usize].base,
            MSR_GS_BASE => self.seg[SegId::Gs as usize].base,
            MSR_APIC_BASE => LAPIC_BASE | 0x900, // enabled, BSP
            _ => self.sys().msr.get(&ecx).copied().unwrap_or(0),
        }
    }

    /// Write a model-specific register, applying the architectural side effects
    /// (`EFER` long-mode bits, the `FS`/`GS` bases).
    fn wrmsr(&mut self, ecx: u32, val: u64) {
        match ecx {
            MSR_EFER => self.efer = val,
            MSR_FS_BASE => self.seg[SegId::Fs as usize].base = val,
            MSR_GS_BASE => self.seg[SegId::Gs as usize].base = val,
            _ => {
                self.sys_mut().msr.insert(ecx, val);
            }
        }
    }

    /// `SWAPGS` — exchange `GS.base` with the `KERNEL_GS_BASE` MSR (the kernel's
    /// per-CPU base swap on a ring transition).
    fn swapgs(&mut self) {
        let cur = self.seg[SegId::Gs as usize].base;
        let kern = self
            .sys()
            .msr
            .get(&MSR_KERNEL_GS_BASE)
            .copied()
            .unwrap_or(0);
        self.seg[SegId::Gs as usize].base = kern;
        self.sys_mut().msr.insert(MSR_KERNEL_GS_BASE, cur);
    }

    /// Group 7 (`0F 01`): `LGDT`/`LIDT`/`SGDT`/`SIDT`/`LMSW`/`SWAPGS`/`RDTSCP`.
    fn group7(&mut self, rex: u8, _start: u64) -> Result<(), Halt> {
        // Peek the ModRM to distinguish the memory forms (LGDT/LIDT, mod != 3)
        // from the register-encoded ones (SWAPGS = 0F 01 F8, etc.).
        let modrm = *self
            .ram
            .get(self.translate(self.rip) as usize)
            .unwrap_or(&0);
        let md = modrm >> 6;
        let ext = (modrm >> 3) & 7;
        if md == 3 {
            // Register forms keyed by the full ModRM byte.
            self.rip = self.rip.wrapping_add(1);
            match modrm {
                0xf8 => self.swapgs(),
                0xf9 => {
                    // RDTSCP — like RDTSC plus the CPU id in ECX.
                    let tsc = self.sys().tsc;
                    self.r[RAX] = tsc & 0xffff_ffff;
                    self.r[RDX] = tsc >> 32;
                    self.r[RCX] = 0;
                }
                _ => {} // MONITOR/MWRC/etc. — no boot-path effect
            }
            return Ok(());
        }
        let (_e, rm) = self.modrm(rex);
        let addr = self.rm_addr(rm).unwrap_or(0);
        match ext {
            2 => {
                // LGDT: limit (u16) then base (u64).
                let limit = self.rd(addr, 2) as u16;
                let base = self.rd(addr + 2, 8);
                self.sys_mut().gdtr = (base, limit);
            }
            3 => {
                // LIDT.
                let limit = self.rd(addr, 2) as u16;
                let base = self.rd(addr + 2, 8);
                self.sys_mut().idtr = (base, limit);
            }
            0 => {
                // SGDT.
                let (base, limit) = self.sys().gdtr;
                self.wr(addr, 2, u64::from(limit));
                self.wr(addr + 2, 8, base);
            }
            1 => {
                // SIDT.
                let (base, limit) = self.sys().idtr;
                self.wr(addr, 2, u64::from(limit));
                self.wr(addr + 2, 8, base);
            }
            _ => {}
        }
        Ok(())
    }

    /// `LTR` — load the task register: read the 64-bit TSS descriptor `sel`
    /// selects from the GDT and latch the TSS base (for the ring-transition stack
    /// switch).
    fn load_tr(&mut self, sel: u16) {
        let (gdt, _) = self.sys().gdtr;
        let desc = gdt + u64::from(sel & 0xfff8);
        let lo = self.rd_phys(desc, 8);
        let hi = self.rd_phys(desc + 8, 8);
        let base =
            ((lo >> 16) & 0xff_ffff) | (((lo >> 56) & 0xff) << 24) | ((hi & 0xffff_ffff) << 32);
        self.sys_mut().tr_base = base;
    }

    /// `SYSCALL` — the fast system-call entry: save `RIP→RCX`, `RFLAGS→R11`, mask
    /// `RFLAGS` with `SFMASK`, load `RIP` from `LSTAR`, and switch `CS`/`SS` to the
    /// kernel selectors from `STAR`. The userspace PID-1 enters the kernel here.
    fn syscall_enter(&mut self) {
        let star = self.sys().msr.get(&MSR_STAR).copied().unwrap_or(0);
        let lstar = self.sys().msr.get(&MSR_LSTAR).copied().unwrap_or(0);
        let sfmask = self.sys().msr.get(&MSR_SFMASK).copied().unwrap_or(0);
        self.r[RCX] = self.rip;
        self.r[11] = self.rflags;
        self.rflags &= !sfmask;
        self.rflags &= !RFLAGS_IF;
        let kcs = ((star >> 32) & 0xffff) as u16;
        self.seg[SegId::Cs as usize].selector = kcs;
        self.seg[SegId::Cs as usize].long = true;
        self.seg[SegId::Ss as usize].selector = kcs.wrapping_add(8);
        self.cpl = 0;
        self.rip = lstar;
    }

    /// `SYSRET` — the fast system-call return: restore `RIP←RCX`, `RFLAGS←R11`,
    /// and the userspace `CS`/`SS` from `STAR`, returning to CPL 3.
    fn sysret(&mut self) {
        let star = self.sys().msr.get(&MSR_STAR).copied().unwrap_or(0);
        self.rip = self.r[RCX];
        self.rflags = (self.r[11] & 0x0024_4dd5) | 0x2;
        let ucs = (((star >> 48) & 0xffff) as u16).wrapping_add(16) | 3;
        self.seg[SegId::Cs as usize].selector = ucs;
        self.seg[SegId::Cs as usize].long = true;
        self.seg[SegId::Ss as usize].selector =
            (((star >> 48) & 0xffff) as u16).wrapping_add(8) | 3;
        self.cpl = 3;
    }
}

/// Sign-extend the low `size` bytes of `v` to a full `i64`.
fn sign_extend(v: u64, size: u8) -> i64 {
    match size {
        1 => v as u8 as i8 as i64,
        2 => v as u16 as i16 as i64,
        4 => v as u32 as i32 as i64,
        _ => v as i64,
    }
}

/// A decoded ModRM r/m operand: a register index, a resolved effective address,
/// or a RIP-relative operand (`disp`, `segment base`) resolved lazily once the
/// full instruction — including any trailing immediate — has decoded.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u64),
    RipRel(i64, u64),
}

/// A string-instruction repeat prefix (`REP`/`REPE` = `F3`, `REPNE` = `F2`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RepKind {
    None,
    Rep,
    Repne,
}

/// The string instruction family (`MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS`).
#[derive(Clone, Copy)]
enum StringOp {
    Movs,
    Stos,
    Lods,
    Scas,
    Cmps,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load + run a flat x86-64 code blob; assert it stopped cleanly.
    fn run(code: &[u8]) -> Cpu {
        let mut cpu = Cpu::new(64 * 1024);
        cpu.load_at(0, code);
        let h = cpu.run(100_000);
        assert!(
            matches!(h, Halt::Halted | Halt::OutOfBudget),
            "unexpected halt: {h:?}"
        );
        cpu
    }

    #[test]
    fn adds_two_registers() {
        // mov rax,5 ; mov rbx,7 ; add rax,rbx ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x05, 0, 0, 0, // mov rax, 5
            0x48, 0xc7, 0xc3, 0x07, 0, 0, 0, // mov rbx, 7
            0x48, 0x01, 0xd8, // add rax, rbx
            0xf4, // hlt
        ];
        assert_eq!(run(&code).reg(0), 12);
    }

    #[test]
    fn countdown_loop_sets_the_zero_flag() {
        // mov rcx,3 ; (dec rcx ; jnz -5) ; hlt  — a real branch loop
        let code = [
            0x48, 0xc7, 0xc1, 0x03, 0, 0, 0, // mov rcx, 3
            0x48, 0xff, 0xc9, // dec rcx
            0x75, 0xfb, // jnz -5 (back to dec)
            0xf4, // hlt
        ];
        let cpu = run(&code);
        assert_eq!(cpu.reg(1), 0, "the loop ran to zero");
        assert!(cpu.rflags() & flag::ZF != 0, "ZF set at zero");
    }

    #[test]
    fn writes_to_the_serial_console() {
        // mov dx,0x3f8 ; mov al,'h' ; out dx,al ; mov al,'i' ; out dx,al ; hlt
        let code = [
            0x66, 0xba, 0xf8, 0x03, // mov dx, 0x3f8 (the 16550 THR port)
            0xb0, b'h', 0xee, // mov al,'h' ; out dx,al
            0xb0, b'i', 0xee, // mov al,'i' ; out dx,al
            0xf4, // hlt
        ];
        assert_eq!(run(&code).console(), b"hi");
    }

    #[test]
    fn long_mode_paging_translates_through_four_levels() {
        // Build a 4 KiB-page mapping VA 0 → PA 0x5000 through PML4→PDPT→PD→PT
        // (entry 0 of each table, the tables at 0x1000/0x2000/0x3000/0x4000), then
        // enable long-mode paging and check translation honours it.
        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        put(&mut cpu, 0x1000, 0x2000 | 1); // PML4[0] → PDPT, present
        put(&mut cpu, 0x2000, 0x3000 | 1); // PDPT[0] → PD
        put(&mut cpu, 0x3000, 0x4000 | 1); // PD[0]   → PT
        put(&mut cpu, 0x4000, 0x5000 | 1); // PT[0]   → frame 0x5000
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = 1 << 8; // LME
        assert_eq!(cpu.translate(0x123), 0x123, "paging off → identity");
        cpu.cr0 = 1 << 31; // PG → paging on
        assert_eq!(cpu.translate(0x123), 0x5123, "VA 0x123 maps to PA 0x5123");
        // A write through the VA lands at the physical frame (rd/wr translate).
        cpu.wr(0x40, 4, 0xdead_beef);
        assert_eq!(
            cpu.rd_phys(0x5040, 4),
            0xdead_beef,
            "the write hit frame 0x5000"
        );
    }

    #[test]
    fn mov_cr_and_wrmsr_set_up_long_mode() {
        // mov rax, 0x1000 ; mov cr3, rax ; mov rax, 0x20 ; mov cr4, rax ;
        // mov ecx,0xC0000080 ; mov eax,0x100 ; xor edx,edx ; wrmsr ; rdmsr ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x00, 0x10, 0, 0, // mov rax, 0x1000
            0x0f, 0x22, 0xd8, // mov cr3, rax
            0xb9, 0x80, 0x00, 0x00, 0xc0, // mov ecx, 0xC0000080
            0xb8, 0x00, 0x01, 0x00, 0x00, // mov eax, 0x100 (LME)
            0x31, 0xd2, // xor edx, edx
            0x0f, 0x30, // wrmsr → EFER = 0x100
            0x0f, 0x32, // rdmsr → eax = EFER low
            0xf4, // hlt
        ];
        let cpu = run(&code);
        assert_eq!(cpu.cr3, 0x1000, "mov cr3 installed the PML4 base");
        assert_eq!(cpu.efer, 0x100, "wrmsr set EFER.LME");
        assert_eq!(
            cpu.reg(0) & 0xffff_ffff,
            0x100,
            "rdmsr read EFER back into eax"
        );
    }

    #[test]
    fn call_and_ret_use_the_stack() {
        // call +1 (push ret, jump over) ; hlt ; (target:) ret would pop — instead
        // verify push/pop directly: mov rax,0x1234 ; push rax ; pop rbx ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x34, 0x12, 0, 0,    // mov rax, 0x1234
            0x50, // push rax
            0x5b, // pop rbx
            0xf4, // hlt
        ];
        assert_eq!(run(&code).reg(3), 0x1234);
    }
}
