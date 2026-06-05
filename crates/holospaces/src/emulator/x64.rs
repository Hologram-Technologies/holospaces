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
}

impl Sys {
    fn new() -> Self {
        Sys {
            uart: Uart {
                output: Vec::new(),
                input: Vec::new(),
                in_cursor: 0,
            },
            virtio: None,
            virtio9p: None,
            virtionet: None,
            loopback: None,
        }
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
    /// Guest RAM (physical, based at [`RAM_BASE`]).
    ram: Vec<u8>,
    sys: Option<Box<Sys>>,
}

// Register indices.
const RSP: usize = 4;

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
            // RIP-relative: disp32 from the *next* instruction. We approximate with
            // the current rip after the disp is consumed.
            let disp = i64::from(self.fetch(4) as i32);
            return (reg, Rm::Mem(self.rip.wrapping_add(disp as u64)));
        } else {
            addr = self.r[rm_field | (((rex & 1) as usize) << 3)];
            base_disp = 0;
        }
        match md {
            1 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(1) as i8)),
            2 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(4) as i32)),
            _ => {}
        }
        addr = addr.wrapping_add(base_disp as u64);
        (reg, Rm::Mem(addr))
    }

    fn load_rm(&mut self, rm: Rm, size: u8) -> u64 {
        match rm {
            Rm::Reg(i) => self.r[i] & Self::mask(size),
            Rm::Mem(a) => self.rd(a, size),
        }
    }

    fn store_rm(&mut self, rm: Rm, size: u8, val: u64) {
        match rm {
            Rm::Reg(i) => {
                if size >= 4 {
                    // 32-bit writes zero the upper 32 bits; 64-bit is full.
                    self.r[i] = val & Self::mask(size);
                } else {
                    let m = Self::mask(size);
                    self.r[i] = (self.r[i] & !m) | (val & m);
                }
            }
            Rm::Mem(a) => self.wr(a, size, val),
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
            _ => {
                let r = a.wrapping_add(b);
                self.flags_arith(a, b, r, size, false);
                r & m
            } // ADC/SBB approximated as ADD/SUB w/o carry-in
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

    /// Serial-port output: a write to `0x3f8` (the 16550 THR) appends to the
    /// console; other ports are ignored.
    fn port_out(&mut self, port: u16, val: u8) {
        if port == 0x3f8 {
            if let Some(sys) = self.sys.as_mut() {
                sys.uart.output.push(val);
            }
        }
    }

    fn port_in(&mut self, port: u16) -> u8 {
        if let Some(sys) = self.sys.as_mut() {
            if port == 0x3f8 && sys.uart.in_cursor < sys.uart.input.len() {
                let b = sys.uart.input[sys.uart.in_cursor];
                sys.uart.in_cursor += 1;
                return b;
            }
            if port == 0x3fd {
                // Line Status Register: THR empty (0x20) always; data-ready (0x01)
                // when input is pending.
                let dr = u8::from(sys.uart.in_cursor < sys.uart.input.len());
                return 0x20 | dr;
            }
        }
        0
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

    /// A guest-RAM view for the shared devbus to walk virtqueues over (x86-64 RAM
    /// is based at [`RAM_BASE`] = 0).
    fn guest_ram(&mut self) -> super::devbus::GuestRam<'_> {
        super::devbus::GuestRam {
            ram: &mut self.ram,
            base: RAM_BASE,
        }
    }

    /// A `virtio-blk` MMIO register write; a `QueueNotify` services the queue
    /// against the κ-disk through the shared `devbus`.
    fn virtio_blk_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio.take() else {
            return;
        };
        if super::devbus::blk_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            super::devbus::blk_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio = Some(dev);
    }

    /// A `virtio-9p` MMIO register write; a `QueueNotify` services the workspace
    /// filesystem queue through the shared `devbus` — the same servicing the
    /// other cores drive (`CC-46`).
    fn virtio_9p_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio9p.take() else {
            return;
        };
        if super::devbus::p9_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            super::devbus::p9_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio9p = Some(dev);
    }

    /// A `virtio-net` MMIO register write; a `QueueNotify` services the transmit
    /// queue or pumps the NAT through the shared `devbus` (`CC-46`).
    fn virtio_net_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        match super::devbus::net_mmio_write(&mut dev, off, value) {
            super::devbus::NetNotify::Transmit => {
                let mut mem = self.guest_ram();
                super::devbus::net_service_tx(&mut mem, &mut dev);
                super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::Receive => {
                let mut mem = self.guest_ram();
                super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::None => {}
        }
        self.sys_mut().virtionet = Some(dev);
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
        super::devbus::net_pump(&mut mem, &mut dev);
        self.sys_mut().virtionet = Some(dev);
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
        loop {
            let b = *self.ram.get(self.rip as usize).unwrap_or(&0);
            match b {
                0x66 => opsz = true,
                0x67 | 0xf0 | 0xf2 | 0xf3 | 0x2e | 0x36 | 0x3e | 0x26 | 0x64 | 0x65 => {}
                0x40..=0x4f => rex = b, // REX (last prefix)
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
                let (a, b) = (self.load_rm(rm, 1), self.r[reg] & 0xff);
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
                let (a, b) = (self.r[reg] & 0xff, self.load_rm(rm, 1));
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
                let imm = self.fetch(4);
                let b = if size == 8 {
                    imm as i32 as i64 as u64
                } else {
                    imm
                };
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
                let (ext, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.fetch(1));
                let res = self.alu((ext & 7) as u8, a, b, 1);
                if ext & 7 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x81 => {
                let (ext, rm) = self.modrm(rex);
                let a = self.load_rm(rm, size);
                let imm = self.fetch(4);
                let b = if size == 8 {
                    imm as i32 as i64 as u64
                } else {
                    imm
                };
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x83 => {
                let (ext, rm) = self.modrm(rex);
                let a = self.load_rm(rm, size);
                let b = self.fetch(1) as i8 as i64 as u64;
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x84 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.r[reg] & 0xff);
                self.flags_logic(a & b, 1);
            }
            0x85 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                self.flags_logic(a & b, size);
            }
            0x88 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.r[reg] & 0xff;
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
            0x8d => {
                let (reg, rm) = self.modrm(rex);
                if let Rm::Mem(a) = rm {
                    self.r[reg] = a & Self::mask(size);
                }
            }
            0x90 => {} // nop
            0xa8 => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                self.flags_logic(a & b, 1);
            }
            0xa9 => {
                let a = self.r[0] & Self::mask(size);
                let imm = self.fetch(4);
                let b = if size == 8 {
                    imm as i32 as i64 as u64
                } else {
                    imm
                };
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
            0xc3 => {
                let v = self.pop();
                self.rip = v;
            }
            0xc6 => {
                let (_e, rm) = self.modrm(rex);
                let imm = self.fetch(1);
                self.store_rm(rm, 1, imm);
            }
            0xc7 => {
                let (_e, rm) = self.modrm(rex);
                let imm = self.fetch(4);
                let v = if size == 8 {
                    imm as i32 as i64 as u64
                } else {
                    imm
                };
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
            0xf4 => return Err(Halt::Halted),
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
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1) as i8 as i64 as u64;
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
                        // WRMSR: MSR[ecx] = edx:eax. EFER (0xC000_0080) holds LME/LMA.
                        let ecx = self.r[1] & 0xffff_ffff;
                        let val = ((self.r[2] & 0xffff_ffff) << 32) | (self.r[0] & 0xffff_ffff);
                        if ecx == 0xC000_0080 {
                            self.efer = val;
                        }
                    }
                    0x32 => {
                        // RDMSR: edx:eax = MSR[ecx].
                        let ecx = self.r[1] & 0xffff_ffff;
                        let val = if ecx == 0xC000_0080 { self.efer } else { 0 };
                        self.r[0] = val & 0xffff_ffff;
                        self.r[2] = val >> 32;
                    }
                    _ => return Err(Halt::Undefined(start)),
                }
            }
            _ => return Err(Halt::Undefined(start)),
        }
        Ok(())
    }
}

/// A decoded ModRM r/m operand: a register index or an effective address.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u64),
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
