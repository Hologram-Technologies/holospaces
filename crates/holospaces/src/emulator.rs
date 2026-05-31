//! **System emulator** — a real RISC-V (RV64IMAC + Zicsr) machine, the core of
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
//! https://riscv.org/technical/specifications/[RISC-V] ISA as its external
//! authority (`CC-9`, arc42 chapter 10): it passes the **official
//! https://github.com/riscv-software-src/riscv-tests[riscv-tests] conformance
//! suite** — the same suite real hardware and QEMU are validated against. It
//! implements the base integer set, integer multiply/divide (M), atomics (A),
//! the compressed encoding (C), the control/status registers (Zicsr), and
//! trap handling across privilege levels — machine and supervisor mode with
//! delegation (`ecall`/`ebreak` exceptions → `mtvec`/`stvec`, `mcause`/`scause`,
//! `mret`/`sret`), and **Sv39 paging** (the page-table walk with accessed/dirty
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
use alloc::{vec, vec::Vec};

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
    /// An Sv39 page fault: `cause` is the RISC-V exception code (12 fetch / 13
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Fetch,
    Load,
    Store,
}

/// A RISC-V hart (hardware thread): 32 integer registers and a program counter.
#[derive(Clone)]
struct Hart {
    x: [u64; 32],
    pc: u64,
}

/// A minimal RISC-V machine: one hart over a flat little-endian RAM, with an
/// `ecall` console (the `write` syscall appends to [`Emulator::console`]).
///
/// RAM is mapped at [`Emulator::base`]; a flat guest image is loaded there and
/// the reset PC is `base`. The machine is deterministic — identical image +
/// identical input yield identical console output and identical final state,
/// so its κ snapshot is reproducible (Law L1/L5; `CC-9`).
pub struct Emulator {
    hart: Hart,
    ram: Vec<u8>,
    base: u64,
    console: Vec<u8>,
    /// Control and status registers (Zicsr) — a flat file; the privileged
    /// semantics (WARL fields, read-only CSRs) are a later conformance step.
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
}

/// The CLINT memory-mapped region (one hart): `msip` at +0, `mtimecmp` at
/// +0x4000, `mtime` at +0xBFF8.
const CLINT_BASE: u64 = 0x0200_0000;
const CLINT_END: u64 = 0x0201_0000;

impl Emulator {
    /// Create a machine with `ram_bytes` of RAM mapped at `base`, the reset PC.
    #[must_use]
    pub fn new(base: u64, ram_bytes: usize) -> Self {
        let mut csrs = BTreeMap::new();
        // `misa` reports the ISA: RV64 (MXL=2) with the I, M, A, C extensions a
        // kernel checks for. mhartid defaults to 0 (single hart).
        let misa = (2u64 << 62) | (1 << 0) | (1 << 2) | (1 << 8) | (1 << 12);
        csrs.insert(csr::MISA, misa);
        Self {
            hart: Hart {
                x: [0; 32],
                pc: base,
            },
            ram: vec![0; ram_bytes],
            base,
            console: Vec::new(),
            csrs,
            reservation: None,
            priv_level: PRIV_M,
            htif: None,
            mtime: 0,
            mtimecmp: 0,
            msip: false,
            provide_sbi: false,
        }
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
            csr::SSTATUS => self.raw_csr(csr::MSTATUS) & csr::SSTATUS_MASK,
            csr::SIE => self.raw_csr(csr::MIE) & csr::S_INT_MASK,
            csr::SIP => self.raw_csr(csr::MIP) & csr::S_INT_MASK,
            _ => self.raw_csr(csr),
        }
    }

    fn raw_csr(&self, csr: u32) -> u64 {
        self.csrs.get(&csr).copied().unwrap_or(0)
    }

    fn csr_write(&mut self, csr: u32, value: u64) {
        match csr {
            csr::SSTATUS => {
                let m =
                    (self.raw_csr(csr::MSTATUS) & !csr::SSTATUS_MASK) | (value & csr::SSTATUS_MASK);
                self.csrs.insert(csr::MSTATUS, m);
            }
            csr::SIE => {
                let m = (self.raw_csr(csr::MIE) & !csr::S_INT_MASK) | (value & csr::S_INT_MASK);
                self.csrs.insert(csr::MIE, m);
            }
            csr::SIP => {
                let m = (self.raw_csr(csr::MIP) & !csr::S_INT_MASK) | (value & csr::S_INT_MASK);
                self.csrs.insert(csr::MIP, m);
            }
            _ => {
                self.csrs.insert(csr, value);
            }
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

    /// The bytes the guest has written to fd 1/2 via the `write` syscall — its
    /// console output (the channel the emulator codemodule publishes).
    #[must_use]
    pub fn console(&self) -> &[u8] {
        &self.console
    }

    /// The current program counter (diagnostics / single-stepping).
    #[must_use]
    pub fn pc(&self) -> u64 {
        self.hart.pc
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
        out.extend_from_slice(&self.ram);
        out
    }

    /// Run until the guest exits, traps, or `max_steps` is reached.
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for _ in 0..max_steps {
            // At each instruction boundary: advance the timer, reconcile the
            // CLINT interrupt latches into `mip`, and take a pending interrupt
            // (which redirects the PC) before the next instruction.
            self.tick();
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

    fn load_phys(&self, addr: u64, width: usize) -> Result<u64, Trap> {
        if (CLINT_BASE..CLINT_END).contains(&addr) {
            return Ok(self.clint_read(addr));
        }
        let o = self.offset(addr, width)?;
        let mut v = 0u64;
        for i in 0..width {
            v |= (self.ram[o + i] as u64) << (8 * i);
        }
        Ok(v)
    }

    fn store_phys(&mut self, addr: u64, width: usize, value: u64) -> Result<(), Trap> {
        if (CLINT_BASE..CLINT_END).contains(&addr) {
            self.clint_write(addr, value);
            return Ok(());
        }
        let o = self.offset(addr, width)?;
        for i in 0..width {
            self.ram[o + i] = (value >> (8 * i)) as u8;
        }
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

    /// Advance the timer and reconcile the memory-mapped interrupt latches into
    /// `mip` (CLINT → MTIP/MSIP), called once per executed instruction.
    fn tick(&mut self) {
        self.mtime = self.mtime.wrapping_add(1);
        let mut mip = self.raw_csr(csr::MIP);
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
        self.csrs.insert(csr::MIP, mip);
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
        let pa = self.translate(addr, access)?;
        self.load_phys(pa, width)
    }

    /// A guest virtual store (translate then write).
    fn store(&mut self, addr: u64, width: usize, value: u64) -> Result<(), Trap> {
        let pa = self.translate(addr, Access::Store)?;
        self.store_phys(pa, width, value)
    }

    /// Translate a virtual address through Sv39 paging (RISC-V Privileged ISA
    /// §4.3-4.4) when `satp.MODE == Sv39` and the effective privilege is below
    /// machine; otherwise the address is physical (bare mode). Sets the
    /// accessed/dirty bits and enforces the page permissions and U/SUM/MXR.
    fn translate(&mut self, vaddr: u64, access: Access) -> Result<u64, Trap> {
        let satp = self.raw_csr(0x180);
        if satp >> 60 != 8 {
            return Ok(vaddr); // bare (no paging)
        }
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
        let vpn = [
            (vaddr >> 12) & 0x1ff,
            (vaddr >> 21) & 0x1ff,
            (vaddr >> 30) & 0x1ff,
        ];
        let mut a = (satp & 0xf_ffff_ffff_ffff) << 12;
        let mut level = 2i32;
        loop {
            let pte_addr = a + vpn[level as usize] * 8;
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
                let ppn = pte >> 10;
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
                return Ok(((ppn << 12) & !mask) | (vaddr & mask));
            }
            a = (pte >> 10) << 12;
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
        // instruction, expanded to its 32-bit equivalent (RISC-V ISA §16).
        let parcel = self.load(pc, 2, Access::Fetch).map_err(Halt::Trap)? as u16;
        let (inst, ilen) = if parcel & 3 == 3 {
            (
                self.load(pc, 4, Access::Fetch).map_err(Halt::Trap)? as u32,
                4u64,
            )
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
                        self.sret();
                        return Ok(());
                    } // SRET
                    0x1050_0073 => {}               // WFI — nop on this model
                    _ if (inst >> 25) == 0x09 => {} // SFENCE.VMA — nop (no TLB)
                    _ => return Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                }
            }
            0x73 => {
                // SYSTEM — Zicsr: CSRRW/S/C and their immediate forms. The source
                // is a register (funct3 1-3) or a 5-bit zimm (funct3 5-7).
                let csr = (inst >> 20) & 0xfff;
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
            0x02 => err = u64::MAX,              // console_getchar — no input
            0x00 => self.set_timer(a0),          // set_timer
            0x08 => return Err(Halt::Exit(0)),   // shutdown
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
