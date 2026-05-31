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
//! `mret`/`sret`) — so the official machine- and supervisor-mode tests run
//! unmodified. A flat program may instead use the `ecall` host boundary (console
//! `write` / `exit`) when it installs no trap vector.

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
    pub const MEDELEG: u32 = 0x302;
    pub const MTVEC: u32 = 0x305;
    pub const MIE: u32 = 0x304;
    pub const MEPC: u32 = 0x341;
    pub const MCAUSE: u32 = 0x342;
    pub const MTVAL: u32 = 0x343;
    pub const MIP: u32 = 0x344;

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
}

impl Emulator {
    /// Create a machine with `ram_bytes` of RAM mapped at `base`, the reset PC.
    #[must_use]
    pub fn new(base: u64, ram_bytes: usize) -> Self {
        Self {
            hart: Hart {
                x: [0; 32],
                pc: base,
            },
            ram: vec![0; ram_bytes],
            base,
            console: Vec::new(),
            csrs: BTreeMap::new(),
            reservation: None,
            priv_level: PRIV_M,
            htif: None,
        }
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
        let delegated = self.priv_level <= PRIV_S && (self.raw_csr(csr::MEDELEG) >> cause) & 1 != 0;
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
            self.hart.pc = self.csr_read(csr::STVEC) & !3;
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
            self.hart.pc = self.csr_read(csr::MTVEC) & !3;
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

    /// The current program counter (diagnostics).
    #[must_use]
    pub fn pc(&self) -> u64 {
        self.hart.pc
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
        out.extend_from_slice(&self.ram);
        out
    }

    /// Run until the guest exits, traps, or `max_steps` is reached.
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for _ in 0..max_steps {
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

    fn load(&self, addr: u64, width: usize) -> Result<u64, Trap> {
        let o = self.offset(addr, width)?;
        let mut v = 0u64;
        for i in 0..width {
            v |= (self.ram[o + i] as u64) << (8 * i);
        }
        Ok(v)
    }

    fn store(&mut self, addr: u64, width: usize, value: u64) -> Result<(), Trap> {
        let o = self.offset(addr, width)?;
        for i in 0..width {
            self.ram[o + i] = (value >> (8 * i)) as u8;
        }
        Ok(())
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
        let parcel = self.load(pc, 2).map_err(Halt::Trap)? as u16;
        let (inst, ilen) = if parcel & 3 == 3 {
            (self.load(pc, 4).map_err(Halt::Trap)? as u32, 4u64)
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
                let v = match funct3 {
                    0 => sext(self.load(addr, 1).map_err(Halt::Trap)?, 8), // LB
                    1 => sext(self.load(addr, 2).map_err(Halt::Trap)?, 16), // LH
                    2 => sext(self.load(addr, 4).map_err(Halt::Trap)?, 32), // LW
                    3 => self.load(addr, 8).map_err(Halt::Trap)?,          // LD
                    4 => self.load(addr, 1).map_err(Halt::Trap)?,          // LBU
                    5 => self.load(addr, 2).map_err(Halt::Trap)?,          // LHU
                    6 => self.load(addr, 4).map_err(Halt::Trap)?,          // LWU
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
                        let v = self.load(addr, width).map_err(Halt::Trap)?;
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
                        let old = self.load(addr, width).map_err(Halt::Trap)?;
                        let res = amo_op(funct5, old, self.rd(rs2), width);
                        self.store(addr, width, res).map_err(Halt::Trap)?;
                        self.wr(rd, amo_extend(old, width));
                    }
                }
            }
            0x0f => { /* FENCE / FENCE.I — ordering no-op on this model */ }
            0x73 if funct3 == 0 => {
                // SYSTEM — ECALL / EBREAK / xRET / WFI / SFENCE.VMA.
                return match inst {
                    0x0000_0073 => self.ecall(),                      // ECALL
                    0x0010_0073 => Err(Halt::Trap(Trap::Breakpoint)), // EBREAK
                    0x3020_0073 => {
                        self.mret();
                        Ok(())
                    } // MRET
                    0x1020_0073 => {
                        self.sret();
                        Ok(())
                    } // SRET
                    0x1050_0073 => Ok(()),                            // WFI — nop on this model
                    _ if (inst >> 25) == 0x09 => Ok(()),              // SFENCE.VMA — nop (no TLB)
                    _ => Err(Halt::Trap(Trap::IllegalInstruction(inst))),
                };
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
                    let byte = self.load(buf.wrapping_add(i), 1).map_err(Halt::Trap)? as u8;
                    self.console.push(byte);
                }
                self.wr(10, len); // return bytes written
                self.hart.pc = self.hart.pc.wrapping_add(4);
                Ok(())
            }
            other => Err(Halt::Trap(Trap::UnknownSyscall(other))),
        }
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
