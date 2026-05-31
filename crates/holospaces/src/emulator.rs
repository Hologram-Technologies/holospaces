//! **System emulator** — a real RISC-V (RV64IMA + Zicsr) machine, the core of
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
//! The core is grown conformance-first against the
//! https://riscv.org/technical/specifications/[RISC-V] ISA as its external
//! authority (`CC-9`, arc42 chapter 10): it executes real RISC-V machine code —
//! assembled by LLVM's RISC-V backend, in the self-checking style of
//! https://github.com/riscv-software-src/riscv-tests[riscv-tests] — and must
//! reproduce the ISA semantics exactly. This is the base integer set, integer
//! multiply/divide (M), atomics (A), and the control/status registers (Zicsr) —
//! RV64IMA + Zicsr — plus the `ecall` boundary a guest uses for console output
//! and exit. The remaining privileged architecture (traps, Sv39 paging,
//! CLINT/PLIC, SBI) and the compressed (C) and floating-point (FD) extensions
//! that a full Linux boot needs are layered on top of this same core in
//! subsequent conformance steps.

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
        }
    }

    fn csr_read(&self, csr: u32) -> u64 {
        self.csrs.get(&csr).copied().unwrap_or(0)
    }

    fn csr_write(&mut self, csr: u32, value: u64) {
        self.csrs.insert(csr, value);
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

    /// Execute one instruction. `Ok(())` advances; `Err(Halt)` stops.
    fn step(&mut self) -> Result<(), Halt> {
        let pc = self.hart.pc;
        let inst = self.load(pc, 4).map_err(Halt::Trap)? as u32;
        // (RV64I is fixed 32-bit; the C extension is a later conformance step.)
        let opcode = inst & 0x7f;
        let rd = (inst >> 7) & 0x1f;
        let rs1 = (inst >> 15) & 0x1f;
        let rs2 = (inst >> 20) & 0x1f;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = (inst >> 25) & 0x7f;
        let mut next = pc.wrapping_add(4);

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
                    1 => a << (rs2 & 0x3f),                  // SLLI (shamt 6b)
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
                // SYSTEM — ECALL / EBREAK.
                match inst >> 7 {
                    0x0000_0000 => return self.ecall(), // ECALL (imm=0, rd/rs1=0)
                    0x0010_0000 => return Err(Halt::Trap(Trap::Breakpoint)), // EBREAK (imm=1)
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
