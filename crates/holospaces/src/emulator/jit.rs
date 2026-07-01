//! The block JIT's decode → IR → codegen front-end (M2 Rung 3).
//!
//! A typed micro-op IR over the x86-64 integer register file + guest RAM, an x86-64 decoder
//! (reg-reg ALU/mov + memory forms with full SIB addressing), a register-allocated
//! IR→WebAssembly-bytecode codegen, and an inline software-TLB address-translation path with
//! a miss/bail back to the interpreter. `compile`/`compile_tlb` emit a bare wasm function the
//! caller runs (native: `wasmtime`; browser: `js_sys::WebAssembly`) — so this module stays
//! `no_std` + `alloc` (pure byte codegen, no runtime dependency). Every layer is proven
//! bit-identical to a Rust interpreter oracle by the seeded-random differentials in the test
//! module below; it covers every instruction shape `sha512_transform` uses. `run()` dispatch
//! and the BLAKE3 block cache (Step C) are wired in `x64.rs`.
#![allow(dead_code)] // the JIT API is exercised by the test module; run() dispatch lands in Step C

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

#[cfg(test)]
use wasmtime::{Engine, Instance, Module, Store};

const NREG: usize = 16;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Bin {
    Add,
    Sub,
    Xor,
    And,
    Or,
}
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Sh {
    ShrU,
    Shl,
    Rotr,
}
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Op {
    Movi { d: u8, imm: u64 },
    Movr { d: u8, s: u8 },
    Bin { op: Bin, d: u8, a: u8, b: u8 },
    Shift { op: Sh, d: u8, a: u8, sh: u8 },
    /// `d = [base + idx<<scale + disp]` — full x86 effective address (SHA-512's `W[]`
    /// stack load and `K[]` SIB-indexed table load).
    Load { d: u8, base: u8, idx: u8, scale: u8, disp: i32 },
    /// `[base + idx<<scale + disp] = s`.
    Store { base: u8, idx: u8, scale: u8, disp: i32, s: u8 },
    /// `d = d op [base + idx<<scale + disp]` — ALU op with a memory source operand
    /// (`add reg, [mem]`), the round's `+W[t]` / `+K[t]`.
    LoadOp { op: Bin, d: u8, base: u8, idx: u8, scale: u8, disp: i32 },
    /// `d = base + idx<<scale + disp` — the effective ADDRESS, no memory access (x86 `lea`).
    /// Pure 64-bit arithmetic (no flags, no TLB) — a hot instruction in compiled code (address
    /// math and 3-operand adds). Broadens block coverage past the load/store/ALU subset.
    Lea { d: u8, base: u8, idx: u8, scale: u8, disp: i32 },
    /// `cmp a, b` — set rflags from `a - b`, write NO register (x86 `cmp`). A loop's condition.
    Cmp { a: u8, b: u8 },
    /// `test a, b` — set rflags from `a & b`, write NO register (x86 `test`). A loop's condition.
    Test { a: u8, b: u8 },
    /// `d = d op imm` with rflags — x86 ALU with a sign-extended immediate (`0x83`/`0x81`,
    /// e.g. `add rax, 1` / `and rcx, -16`). A counter step or mask; pervasive in real loop bodies.
    BinImm { op: Bin, d: u8, imm: u64 },
    /// `cmp a, imm` — set rflags from `a - imm`, write NO register (x86 `cmp r, imm`, `0x83 /7`).
    /// The other half of a counted loop's condition (compare against a constant bound).
    CmpImm { a: u8, imm: u64 },
    /// `push s` — `rsp -= 8; [rsp] = s` (x86 `0x50+r`). The source value is captured BEFORE the
    /// `rsp` decrement (so `push rsp` stores the OLD rsp). A stack-memory op: the access goes
    /// through the inline TLB and may bail (the stack page faults in) leaving `rsp` untouched.
    Push { s: u8 },
    /// `pop d` — `d = [rsp]; rsp += 8` (x86 `0x58+r`). The load uses the original `rsp`, then `rsp`
    /// is incremented, then `d` is written (so `pop rsp` ends with `rsp = [old rsp]`). Stack-memory.
    Pop { d: u8 },
}

/// x86 register index of `rsp` (the stack pointer) — `push`/`pop` address through it.
const RSP: u8 = 4;

/// Address-mode sentinels: no base register / no index register.
pub(crate) const NO_REG: u8 = 0xff;

/// Guest RAM lives in the same wasm memory as the register file, after a page of
/// headroom: regs at `r*8`, guest byte `A` at wasm offset `GUEST_BASE + A`.
pub(crate) const GUEST_BASE: u64 = 0x1000;
#[cfg(test)]
const GUEST_LEN: usize = 0x3000; // 3 pages of test guest RAM

/// Software TLB region in wasm memory (between the regs and guest RAM): a direct-mapped
/// array of `TLB_SIZE` 16-byte entries `(tag: vpage @0, host_off: byte offset @8)`. The
/// inline-TLB codegen translates a guest virtual address by indexing this on the hit path
/// — the mechanism that lets the JIT touch real (paged) guest RAM. `$va` scratch = local 16.
pub(crate) const TLB_BASE: u64 = 0x200;
pub(crate) const TLB_SIZE: u64 = 64; // power of two; slot = vpage & (TLB_SIZE-1)

/// The guest `rflags` slot in wasm memory (just past the 16 registers), and the flag bits
/// an ALU op writes: CF(0) PF(2) ZF(6) SF(7) OF(11). AF(4) is deliberately NOT modelled —
/// `x64.rs::flags_arith`/`flags_logic` leave AF untouched, so to stay bit-identical to
/// `step()` the JIT must leave it untouched too.
pub(crate) const RFLAGS_MEM: u64 = NREG as u64 * 8; // mem offset 128
const ALU_FLAGS_MASK: u64 = 0x8c5; // CF|PF|ZF|SF|OF (no AF — matches the interpreter)
/// `compile_region` exit signalling (between RFLAGS and the TLB image): a region that exits on a
/// TLB miss writes the faulting vaddr to `VADDR_MEM` and `1` to `MISS_MEM`, so the pooled executor
/// fetches that page and retries (the region analogue of `exec_block_pooled`'s bail-index + vaddr
/// recompute). A normal/yield exit leaves `MISS_MEM = 0` → the executor returns it.
pub(crate) const MISS_MEM: u64 = 0x90;
pub(crate) const VADDR_MEM: u64 = 0x98;
/// The region's guest EXIT rip is written here (not returned), so the region wasm is `() -> ()` —
/// browser-robust (a wasm `i64` *return* to JS needs BigInt integration; a memory read is trivial).
pub(crate) const EXIT_RIP_MEM: u64 = 0xa0;

/// The region-executor memory ABI — the offsets into the region wasm's linear `Memory` that ANY
/// executor marshals to: the native `wasmtime` one (`super::jit_exec`) and the browser's
/// `js_sys::WebAssembly` twin (in `holospaces-web`), injected through `x64::set_region_executor`.
///
/// The browser executor writes the entry regs (`REGS_BASE`), `rflags` (`RFLAGS_MEM`), the software
/// TLB image (`TLB_BASE`, `TLB_SIZE` × 16 B), and the page pool (`GUEST_BASE`), calls the region's
/// `() -> ()` `run` export, then reads back regs/rflags/pool plus the miss signal (`MISS_MEM` /
/// `VADDR_MEM`) and the exit rip (`EXIT_RIP_MEM`). These public mirrors keep the codegen constants
/// private; `abi_mirrors_codegen` asserts they stay in lock-step.
pub mod abi {
    /// Register file base: guest register `r` at wasm byte `r * 8` (16 regs span `0..128`).
    pub const REGS_BASE: u64 = 0;
    /// Guest `rflags` slot (just past the 16 registers).
    pub const RFLAGS_MEM: u64 = super::RFLAGS_MEM;
    /// TLB-miss flag: the region writes `1` here when it exits on a miss (else `0`).
    pub const MISS_MEM: u64 = super::MISS_MEM;
    /// The faulting guest virtual address the region wrote on a miss.
    pub const VADDR_MEM: u64 = super::VADDR_MEM;
    /// The region's guest exit rip (where the interpreter resumes).
    pub const EXIT_RIP_MEM: u64 = super::EXIT_RIP_MEM;
    /// Software-TLB image base: a direct-mapped array of `TLB_SIZE` 16-byte entries.
    pub const TLB_BASE: u64 = super::TLB_BASE;
    /// Number of TLB slots (power of two; `slot = vpage & (TLB_SIZE - 1)`).
    pub const TLB_SIZE: u64 = super::TLB_SIZE;
    /// Page pool / guest-RAM base: guest byte at pool offset `o` lives at `GUEST_BASE + o`.
    pub const GUEST_BASE: u64 = super::GUEST_BASE;
}

#[cfg(test)]
#[test]
fn abi_mirrors_codegen() {
    // The public executor ABI must equal the private codegen constants — one source of truth.
    assert_eq!(abi::RFLAGS_MEM, RFLAGS_MEM);
    assert_eq!(abi::MISS_MEM, MISS_MEM);
    assert_eq!(abi::VADDR_MEM, VADDR_MEM);
    assert_eq!(abi::EXIT_RIP_MEM, EXIT_RIP_MEM);
    assert_eq!(abi::TLB_BASE, TLB_BASE);
    assert_eq!(abi::TLB_SIZE, TLB_SIZE);
    assert_eq!(abi::GUEST_BASE, GUEST_BASE);
    assert_eq!(abi::REGS_BASE, 0);
}

// Local layout: 21 × i64 then 2 × i32 (always declared; unused ones are harmless).
const VA_LOCAL: u8 = NREG as u8; // i64 16: vaddr scratch (TLB)
const RFLAGS_LOCAL: u8 = NREG as u8 + 1; // i64 17: live rflags value (flags mode)
const FA_LOCAL: u8 = NREG as u8 + 2; // i64 18: flag operand a
const FB_LOCAL: u8 = NREG as u8 + 3; // i64 19: flag operand b
const FR_LOCAL: u8 = NREG as u8 + 4; // i64 20: flag result
const BAIL_LOCAL: u8 = NREG as u8 + 5; // i32 21: bail instruction index (TLB)
const TE_LOCAL: u8 = NREG as u8 + 6; // i32 22: TLB entry address scratch

/// Whether a block contains a `Shift`/`Rotr` op — whose flags the codegen does not yet
/// model, so such a block diverges from `step()` on `rflags` (a known gap, used to diagnose
/// shadow mismatches and to gate the JIT off those blocks until shift flags land).
pub(crate) fn block_has_shift(ops: &[Op]) -> bool {
    ops.iter().any(|o| matches!(o, Op::Shift { .. }))
}

/// The address mode `(base, idx, scale, disp)` of a memory op (`None` for non-memory ops) —
/// lets the executor recompute a faulting access's vaddr from `ops[bail]` + the regs.
pub(crate) fn op_mem_addr(op: &Op) -> Option<(u8, u8, u8, i32)> {
    match *op {
        Op::Load { base, idx, scale, disp, .. }
        | Op::Store { base, idx, scale, disp, .. }
        | Op::LoadOp { base, idx, scale, disp, .. } => Some((base, idx, scale, disp)),
        // push faults at [rsp-8], pop at [rsp]; rsp is unmodified at the bail (the access bails
        // before the rsp adjustment), so the recompute from the returned regs is exact.
        Op::Push { .. } => Some((RSP, NO_REG, 0, -8)),
        Op::Pop { .. } => Some((RSP, NO_REG, 0, 0)),
        _ => None,
    }
}

/// Effective address `base + idx<<scale + disp` (sentinels skipped) — the meaning shared
/// by the interpreter oracle and (mirrored in wasm) the codegen.
pub(crate) fn eff_addr(r: &[u64; NREG], base: u8, idx: u8, scale: u8, disp: i32) -> usize {
    let mut a = disp as i64 as u64;
    if base != NO_REG {
        a = a.wrapping_add(r[base as usize]);
    }
    if idx != NO_REG {
        a = a.wrapping_add(r[idx as usize] << scale);
    }
    a as usize
}

/// The reference oracle — the meaning of the IR, in plain Rust.
#[cfg(test)]
fn interpret(block: &[Op], r: &mut [u64; NREG], ram: &mut [u8]) {
    for op in block {
        match *op {
            Op::Movi { d, imm } => r[d as usize] = imm,
            Op::Movr { d, s } => r[d as usize] = r[s as usize],
            Op::Load { d, base, idx, scale, disp } => {
                let a = eff_addr(r, base, idx, scale, disp);
                r[d as usize] = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
            }
            Op::Store { base, idx, scale, disp, s } => {
                let a = eff_addr(r, base, idx, scale, disp);
                ram[a..a + 8].copy_from_slice(&r[s as usize].to_le_bytes());
            }
            Op::LoadOp { op, d, base, idx, scale, disp } => {
                let a = eff_addr(r, base, idx, scale, disp);
                let m = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
                let dv = r[d as usize];
                r[d as usize] = match op {
                    Bin::Add => dv.wrapping_add(m),
                    Bin::Sub => dv.wrapping_sub(m),
                    Bin::Xor => dv ^ m,
                    Bin::And => dv & m,
                    Bin::Or => dv | m,
                };
            }
            Op::Lea { d, base, idx, scale, disp } => {
                r[d as usize] = eff_addr(r, base, idx, scale, disp) as u64; // address, not a load
            }
            Op::Cmp { .. } | Op::Test { .. } | Op::CmpImm { .. } => {} // flag-only — ignored here
            Op::Push { s } => {
                let val = r[s as usize]; // capture BEFORE the rsp decrement (push rsp = old rsp)
                let a = r[RSP as usize].wrapping_sub(8) as usize;
                ram[a..a + 8].copy_from_slice(&val.to_le_bytes());
                r[RSP as usize] = a as u64;
            }
            Op::Pop { d } => {
                let a = r[RSP as usize] as usize;
                let val = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
                r[RSP as usize] = r[RSP as usize].wrapping_add(8); // increment, then write d
                r[d as usize] = val; // pop rsp → rsp = [old rsp] (the write wins)
            }
            Op::BinImm { op, d, imm } => {
                let dv = r[d as usize];
                r[d as usize] = match op {
                    Bin::Add => dv.wrapping_add(imm),
                    Bin::Sub => dv.wrapping_sub(imm),
                    Bin::Xor => dv ^ imm,
                    Bin::And => dv & imm,
                    Bin::Or => dv | imm,
                };
            }
            Op::Bin { op, d, a, b } => {
                let (a, b) = (r[a as usize], r[b as usize]);
                r[d as usize] = match op {
                    Bin::Add => a.wrapping_add(b),
                    Bin::Sub => a.wrapping_sub(b),
                    Bin::Xor => a ^ b,
                    Bin::And => a & b,
                    Bin::Or => a | b,
                };
            }
            Op::Shift { op, d, a, sh } => {
                let a = r[a as usize];
                let s = u32::from(sh) & 63;
                r[d as usize] = match op {
                    Sh::ShrU => a >> s,
                    Sh::Shl => a << s,
                    Sh::Rotr => a.rotate_right(s),
                };
            }
        }
    }
}

// ── wasm encoding helpers (hand-emitted, like the proven PoC) ──────────────────────────
fn uleb(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            out.push(b | 0x80);
        } else {
            out.push(b);
            break;
        }
    }
}
fn sleb(mut n: i64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7; // arithmetic
        let done = (n == 0 && b & 0x40 == 0) || (n == -1 && b & 0x40 != 0);
        if done {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}
fn section(id: u8, body: Vec<u8>, out: &mut Vec<u8>) {
    out.push(id);
    uleb(body.len() as u64, out);
    out.extend(body);
}

/// Direct-mapped codegen: guest address maps straight to `GUEST_BASE + addr` (no paging),
/// no flags — the model the register/memory unit differentials use.
pub(crate) fn compile(block: &[Op]) -> Vec<u8> {
    compile_mode(block, false, false)
}

/// Inline-TLB codegen: a guest *virtual* address is translated through the software TLB
/// (hit path) before the load/store — the real paged-memory model the boot needs.
pub(crate) fn compile_tlb(block: &[Op]) -> Vec<u8> {
    compile_mode(block, true, false)
}

/// Direct codegen that also maintains the guest `rflags` (CF/PF/AF/ZF/SF/OF) for the ALU
/// ops — what the boot's `regs`+`rflags` differential against `step()` requires.
pub(crate) fn compile_flags(block: &[Op]) -> Vec<u8> {
    compile_mode(block, false, true)
}

/// The codegen the live `run()` dispatch uses: inline-TLB address translation (with
/// miss/bail) AND `rflags` maintenance, composed. Returns an `i32` (bail index / block
/// length). Note: `Shift`/`Rotr` flags are not yet modelled — a block whose shift flags are
/// live must bail (the boot's differential-vs-`step()` enforces this).
pub(crate) fn compile_tlb_flags(block: &[Op]) -> Vec<u8> {
    compile_mode(block, true, true)
}

/// Emit the x86 rflags update for an ALU op into `$rflags` (RFLAGS_LOCAL), reading operands a/b in
/// `$fa`/`$fb` and the result in `$fr` (the caller sets them). Folds CF/PF/ZF/SF/OF exactly as the
/// `interpret`-side `x86_alu_flags` oracle; logical ops clear CF/OF/AF. Module-level so both
/// `compile_mode` and the chaining-loop codegen share one definition.
fn emit_flags(op: Bin, c: &mut Vec<u8>) {
    let get = |l: u8, c: &mut Vec<u8>| {
        c.push(0x20);
        uleb(u64::from(l), c);
    };
    // RFLAGS &= ~ALU_FLAGS_MASK  (keep IF/DF/reserved bits, clear the 6 ALU flags)
    get(RFLAGS_LOCAL, c);
    c.push(0x42);
    sleb(!ALU_FLAGS_MASK as i64, c);
    c.push(0x83); // i64.and
    c.push(0x21);
    uleb(u64::from(RFLAGS_LOCAL), c);
    let or_in = |c: &mut Vec<u8>| {
        get(RFLAGS_LOCAL, c);
        c.push(0x84); // i64.or
        c.push(0x21);
        uleb(u64::from(RFLAGS_LOCAL), c);
    };
    let shl = |n: i64, c: &mut Vec<u8>| {
        c.push(0x42);
        sleb(n, c);
        c.push(0x86); // i64.shl
    };
    // ZF = (fr == 0) << 6
    get(FR_LOCAL, c);
    c.push(0x50);
    c.push(0xad);
    shl(6, c);
    or_in(c);
    // SF = (fr >> 63) << 7
    get(FR_LOCAL, c);
    c.push(0x42);
    sleb(63, c);
    c.push(0x88);
    shl(7, c);
    or_in(c);
    // PF = (popcount(fr & 0xff) even) << 2
    get(FR_LOCAL, c);
    c.push(0x42);
    sleb(0xff, c);
    c.push(0x83);
    c.push(0x7b); // i64.popcnt
    c.push(0x42);
    sleb(1, c);
    c.push(0x83);
    c.push(0x50);
    c.push(0xad);
    shl(2, c);
    or_in(c);
    match op {
        Bin::Add | Bin::Sub => {
            if matches!(op, Bin::Add) {
                get(FR_LOCAL, c);
                get(FA_LOCAL, c);
                c.push(0x54); // CF = fr <u fa
                c.push(0xad);
                or_in(c);
                get(FA_LOCAL, c);
                get(FR_LOCAL, c);
                c.push(0x85);
                get(FB_LOCAL, c);
                get(FR_LOCAL, c);
                c.push(0x85);
                c.push(0x83); // OF = ((fa^fr)&(fb^fr))
            } else {
                get(FA_LOCAL, c);
                get(FB_LOCAL, c);
                c.push(0x54); // CF = fa <u fb
                c.push(0xad);
                or_in(c);
                get(FA_LOCAL, c);
                get(FB_LOCAL, c);
                c.push(0x85);
                get(FA_LOCAL, c);
                get(FR_LOCAL, c);
                c.push(0x85);
                c.push(0x83); // OF = ((fa^fb)&(fa^fr))
            }
            c.push(0x42);
            sleb(63, c);
            c.push(0x88);
            shl(11, c);
            or_in(c);
        }
        Bin::And | Bin::Or | Bin::Xor => {} // CF=OF=AF=0 (already cleared)
    }
}

/// CHAINING PROOF (S1) — compile a single-block **self-loop** into one wasm unit: the `body` ops
/// run repeatedly, registers persisting in wasm locals across iterations (NO per-iteration
/// marshalling — the measured 25× path), continuing while guest register `cond_reg != 0` and
/// bailing after `max_iters` (the interrupt-yield bound — a chained region must yield so the run
/// loop services timers; the hrtimer-storm lesson). Returns the iteration count as i32; regs are
/// written back to memory at exit. Flag-free (the condition is a direct register test) so it proves
/// the dispatcher-loop structure without the flag/cc surface; the `jcc`-driven variant composes
/// [`emit_cc`] in place of the register test. Body ops: `Movi/Movr/Bin/Shift` (reg dataflow).
pub(crate) fn compile_counted_loop(body: &[Op], cond_reg: u8, max_iters: u64) -> Vec<u8> {
    const ITER: u8 = NREG as u8; // i64 local 16 = the in-wasm iteration counter
    let mut code = Vec::new();
    uleb(1, &mut code);
    uleb(u64::from(ITER) + 1, &mut code); // (NREG + 1) × i64
    code.push(0x7e);
    let getl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x20);
        uleb(u64::from(r), c);
    };
    let setl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x21);
        uleb(u64::from(r), c);
    };
    // entry: regs → locals, ITER = 0
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code);
        code.extend([0x29, 0x03, 0x00]); // i64.load mem[r*8]
        setl(r as u8, &mut code);
    }
    code.push(0x42);
    sleb(0, &mut code);
    setl(ITER, &mut code);
    code.push(0x02);
    code.push(0x40); // block $exit
    code.push(0x03);
    code.push(0x40); // loop $top
    for op in body {
        match *op {
            Op::Movi { d, imm } => {
                code.push(0x42);
                sleb(imm as i64, &mut code);
                setl(d, &mut code);
            }
            Op::Movr { d, s } => {
                getl(s, &mut code);
                setl(d, &mut code);
            }
            Op::Bin { op, d, a, b } => {
                getl(a, &mut code);
                getl(b, &mut code);
                code.push(match op {
                    Bin::Add => 0x7c,
                    Bin::Sub => 0x7d,
                    Bin::Xor => 0x85,
                    Bin::And => 0x83,
                    Bin::Or => 0x84,
                });
                setl(d, &mut code);
            }
            Op::Shift { op, d, a, sh } => {
                getl(a, &mut code);
                code.push(0x42);
                sleb(i64::from(sh), &mut code);
                code.push(match op {
                    Sh::ShrU => 0x88,
                    Sh::Shl => 0x86,
                    Sh::Rotr => 0x8a,
                });
                setl(d, &mut code);
            }
            _ => panic!("compile_counted_loop body supports reg-dataflow ops only"),
        }
    }
    // ITER += 1
    getl(ITER, &mut code);
    code.push(0x42);
    sleb(1, &mut code);
    code.push(0x7c); // i64.add
    setl(ITER, &mut code);
    // if ITER >= max_iters → br $exit (depth 1): the interrupt-yield bound
    getl(ITER, &mut code);
    code.push(0x42);
    sleb(max_iters as i64, &mut code);
    code.push(0x5a); // i64.ge_u → i32
    code.extend([0x0d, 0x01]); // br_if $exit
    // if cond_reg != 0 → br $top (depth 0): continue the loop
    getl(cond_reg, &mut code);
    code.push(0x42);
    sleb(0, &mut code);
    code.push(0x52); // i64.ne → i32
    code.extend([0x0d, 0x00]); // br_if $top
    code.push(0x0b); // end loop (fall-through = cond_reg==0 → exit)
    code.push(0x0b); // end block $exit
    // exit: regs → memory, return ITER (i32)
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code);
        getl(r as u8, &mut code);
        code.extend([0x37, 0x03, 0x00]); // i64.store
    }
    getl(ITER, &mut code);
    code.push(0xa7); // i32.wrap_i64 → the function result
    code.push(0x0b); // end function
    // module: () -> i32
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    section(1, vec![0x01, 0x60, 0x00, 0x01, 0x7f], &mut m);
    section(3, vec![0x01, 0x00], &mut m);
    section(5, vec![0x01, 0x00, 0x04], &mut m);
    let mut exp = vec![0x02];
    exp.extend([0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00]); // "mem"
    exp.extend([0x03, 0x72, 0x75, 0x6e, 0x00, 0x00]); // "run"
    section(7, exp, &mut m);
    let mut cs = Vec::new();
    uleb(1, &mut cs);
    uleb(code.len() as u64, &mut cs);
    cs.extend(code);
    section(10, cs, &mut m);
    m
}

/// CHAINING PROOF (S1) — the REAL x86 loop: a self-loop whose body maintains rflags (via
/// [`emit_flags`]) and whose back-branch is a true condition code (via [`emit_cc`]), e.g.
/// `...; sub rcx, rdx; jnz top`. Composes the verified `emit_cc` + flag codegen into one in-wasm
/// loop, regs+rflags persisting across iterations (no per-iteration marshalling), bounded by
/// `max_iters` (interrupt-yield). Body ops: `Movi/Movr/Bin/Shift`; `Bin` maintains flags. The
/// generalization of this — multiple distinct blocks linked by branches — is the multi-block
/// dispatcher (the remaining S1 work).
pub(crate) fn compile_jcc_loop(body: &[Op], cc: u8, max_iters: u64) -> Vec<u8> {
    const ITER: u8 = NREG as u8 + 5; // i64 local 21, after FR_LOCAL(20)
    let mut code = Vec::new();
    uleb(1, &mut code);
    uleb(u64::from(ITER) + 1, &mut code); // 22 × i64 (regs + VA + RFLAGS + FA/FB/FR + ITER)
    code.push(0x7e);
    let getl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x20);
        uleb(u64::from(r), c);
    };
    let setl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x21);
        uleb(u64::from(r), c);
    };
    // entry: regs → locals, rflags → $rflags, ITER = 0
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code);
        code.extend([0x29, 0x03, 0x00]);
        setl(r as u8, &mut code);
    }
    code.push(0x41);
    sleb(RFLAGS_MEM as i64, &mut code);
    code.extend([0x29, 0x03, 0x00]);
    setl(RFLAGS_LOCAL, &mut code);
    code.push(0x42);
    sleb(0, &mut code);
    setl(ITER, &mut code);
    code.push(0x02);
    code.push(0x40); // block $exit
    code.push(0x03);
    code.push(0x40); // loop $top
    for op in body {
        match *op {
            Op::Movi { d, imm } => {
                code.push(0x42);
                sleb(imm as i64, &mut code);
                setl(d, &mut code);
            }
            Op::Movr { d, s } => {
                getl(s, &mut code);
                setl(d, &mut code);
            }
            Op::Bin { op, d, a, b } => {
                let opcode = match op {
                    Bin::Add => 0x7c,
                    Bin::Sub => 0x7d,
                    Bin::Xor => 0x85,
                    Bin::And => 0x83,
                    Bin::Or => 0x84,
                };
                getl(a, &mut code);
                setl(FA_LOCAL, &mut code);
                getl(b, &mut code);
                setl(FB_LOCAL, &mut code);
                getl(FA_LOCAL, &mut code);
                getl(FB_LOCAL, &mut code);
                code.push(opcode);
                setl(FR_LOCAL, &mut code);
                getl(FR_LOCAL, &mut code);
                setl(d, &mut code);
                emit_flags(op, &mut code);
            }
            Op::Shift { op, d, a, sh } => {
                getl(a, &mut code);
                code.push(0x42);
                sleb(i64::from(sh), &mut code);
                code.push(match op {
                    Sh::ShrU => 0x88,
                    Sh::Shl => 0x86,
                    Sh::Rotr => 0x8a,
                });
                setl(d, &mut code);
            }
            _ => panic!("compile_jcc_loop body supports reg-dataflow ops only"),
        }
    }
    // ITER += 1 ; if ITER >= max → br $exit
    getl(ITER, &mut code);
    code.push(0x42);
    sleb(1, &mut code);
    code.push(0x7c);
    setl(ITER, &mut code);
    getl(ITER, &mut code);
    code.push(0x42);
    sleb(max_iters as i64, &mut code);
    code.push(0x5a); // i64.ge_u
    code.extend([0x0d, 0x01]); // br_if $exit
    // if cc taken → br $top (continue)
    emit_cc(cc, &mut code);
    code.extend([0x0d, 0x00]); // br_if $top
    code.push(0x0b); // end loop
    code.push(0x0b); // end block
    // exit: regs + rflags → memory, return ITER
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code);
        getl(r as u8, &mut code);
        code.extend([0x37, 0x03, 0x00]);
    }
    code.push(0x41);
    sleb(RFLAGS_MEM as i64, &mut code);
    getl(RFLAGS_LOCAL, &mut code);
    code.extend([0x37, 0x03, 0x00]);
    getl(ITER, &mut code);
    code.push(0xa7); // i32.wrap_i64
    code.push(0x0b);
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    section(1, vec![0x01, 0x60, 0x00, 0x01, 0x7f], &mut m);
    section(3, vec![0x01, 0x00], &mut m);
    section(5, vec![0x01, 0x00, 0x04], &mut m);
    let mut exp = vec![0x02];
    exp.extend([0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00]);
    exp.extend([0x03, 0x72, 0x75, 0x6e, 0x00, 0x00]);
    section(7, exp, &mut m);
    let mut cs = Vec::new();
    uleb(1, &mut cs);
    uleb(code.len() as u64, &mut cs);
    cs.extend(code);
    section(10, cs, &mut m);
    m
}

/// CHAINING (S1) — compile a discovered [`Region`] of linked basic blocks into ONE wasm unit with a
/// `br_table` dispatcher: regs+rflags persist in i64 locals across blocks, `$pc` (an i32 block
/// INDEX) selects the current block, branches resolve in wasm, and control leaves only at a region
/// exit (out-of-region/indirect target, `Exit` terminator, or the `max_iters` interrupt-yield
/// bound). Returns the guest rip to resume at (i64). Handles reg/flag/lea + memory ops through the
/// **inline software-TLB** (live paging): a miss exits the region so the interpreter faults the page
/// in and the dispatcher re-enters. The caller fills the TLB image (`TLB_BASE`) from its mappings.
pub(crate) fn compile_region(region: &Region, max_iters: u64) -> Vec<u8> {
    const ITER: u8 = NREG as u8 + 5; // i64 21
    const EXIT_RIP: u8 = NREG as u8 + 6; // i64 22
    const PC: u8 = NREG as u8 + 7; // i32 23
    const TE: u8 = NREG as u8 + 8; // i32 24 (TLB entry scratch; $va reuses VA_LOCAL=16)
    let n = region.blocks.len();
    let getl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x20);
        uleb(u64::from(r), c);
    };
    let setl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x21);
        uleb(u64::from(r), c);
    };
    // Inline software-TLB address translation (live paging): push the i32 wasm host offset for the
    // guest vaddr `disp + base + idx<<scale`. On a TLB MISS, set $exit_rip = `blk_start` and br to
    // $exit (depth `exit_d + 1` — the +1 is the tag-check `if`) so the interpreter re-runs the block,
    // faults the page in, and the dispatcher re-enters. Mirrors `compile_mode::emit_addr` exactly,
    // only the miss target differs (EXIT_RIP + per-block depth instead of BAIL + fixed br 1).
    let emit_addr_tlb = |base: u8, idx: u8, scale: u8, disp: i32, blk_start: u64, exit_d: u64, c: &mut Vec<u8>| {
        c.push(0x42);
        sleb(i64::from(disp), c);
        if base != NO_REG {
            c.push(0x20);
            uleb(u64::from(base), c);
            c.push(0x7c);
        }
        if idx != NO_REG {
            c.push(0x20);
            uleb(u64::from(idx), c);
            c.push(0x42);
            sleb(i64::from(scale), c);
            c.push(0x86);
            c.push(0x7c);
        }
        c.push(0x21);
        uleb(u64::from(VA_LOCAL), c); // $va = vaddr
        // $te = TLB_BASE + (((va>>12) & (TLB_SIZE-1)) * 16)
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c);
        c.push(0x42);
        sleb(12, c);
        c.push(0x88);
        c.push(0x42);
        sleb((TLB_SIZE - 1) as i64, c);
        c.push(0x83);
        c.push(0x42);
        sleb(16, c);
        c.push(0x7e);
        c.push(0x42);
        sleb(TLB_BASE as i64, c);
        c.push(0x7c);
        c.push(0xa7);
        c.push(0x21);
        uleb(u64::from(TE), c);
        // tag check: if [$te].tag != (va>>12) → miss → exit
        c.push(0x20);
        uleb(u64::from(TE), c);
        c.extend([0x29, 0x03, 0x00]);
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c);
        c.push(0x42);
        sleb(12, c);
        c.push(0x88);
        c.push(0x52); // i64.ne
        c.extend([0x04, 0x40]); // if (miss)
        // VADDR_MEM = $va ; MISS_MEM = 1  (so the pooled executor fetches this page + retries)
        c.push(0x41);
        sleb(VADDR_MEM as i64, c);
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c);
        c.extend([0x37, 0x03, 0x00]); // i64.store $va
        c.push(0x41);
        sleb(MISS_MEM as i64, c);
        c.push(0x42);
        sleb(1, c);
        c.extend([0x37, 0x03, 0x00]); // i64.store 1
        c.push(0x42);
        sleb(blk_start as i64, c);
        c.push(0x21);
        uleb(u64::from(EXIT_RIP), c); // $exit_rip = block start
        c.push(0x0c);
        uleb(exit_d + 1, c); // br $exit (+1 for this if)
        c.push(0x0b); // end if
        // hit: host = [$te+8] + GUEST_BASE + (va & 0xfff)
        c.push(0x20);
        uleb(u64::from(TE), c);
        c.extend([0x29, 0x03, 0x08]);
        c.push(0x42);
        sleb(GUEST_BASE as i64, c);
        c.push(0x7c);
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c);
        c.push(0x42);
        sleb(0xfff, c);
        c.push(0x83);
        c.push(0x7c);
        c.push(0xa7); // i32.wrap_i64
    };
    // "go to" a guest target: in-region → set $pc + br $loop ; else → set $exit_rip + br $exit.
    let goto = |target: u64, loop_d: u64, exit_d: u64, c: &mut Vec<u8>| {
        match region.index_of(target) {
            Some(j) => {
                c.push(0x41);
                sleb(j as i64, c); // i32.const j
                setl(PC, c);
                c.push(0x0c);
                uleb(loop_d, c); // br $loop
            }
            None => {
                c.push(0x42);
                sleb(target as i64, c); // i64.const target
                setl(EXIT_RIP, c);
                c.push(0x0c);
                uleb(exit_d, c); // br $exit
            }
        }
    };
    let mut code = Vec::new();
    uleb(2, &mut code); // 2 local groups
    uleb(u64::from(EXIT_RIP) + 1, &mut code); // 23 × i64
    code.push(0x7e);
    uleb(2, &mut code);
    code.push(0x7f); // 2 × i32 ($pc, $te)
    // entry marshalling
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code);
        code.extend([0x29, 0x03, 0x00]);
        setl(r as u8, &mut code);
    }
    code.push(0x41);
    sleb(RFLAGS_MEM as i64, &mut code);
    code.extend([0x29, 0x03, 0x00]);
    setl(RFLAGS_LOCAL, &mut code);
    code.push(0x42);
    sleb(0, &mut code);
    setl(ITER, &mut code); // ITER = 0
    code.push(0x41);
    sleb(0, &mut code);
    setl(PC, &mut code); // $pc = 0 (blocks[0] = entry)
    code.push(0x41);
    sleb(MISS_MEM as i64, &mut code);
    code.push(0x42);
    sleb(0, &mut code);
    code.extend([0x37, 0x03, 0x00]); // MISS_MEM = 0 (no TLB miss yet)
    // dispatcher: block $exit { loop $loop { block $Bdef { block $B_{n-1} … block $B0 { br_table } …
    code.extend([0x02, 0x40]); // block $exit
    code.extend([0x03, 0x40]); // loop $loop
    for _ in 0..=n {
        code.extend([0x02, 0x40]); // $Bdef then $B_{n-1} … $B0  (n+1 blocks)
    }
    // br_table inside $B0: pc i → $Bi (depth i), default → $Bdef (depth n)
    getl(PC, &mut code);
    code.push(0x0e);
    uleb(n as u64, &mut code); // count
    for i in 0..n {
        uleb(i as u64, &mut code);
    }
    uleb(n as u64, &mut code); // default → $Bdef
    code.push(0x0b); // end $B0
    // block bodies: for k in 0..n, then the default after $Bdef
    for (k, blk) in region.blocks.iter().enumerate() {
        let loop_d = (n - k) as u64; // br $loop depth from block k's body
        let exit_d = (n - k + 1) as u64; // br $exit depth
        // interrupt-yield bound: ITER++ ; if ITER >= max → resume at this block, exit
        getl(ITER, &mut code);
        code.push(0x42);
        sleb(1, &mut code);
        code.push(0x7c);
        setl(ITER, &mut code);
        getl(ITER, &mut code);
        code.push(0x42);
        sleb(max_iters as i64, &mut code);
        code.push(0x5a); // i64.ge_u
        code.extend([0x04, 0x40]); // if (adds +1 depth inside)
        code.push(0x42);
        sleb(blk.start as i64, &mut code);
        setl(EXIT_RIP, &mut code);
        code.push(0x0c);
        uleb(exit_d + 1, &mut code); // br $exit (+1 inside the if)
        code.push(0x0b); // end if
        // block ops (reg/flag dataflow)
        for op in &blk.ops {
            match *op {
                Op::Movi { d, imm } => {
                    code.push(0x42);
                    sleb(imm as i64, &mut code);
                    setl(d, &mut code);
                }
                Op::Movr { d, s } => {
                    getl(s, &mut code);
                    setl(d, &mut code);
                }
                Op::Bin { op, d, a, b } => {
                    let opcode = match op {
                        Bin::Add => 0x7c,
                        Bin::Sub => 0x7d,
                        Bin::Xor => 0x85,
                        Bin::And => 0x83,
                        Bin::Or => 0x84,
                    };
                    getl(a, &mut code);
                    setl(FA_LOCAL, &mut code);
                    getl(b, &mut code);
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opcode);
                    setl(FR_LOCAL, &mut code);
                    getl(FR_LOCAL, &mut code);
                    setl(d, &mut code);
                    emit_flags(op, &mut code);
                }
                Op::Shift { op, d, a, sh } => {
                    getl(a, &mut code);
                    code.push(0x42);
                    sleb(i64::from(sh), &mut code);
                    code.push(match op {
                        Sh::ShrU => 0x88,
                        Sh::Shl => 0x86,
                        Sh::Rotr => 0x8a,
                    });
                    setl(d, &mut code);
                }
                Op::Lea { d, base, idx, scale, disp } => {
                    code.push(0x42);
                    sleb(i64::from(disp), &mut code);
                    if base != NO_REG {
                        getl(base, &mut code);
                        code.push(0x7c);
                    }
                    if idx != NO_REG {
                        getl(idx, &mut code);
                        code.push(0x42);
                        sleb(i64::from(scale), &mut code);
                        code.push(0x86);
                        code.push(0x7c);
                    }
                    setl(d, &mut code);
                }
                // cmp/test: rflags only, no register (a region maintains rflags throughout).
                Op::Cmp { a, b } | Op::Test { a, b } => {
                    let (alu, opc) = match *op {
                        Op::Cmp { .. } => (Bin::Sub, 0x7du8),
                        _ => (Bin::And, 0x83u8),
                    };
                    getl(a, &mut code);
                    setl(FA_LOCAL, &mut code);
                    getl(b, &mut code);
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opc);
                    setl(FR_LOCAL, &mut code);
                    emit_flags(alu, &mut code);
                }
                // ALU with an immediate: `d = d op imm`, maintaining rflags.
                Op::BinImm { op, d, imm } => {
                    let opcode = match op {
                        Bin::Add => 0x7c,
                        Bin::Sub => 0x7d,
                        Bin::Xor => 0x85,
                        Bin::And => 0x83,
                        Bin::Or => 0x84,
                    };
                    getl(d, &mut code);
                    setl(FA_LOCAL, &mut code);
                    code.push(0x42);
                    sleb(imm as i64, &mut code); // i64.const imm
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opcode);
                    setl(FR_LOCAL, &mut code);
                    getl(FR_LOCAL, &mut code);
                    setl(d, &mut code);
                    emit_flags(op, &mut code);
                }
                // cmp r, imm: rflags only (flags of `a - imm`), no register.
                Op::CmpImm { a, imm } => {
                    getl(a, &mut code);
                    setl(FA_LOCAL, &mut code);
                    code.push(0x42);
                    sleb(imm as i64, &mut code); // i64.const imm
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(0x7d); // i64.sub
                    setl(FR_LOCAL, &mut code);
                    emit_flags(Bin::Sub, &mut code);
                }
                // push: [rsp-8] = s (pre-decrement value) through the inline TLB (a stack-page miss
                // exits the region, rsp untouched), then rsp -= 8. No flags.
                Op::Push { s } => {
                    emit_addr_tlb(RSP, NO_REG, 0, -8, blk.start, exit_d, &mut code);
                    getl(s, &mut code);
                    code.extend([0x37, 0x03, 0x00]); // i64.store
                    getl(RSP, &mut code);
                    code.push(0x42);
                    sleb(-8, &mut code);
                    code.push(0x7c);
                    setl(RSP, &mut code);
                }
                // pop: load [rsp] (miss → region exit), rsp += 8, then write d. `$fr` is free scratch.
                Op::Pop { d } => {
                    emit_addr_tlb(RSP, NO_REG, 0, 0, blk.start, exit_d, &mut code);
                    code.extend([0x29, 0x03, 0x00]); // i64.load
                    setl(FR_LOCAL, &mut code);
                    getl(RSP, &mut code);
                    code.push(0x42);
                    sleb(8, &mut code);
                    code.push(0x7c);
                    setl(RSP, &mut code);
                    getl(FR_LOCAL, &mut code);
                    setl(d, &mut code);
                }
                // Inline-TLB memory ops: a miss exits the region (EXIT_RIP=block start) so the
                // interpreter faults the page in and the dispatcher re-enters. (Live paging path.)
                Op::Load { d, base, idx, scale, disp } => {
                    emit_addr_tlb(base, idx, scale, disp, blk.start, exit_d, &mut code);
                    code.extend([0x29, 0x03, 0x00]); // i64.load
                    setl(d, &mut code);
                }
                Op::Store { base, idx, scale, disp, s } => {
                    emit_addr_tlb(base, idx, scale, disp, blk.start, exit_d, &mut code);
                    getl(s, &mut code);
                    code.extend([0x37, 0x03, 0x00]); // i64.store
                }
                Op::LoadOp { op, d, base, idx, scale, disp } => {
                    getl(d, &mut code);
                    emit_addr_tlb(base, idx, scale, disp, blk.start, exit_d, &mut code);
                    code.extend([0x29, 0x03, 0x00]); // i64.load
                    code.push(match op {
                        Bin::Add => 0x7c,
                        Bin::Sub => 0x7d,
                        Bin::Xor => 0x85,
                        Bin::And => 0x83,
                        Bin::Or => 0x84,
                    });
                    setl(d, &mut code);
                }
            }
        }
        // terminator
        match blk.term {
            Terminator::Jmp { target } => goto(target, loop_d, exit_d, &mut code),
            Terminator::Jcc { cc, taken, fall } => {
                emit_cc(cc, &mut code);
                code.extend([0x04, 0x40]); // if (taken)  → +1 depth inside
                goto(taken, loop_d + 1, exit_d + 1, &mut code);
                code.push(0x05); // else (fall)
                goto(fall, loop_d + 1, exit_d + 1, &mut code);
                code.push(0x0b); // end if
            }
            Terminator::Exit => {
                code.push(0x42);
                sleb((blk.start + blk.len as u64) as i64, &mut code);
                setl(EXIT_RIP, &mut code);
                code.push(0x0c);
                uleb(exit_d, &mut code); // br $exit
            }
        }
        code.push(0x0b); // end $B_{k+1} (closing the next outer block → start of block k+1's body)
    }
    // default (after end $Bdef): unreachable in practice ($pc is always valid) — exit safely.
    code.push(0x42);
    sleb(region.entry as i64, &mut code);
    setl(EXIT_RIP, &mut code);
    code.extend([0x0c, 0x01]); // br $exit (depth 1: containing $loop(0), $exit(1))
    code.push(0x0b); // end $loop
    code.push(0x0b); // end $exit
    // exit marshalling: regs+rflags → memory, return $exit_rip
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code);
        getl(r as u8, &mut code);
        code.extend([0x37, 0x03, 0x00]);
    }
    code.push(0x41);
    sleb(RFLAGS_MEM as i64, &mut code);
    getl(RFLAGS_LOCAL, &mut code);
    code.extend([0x37, 0x03, 0x00]);
    // Write the exit rip to memory (not an i64 return) — browser-robust.
    code.push(0x41);
    sleb(EXIT_RIP_MEM as i64, &mut code);
    getl(EXIT_RIP, &mut code);
    code.extend([0x37, 0x03, 0x00]); // i64.store mem[EXIT_RIP_MEM] = $exit_rip
    code.push(0x0b); // end function
    // module: () -> ()  (the exit rip is in mem[EXIT_RIP_MEM])
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    section(1, vec![0x01, 0x60, 0x00, 0x00], &mut m); // () -> ()
    section(3, vec![0x01, 0x00], &mut m);
    section(5, vec![0x01, 0x00, 0x04], &mut m);
    let mut exp = vec![0x02];
    exp.extend([0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00]);
    exp.extend([0x03, 0x72, 0x75, 0x6e, 0x00, 0x00]);
    section(7, exp, &mut m);
    let mut cs = Vec::new();
    uleb(1, &mut cs);
    uleb(code.len() as u64, &mut cs);
    cs.extend(code);
    section(10, cs, &mut m);
    m
}

/// x86 condition-code predicate (`jcc`/`setcc`/`cmovcc`), mirroring the interpreter's `Cpu::cond`
/// EXACTLY: `cc` is the opcode low nibble (0x0..0xf). Reads the modelled flags from `rflags`
/// (CF=bit0, PF=2, ZF=6, SF=7, OF=11 — the `ALU_FLAGS_MASK` layout). The oracle the in-wasm `jcc`
/// codegen ([`emit_cc`]) is validated against. **Keep in sync with `Cpu::cond`.**
pub(crate) fn cc_taken(cc: u8, rflags: u64) -> bool {
    let bit = |n: u32| rflags & (1u64 << n) != 0;
    let (cf, pf, zf, sf, of) = (bit(0), bit(2), bit(6), bit(7), bit(11));
    let base = match (cc >> 1) & 7 {
        0 => of,
        1 => cf,
        2 => zf,
        3 => cf || zf,
        4 => sf,
        5 => pf,
        6 => sf != of,
        _ => (sf != of) || zf,
    };
    if cc & 1 == 1 {
        !base
    } else {
        base
    }
}

/// Emit wasm pushing an i32 (1 = taken, 0 = not) for condition code `cc`, computed from the live
/// rflags in `$rflags` (RFLAGS_LOCAL). The one genuinely new codegen surface for in-wasm `jcc` —
/// validated bit-for-bit against [`cc_taken`] by `jit_condition_codes_match_the_interpreter`.
fn emit_cc(cc: u8, c: &mut Vec<u8>) {
    // bit(n) = ($rflags >> n) & 1  → i64 0/1
    let bit = |n: i64, c: &mut Vec<u8>| {
        c.push(0x20);
        uleb(u64::from(RFLAGS_LOCAL), c); // local.get $rflags
        c.push(0x42);
        sleb(n, c); // i64.const n
        c.push(0x88); // i64.shr_u
        c.push(0x42);
        sleb(1, c); // i64.const 1
        c.push(0x83); // i64.and
    };
    // base (i64 0/1) per cc>>1
    match (cc >> 1) & 7 {
        0 => bit(11, c),                                          // OF
        1 => bit(0, c),                                           // CF
        2 => bit(6, c),                                           // ZF
        3 => { bit(0, c); bit(6, c); c.push(0x84); }              // CF | ZF
        4 => bit(7, c),                                           // SF
        5 => bit(2, c),                                           // PF
        6 => { bit(7, c); bit(11, c); c.push(0x85); }             // SF ^ OF
        _ => { bit(7, c); bit(11, c); c.push(0x85); bit(6, c); c.push(0x84); } // (SF^OF) | ZF
    }
    // apply the cc&1 negation, leaving an i32 0/1
    if cc & 1 == 1 {
        c.push(0x50); // i64.eqz → i32 (1 if base==0)
    } else {
        c.push(0x42);
        sleb(0, c);
        c.push(0x52); // i64.const 0 ; i64.ne → i32 (1 if base!=0)
    }
}

/// Register-allocated codegen: load all 16 regs from memory into i64 locals at entry,
/// compute on locals, store back at exit (per-op traffic is `local.get/set`, not memory).
/// `tlb` selects the address path (direct vs inline software-TLB translation); `flags`
/// additionally tracks `rflags` (mem slot `RFLAGS_MEM`) across the ALU ops.
fn compile_mode(block: &[Op], tlb: bool, flags: bool) -> Vec<u8> {
    let mut code = Vec::new();
    // locals: 21 × i64 (16 regs + vaddr + rflags + 3 flag scratch) then 2 × i32
    // (bail-index + TLB-entry scratch). Always declared; unused ones are harmless.
    uleb(2, &mut code);
    uleb(NREG as u64 + 5, &mut code);
    code.push(0x7e); // i64 × 21
    uleb(2, &mut code);
    code.push(0x7f); // i32 × 2
    let getl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x20);
        uleb(u64::from(r), c);
    }; // local.get
    let setl = |r: u8, c: &mut Vec<u8>| {
        c.push(0x21);
        uleb(u64::from(r), c);
    }; // local.set
    let load_mem = |r: usize, c: &mut Vec<u8>| {
        c.push(0x41);
        sleb((r * 8) as i64, c); // i32.const addr
        c.extend([0x29, 0x03, 0x00]); // i64.load align=3 off=0
    };
    // emit the i32 wasm offset for an effective address `base + idx<<scale + disp`.
    // Direct: + GUEST_BASE, wrap. TLB: translate the vaddr through the software TLB; on a
    // tag miss, set bail-index = `k` and `br $exit` (depth 1 — out of the `if`, to the
    // body block). Bail happens BEFORE any reg/mem write for op `k`, so state stays clean.
    let emit_addr = |base: u8, idx: u8, scale: u8, disp: i32, k: usize, c: &mut Vec<u8>| {
        // vaddr = disp + base + idx<<scale  (i64)
        c.push(0x42);
        sleb(i64::from(disp), c); // i64.const disp
        if base != NO_REG {
            c.push(0x20);
            uleb(u64::from(base), c); // local.get base
            c.push(0x7c); // i64.add
        }
        if idx != NO_REG {
            c.push(0x20);
            uleb(u64::from(idx), c); // local.get idx
            c.push(0x42);
            sleb(i64::from(scale), c); // i64.const scale
            c.push(0x86); // i64.shl
            c.push(0x7c); // i64.add
        }
        if !tlb {
            c.push(0x42);
            sleb(GUEST_BASE as i64, c); // + GUEST_BASE
            c.push(0x7c); // i64.add
            c.push(0xa7); // i32.wrap_i64
            return;
        }
        // vaddr → $va scratch
        c.push(0x21);
        uleb(u64::from(VA_LOCAL), c); // local.set $va
        // te = TLB_BASE + ((($va >> 12) & (TLB_SIZE-1)) * 16)  → i32, kept in $te
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c); // local.get $va
        c.push(0x42);
        sleb(12, c);
        c.push(0x88); // i64.shr_u  → vpage
        c.push(0x42);
        sleb((TLB_SIZE - 1) as i64, c);
        c.push(0x83); // i64.and  → slot
        c.push(0x42);
        sleb(16, c);
        c.push(0x7e); // i64.mul  → byte offset
        c.push(0x42);
        sleb(TLB_BASE as i64, c);
        c.push(0x7c); // i64.add  → entry addr
        c.push(0xa7); // i32.wrap_i64
        c.push(0x21);
        uleb(u64::from(TE_LOCAL), c); // local.set $te
        // tag check: if [$te].tag != vpage → bail
        c.push(0x20);
        uleb(u64::from(TE_LOCAL), c); // local.get $te
        c.extend([0x29, 0x03, 0x00]); // i64.load [te]  → tag
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c); // local.get $va
        c.push(0x42);
        sleb(12, c);
        c.push(0x88); // i64.shr_u → vpage
        c.push(0x52); // i64.ne
        c.push(0x04);
        c.push(0x40); // if (void blocktype)
        c.push(0x41);
        sleb(k as i64, c); // i32.const k
        c.push(0x21);
        uleb(u64::from(BAIL_LOCAL), c); // local.set $bail
        c.extend([0x0c, 0x01]); // br 1  → $exit (out of if, out of body block)
        c.push(0x0b); // end if
        // hit: host = [$te+8].host_off + GUEST_BASE + ($va & 0xfff)
        c.push(0x20);
        uleb(u64::from(TE_LOCAL), c); // local.get $te
        c.extend([0x29, 0x03, 0x08]); // i64.load offset=8 → host_off
        c.push(0x42);
        sleb(GUEST_BASE as i64, c);
        c.push(0x7c); // i64.add
        c.push(0x20);
        uleb(u64::from(VA_LOCAL), c); // local.get $va
        c.push(0x42);
        sleb(0xfff, c);
        c.push(0x83); // i64.and
        c.push(0x7c); // i64.add
        c.push(0xa7); // i32.wrap_i64
    };
    // emit the x86 rflags update for an ALU op, reading operands a/b in $fa/$fb and the
    // result in $fr (set by the caller) and folding CF/PF/AF/ZF/SF/OF into $rflags. Mirrors
    // the `interpret`-side `x86_flags` oracle exactly. Logical ops clear CF/OF/AF.
    // entry: regs → locals (and the live rflags, in flags mode)
    for r in 0..NREG {
        load_mem(r, &mut code);
        setl(r as u8, &mut code);
    }
    if flags {
        code.push(0x41);
        sleb(RFLAGS_MEM as i64, &mut code); // i32.const RFLAGS_MEM
        code.extend([0x29, 0x03, 0x00]); // i64.load
        code.push(0x21);
        uleb(u64::from(RFLAGS_LOCAL), &mut code); // local.set $rflags
    }
    if tlb {
        // bail-index defaults to "completed" (= block length); body wrapped in a block $exit
        code.push(0x41);
        sleb(block.len() as i64, &mut code); // i32.const len
        code.push(0x21);
        uleb(u64::from(BAIL_LOCAL), &mut code); // local.set $bail
        code.push(0x02);
        code.push(0x40); // block (void) $exit
    }
    // body
    for (k, op) in block.iter().enumerate() {
        match *op {
            Op::Movi { d, imm } => {
                code.push(0x42);
                sleb(imm as i64, &mut code); // i64.const
                setl(d, &mut code);
            }
            Op::Movr { d, s } => {
                getl(s, &mut code);
                setl(d, &mut code);
            }
            Op::Bin { op, d, a, b } => {
                let opcode = match op {
                    Bin::Add => 0x7c,
                    Bin::Sub => 0x7d,
                    Bin::Xor => 0x85,
                    Bin::And => 0x83,
                    Bin::Or => 0x84,
                };
                if flags {
                    // capture a, b, result in scratch (a/b may alias d) so the flag math is
                    // correct, then write d and fold the flags into $rflags.
                    getl(a, &mut code);
                    setl(FA_LOCAL, &mut code);
                    getl(b, &mut code);
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opcode);
                    setl(FR_LOCAL, &mut code);
                    getl(FR_LOCAL, &mut code);
                    setl(d, &mut code);
                    emit_flags(op, &mut code);
                } else {
                    getl(a, &mut code);
                    getl(b, &mut code);
                    code.push(opcode);
                    setl(d, &mut code);
                }
            }
            Op::Shift { op, d, a, sh } => {
                getl(a, &mut code);
                code.push(0x42);
                sleb(i64::from(sh), &mut code); // i64.const shift
                code.push(match op {
                    Sh::ShrU => 0x88,
                    Sh::Shl => 0x86,
                    Sh::Rotr => 0x8a,
                });
                setl(d, &mut code);
            }
            // `lea`: compute the effective VIRTUAL address `disp + base + idx<<scale` (i64) and
            // store it to `d` — no memory access, no flags, no TLB (so it never bails). Mirrors the
            // vaddr math in `emit_addr` and the `eff_addr` interpreter oracle exactly.
            Op::Lea { d, base, idx, scale, disp } => {
                code.push(0x42);
                sleb(i64::from(disp), &mut code); // i64.const disp
                if base != NO_REG {
                    getl(base, &mut code);
                    code.push(0x7c); // i64.add
                }
                if idx != NO_REG {
                    getl(idx, &mut code);
                    code.push(0x42);
                    sleb(i64::from(scale), &mut code); // i64.const scale
                    code.push(0x86); // i64.shl
                    code.push(0x7c); // i64.add
                }
                setl(d, &mut code);
            }
            // cmp/test: set rflags only (no register). In flag-less mode they're inert (no effect).
            Op::Cmp { a, b } | Op::Test { a, b } => {
                if flags {
                    let (alu, opc) = match *op {
                        Op::Cmp { .. } => (Bin::Sub, 0x7du8),
                        _ => (Bin::And, 0x83u8),
                    };
                    getl(a, &mut code);
                    setl(FA_LOCAL, &mut code);
                    getl(b, &mut code);
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opc);
                    setl(FR_LOCAL, &mut code);
                    emit_flags(alu, &mut code);
                }
            }
            // ALU with an immediate: `d = d op imm` (+ rflags in flags mode). Mirrors `Bin` with the
            // second operand an `i64.const` instead of a register.
            Op::BinImm { op, d, imm } => {
                let opcode = match op {
                    Bin::Add => 0x7c,
                    Bin::Sub => 0x7d,
                    Bin::Xor => 0x85,
                    Bin::And => 0x83,
                    Bin::Or => 0x84,
                };
                if flags {
                    getl(d, &mut code);
                    setl(FA_LOCAL, &mut code);
                    code.push(0x42);
                    sleb(imm as i64, &mut code); // i64.const imm
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opcode);
                    setl(FR_LOCAL, &mut code);
                    getl(FR_LOCAL, &mut code);
                    setl(d, &mut code);
                    emit_flags(op, &mut code);
                } else {
                    getl(d, &mut code);
                    code.push(0x42);
                    sleb(imm as i64, &mut code); // i64.const imm
                    code.push(opcode);
                    setl(d, &mut code);
                }
            }
            // cmp r, imm: rflags only (flags of `a - imm`), no register. Inert in flag-less mode.
            Op::CmpImm { a, imm } => {
                if flags {
                    getl(a, &mut code);
                    setl(FA_LOCAL, &mut code);
                    code.push(0x42);
                    sleb(imm as i64, &mut code); // i64.const imm
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(0x7d); // i64.sub
                    setl(FR_LOCAL, &mut code);
                    emit_flags(Bin::Sub, &mut code);
                }
            }
            // push: store the (pre-decrement) value to [rsp-8] through the TLB (may bail BEFORE any
            // write — rsp untouched), then rsp -= 8. No flags.
            Op::Push { s } => {
                emit_addr(RSP, NO_REG, 0, -8, k, &mut code); // host addr of [rsp-8]
                getl(s, &mut code); // value (rsp still the OLD value here → push rsp is correct)
                code.extend([0x37, 0x03, 0x00]); // i64.store
                getl(RSP, &mut code);
                code.push(0x42);
                sleb(-8, &mut code);
                code.push(0x7c); // i64.add (rsp - 8)
                setl(RSP, &mut code);
            }
            // pop: load [rsp] (may bail BEFORE any write), rsp += 8, THEN write d (so pop rsp ends
            // with rsp = [old rsp]). `$fr` is a free scratch (push/pop touch no flags). No flags.
            Op::Pop { d } => {
                emit_addr(RSP, NO_REG, 0, 0, k, &mut code); // host addr of [rsp]
                code.extend([0x29, 0x03, 0x00]); // i64.load
                setl(FR_LOCAL, &mut code); // temp = [rsp]
                getl(RSP, &mut code);
                code.push(0x42);
                sleb(8, &mut code);
                code.push(0x7c); // i64.add (rsp + 8)
                setl(RSP, &mut code);
                getl(FR_LOCAL, &mut code);
                setl(d, &mut code); // d = temp
            }
            // memory ops: emit_addr leaves the i32 wasm offset on the stack (and on the
            // TLB path may bail to $exit before this op writes anything)
            Op::Load { d, base, idx, scale, disp } => {
                emit_addr(base, idx, scale, disp, k, &mut code);
                code.extend([0x29, 0x03, 0x00]); // i64.load align=3
                setl(d, &mut code);
            }
            Op::Store { base, idx, scale, disp, s } => {
                emit_addr(base, idx, scale, disp, k, &mut code); // addr
                getl(s, &mut code); // value
                code.extend([0x37, 0x03, 0x00]); // i64.store align=3
            }
            Op::LoadOp { op, d, base, idx, scale, disp } => {
                let opcode = match op {
                    Bin::Add => 0x7c,
                    Bin::Sub => 0x7d,
                    Bin::Xor => 0x85,
                    Bin::And => 0x83,
                    Bin::Or => 0x84,
                };
                if flags {
                    // FA = d_old, FB = [mem] (emit_addr may bail before either is written),
                    // FR = result; then d = result and fold the flags.
                    getl(d, &mut code);
                    setl(FA_LOCAL, &mut code);
                    emit_addr(base, idx, scale, disp, k, &mut code);
                    code.extend([0x29, 0x03, 0x00]); // i64.load
                    setl(FB_LOCAL, &mut code);
                    getl(FA_LOCAL, &mut code);
                    getl(FB_LOCAL, &mut code);
                    code.push(opcode);
                    setl(FR_LOCAL, &mut code);
                    getl(FR_LOCAL, &mut code);
                    setl(d, &mut code);
                    emit_flags(op, &mut code);
                } else {
                    getl(d, &mut code); // current d on stack
                    emit_addr(base, idx, scale, disp, k, &mut code);
                    code.extend([0x29, 0x03, 0x00]); // i64.load → mem value
                    code.push(opcode);
                    setl(d, &mut code);
                }
            }
        }
    }
    if tlb {
        code.push(0x0b); // end block $exit — bail and fall-through converge here
    }
    // exit: locals → regs (store back architectural state; at a bail it is exactly the
    // state before the faulting op, so the interpreter can re-execute from there)
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code); // i32.const addr
        getl(r as u8, &mut code);
        code.extend([0x37, 0x03, 0x00]); // i64.store
    }
    if flags {
        code.push(0x41);
        sleb(RFLAGS_MEM as i64, &mut code); // i32.const RFLAGS_MEM
        code.push(0x20);
        uleb(u64::from(RFLAGS_LOCAL), &mut code); // local.get $rflags
        code.extend([0x37, 0x03, 0x00]); // i64.store
    }
    if tlb {
        code.push(0x20);
        uleb(u64::from(BAIL_LOCAL), &mut code); // local.get $bail → i32 result
    }
    code.push(0x0b); // end function

    // assemble the module; TLB blocks return i32 (the bail index / block length)
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    let functype = if tlb {
        vec![0x01, 0x60, 0x00, 0x01, 0x7f] // () -> i32
    } else {
        vec![0x01, 0x60, 0x00, 0x00] // () -> ()
    };
    section(1, functype, &mut m);
    section(3, vec![0x01, 0x00], &mut m); // func 0: type 0
    section(5, vec![0x01, 0x00, 0x04], &mut m); // memory min 4 pages (regs + guest RAM)
    let mut exp = vec![0x02];
    exp.extend([0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00]); // "mem"
    exp.extend([0x03, 0x72, 0x75, 0x6e, 0x00, 0x00]); // "run"
    section(7, exp, &mut m);
    let mut cs = Vec::new();
    uleb(1, &mut cs);
    uleb(code.len() as u64, &mut cs);
    cs.extend(code);
    section(10, cs, &mut m);
    m
}

/// Run a compiled block via wasmtime over a register file (mem `0..128`) and guest RAM
/// (mem `GUEST_BASE..`). `ram` is updated in place by any stores; returns the result regs.
#[cfg(test)]
fn run_wasm(bytes: &[u8], regs: [u64; NREG], ram: &mut [u8]) -> [u64; NREG] {
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("emitted wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, GUEST_BASE as usize, ram).unwrap();
    run.call(&mut store, ()).expect("run");
    let mut out = [0u64; NREG];
    for (i, o) in out.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    mem.read(&store, GUEST_BASE as usize, ram).unwrap();
    out
}

/// As `run_wasm`, plus the software TLB image written at `TLB_BASE` — for inline-TLB blocks
/// whose memory ops translate guest virtual addresses through it. Returns `(regs, bail)`
/// where `bail` is the index of the instruction that took a TLB miss (or `block.len()` if
/// the block completed) — what the interpreter would resume from.
#[cfg(test)]
fn run_wasm_tlb(bytes: &[u8], regs: [u64; NREG], ram: &mut [u8], tlb: &[u8]) -> ([u64; NREG], i32) {
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("emitted wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, TLB_BASE as usize, tlb).unwrap();
    mem.write(&mut store, GUEST_BASE as usize, ram).unwrap();
    let bail = run.call(&mut store, ()).expect("run");
    let mut out = [0u64; NREG];
    for (i, o) in out.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    mem.read(&store, GUEST_BASE as usize, ram).unwrap();
    (out, bail)
}

// A tiny seeded PRNG (deterministic, no Math.random/Date) for the fuzz corpus.
#[cfg(test)]
struct Rng(u64);
#[cfg(test)]
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn reg(&mut self) -> u8 {
        (self.next() % NREG as u64) as u8
    }
}

#[test]
fn jit_codegen_is_bit_identical_to_the_interpreter() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..400 {
        // a random block of 1..40 ops over the 16 regs
        let n = 1 + (rng.next() % 40);
        let mut block = Vec::new();
        for _ in 0..n {
            let sh = (rng.next() % 64) as u8;
            block.push(match rng.next() % 9 {
                0 => Op::Movi { d: rng.reg(), imm: rng.next() },
                1 => Op::Bin { op: Bin::Add, d: rng.reg(), a: rng.reg(), b: rng.reg() },
                2 => Op::Bin { op: Bin::Sub, d: rng.reg(), a: rng.reg(), b: rng.reg() },
                3 => Op::Bin { op: Bin::Xor, d: rng.reg(), a: rng.reg(), b: rng.reg() },
                4 => Op::Bin { op: Bin::And, d: rng.reg(), a: rng.reg(), b: rng.reg() },
                5 => Op::Bin { op: Bin::Or, d: rng.reg(), a: rng.reg(), b: rng.reg() },
                6 => Op::Shift { op: Sh::ShrU, d: rng.reg(), a: rng.reg(), sh },
                7 => Op::Shift { op: Sh::Shl, d: rng.reg(), a: rng.reg(), sh },
                _ => Op::Shift { op: Sh::Rotr, d: rng.reg(), a: rng.reg(), sh },
            });
        }
        // random initial register state
        let mut regs = [0u64; NREG];
        for r in regs.iter_mut() {
            *r = rng.next();
        }
        let mut want = regs;
        let mut ram = vec![0u8; GUEST_LEN];
        interpret(&block, &mut want, &mut ram);
        let got = run_wasm(&compile(&block), regs, &mut ram);
        assert_eq!(got, want, "JIT block result diverged from the interpreter");
    }
}

/// A realistic compute block: one SHA-512 round-ish mix (Maj + a Sigma) — the shape the
/// real codegen will compile from `sha512_transform`.
#[test]
fn jit_codegen_matches_on_a_sha512_shaped_block() {
    // Maj(a,b,c) = (a&b) ^ (a&c) ^ (b&c);  S0(a) = ror(a,28) ^ ror(a,34) ^ ror(a,39)
    let block = [
        Op::Bin { op: Bin::And, d: 8, a: 0, b: 1 },   // t0 = a & b
        Op::Bin { op: Bin::And, d: 9, a: 0, b: 2 },   // t1 = a & c
        Op::Bin { op: Bin::And, d: 10, a: 1, b: 2 },  // t2 = b & c
        Op::Bin { op: Bin::Xor, d: 8, a: 8, b: 9 },   // t0 ^= t1
        Op::Bin { op: Bin::Xor, d: 8, a: 8, b: 10 },  // maj = t0 ^ t2  -> r8
        Op::Shift { op: Sh::Rotr, d: 9, a: 0, sh: 28 },
        Op::Shift { op: Sh::Rotr, d: 10, a: 0, sh: 34 },
        Op::Shift { op: Sh::Rotr, d: 11, a: 0, sh: 39 },
        Op::Bin { op: Bin::Xor, d: 9, a: 9, b: 10 },
        Op::Bin { op: Bin::Xor, d: 9, a: 9, b: 11 },  // S0 = ... -> r9
    ];
    let mut regs = [0u64; NREG];
    regs[0] = 0x6a09_e667_f3bc_c908;
    regs[1] = 0xbb67_ae85_84ca_a73b;
    regs[2] = 0x3c6e_f372_fe94_f82b;
    let mut want = regs;
    let mut ram = vec![0u8; GUEST_LEN];
    interpret(&block, &mut want, &mut ram);
    assert_eq!(run_wasm(&compile(&block), regs, &mut ram), want, "SHA-512-shaped block diverged");
}

/// BENCHMARK (ignored) — the JIT's whole reason to exist: does executing a *compiled* block beat
/// *interpreting* it, INCLUDING the per-call marshalling (regs in/out of wasm linear memory)? The
/// wasm Module+Instance are built ONCE and reused (steady state), so this isolates
/// execute+marshal vs interpret — not compile/instantiate. The block is the 10-op SHA-512-shaped
/// mix the codegen targets. NOTE: the comparison baseline here is the fast *IR* interpreter (a
/// Rust match over `Op`s, no x86 decode); the REAL emulator interpreter (`Cpu::step`) decodes x86
/// bytes per instruction and is far heavier, so the real-world JIT speedup is *larger* than the
/// ratio printed here — if the JIT can't beat even the IR interpreter, the marshalling dominates
/// and the design needs rethinking before the browser (js_sys::WebAssembly call overhead is worse).
/// Run: `cargo test -p holospaces --release --features jit --lib jit_block_execute_speedup -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: warm JIT block execute+marshal vs IR interpret"]
fn jit_block_execute_speedup() {
    use std::time::Instant;
    let block = [
        Op::Bin { op: Bin::And, d: 8, a: 0, b: 1 },
        Op::Bin { op: Bin::And, d: 9, a: 0, b: 2 },
        Op::Bin { op: Bin::And, d: 10, a: 1, b: 2 },
        Op::Bin { op: Bin::Xor, d: 8, a: 8, b: 9 },
        Op::Bin { op: Bin::Xor, d: 8, a: 8, b: 10 },
        Op::Shift { op: Sh::Rotr, d: 9, a: 0, sh: 28 },
        Op::Shift { op: Sh::Rotr, d: 10, a: 0, sh: 34 },
        Op::Shift { op: Sh::Rotr, d: 11, a: 0, sh: 39 },
        Op::Bin { op: Bin::Xor, d: 9, a: 9, b: 10 },
        Op::Bin { op: Bin::Xor, d: 9, a: 9, b: 11 },
    ];
    let wasm = compile(&block);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("valid wasm");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    let regs0: [u64; NREG] = {
        let mut r = [0u64; NREG];
        r[0] = 0x6a09_e667_f3bc_c908;
        r[1] = 0xbb67_ae85_84ca_a73b;
        r[2] = 0x3c6e_f372_fe94_f82b;
        r
    };
    let n = 2_000_000u64;
    let mut acc = 0u64;
    // Warm both paths.
    for _ in 0..10_000 {
        for (i, v) in regs0.iter().enumerate() { mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap(); }
        run.call(&mut store, ()).unwrap();
    }
    // JIT: marshal regs in, execute the compiled block, marshal a result reg out — per iteration.
    let t = Instant::now();
    for _ in 0..n {
        for (i, v) in regs0.iter().enumerate() { mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap(); }
        run.call(&mut store, ()).unwrap();
        let mut b = [0u8; 8];
        mem.read(&store, 8 * 8, &mut b).unwrap();
        acc ^= u64::from_le_bytes(b);
    }
    let jit_ns = t.elapsed().as_nanos() as f64 / n as f64;
    // JIT "chained" (the stay-in-wasm model): regs persist in wasm memory across blocks, so NO
    // per-block reg marshalling — only the wasm call + execution. This is the ceiling the real
    // dispatch reaches if it chains consecutive blocks in wasm instead of round-tripping to the
    // interpreter per block. Write regs ONCE, then call repeatedly.
    for (i, v) in regs0.iter().enumerate() { mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap(); }
    let t = Instant::now();
    for _ in 0..n {
        run.call(&mut store, ()).unwrap();
    }
    let chained_ns = t.elapsed().as_nanos() as f64 / n as f64;
    {
        let mut b = [0u8; 8];
        mem.read(&store, 8 * 8, &mut b).unwrap();
        acc ^= u64::from_le_bytes(b);
    }
    // IR interpreter: the same block, the fast Rust-match baseline.
    let mut ram = vec![0u8; GUEST_LEN];
    let t = Instant::now();
    for _ in 0..n {
        let mut r = regs0;
        interpret(&block, &mut r, &mut ram);
        acc ^= r[8];
    }
    let int_ns = t.elapsed().as_nanos() as f64 / n as f64;
    let ops = block.len() as f64;
    // The real interpreter (Cpu::step) costs ~45 ns/op (measured: cc45 interpreter_throughput) — the
    // baseline the JIT actually replaces. Use it to report the real-world speedup of each model.
    let step_ns_per_op = 45.4;
    eprintln!(
        "\n==== JIT BLOCK BENCH ({} ops) ====\n\
         JIT marshal-per-block (current dispatch): {:.1} ns/block  ({:.2} ns/op)  → {:.1}x vs Cpu::step\n\
         JIT chained / stay-in-wasm (ceiling):      {:.1} ns/block  ({:.2} ns/op)  → {:.1}x vs Cpu::step\n\
         IR interpret (wrong baseline, ref only):   {:.1} ns/block  ({:.2} ns/op)\n\
         marshalling overhead per block: {:.1} ns ({:.0}%% of marshal-per-block time)\n\
         → DIRECTIVE: chain blocks in wasm; per-block marshalling costs {:.1}x throughput.\n\
         guard {:#x}\n====\n",
        block.len(),
        jit_ns, jit_ns / ops, step_ns_per_op / (jit_ns / ops),
        chained_ns, chained_ns / ops, step_ns_per_op / (chained_ns / ops),
        int_ns, int_ns / ops,
        jit_ns - chained_ns, 100.0 * (jit_ns - chained_ns) / jit_ns,
        jit_ns / chained_ns,
        acc,
    );
}

/// Decode an x86-64 memory operand (ModRM mod≠3): the ModRM r/m field, an optional SIB
/// byte, and disp8/disp32 → `(base, idx, scale, disp)` using `NO_REG` sentinels. `p` points
/// at the ModRM byte; returns the address mode and the position past the displacement.
/// Bails (`None`) on RIP-relative (needs the instruction address) or truncated input.
fn decode_mem(bytes: &[u8], p0: usize, rex: u8, mod_: u8, modrm: u8) -> Option<((u8, u8, u8, i32), usize)> {
    let mut p = p0 + 1; // past ModRM
    let rexb = if rex & 0x01 != 0 { 8u8 } else { 0 }; // REX.B → base / r-m high bit
    let rexx = if rex & 0x02 != 0 { 8u8 } else { 0 }; // REX.X → index high bit
    let rm_low = modrm & 7;
    let (mut base, mut idx, mut scale) = (NO_REG, NO_REG, 0u8);
    let mut disp32_no_base = false;
    if rm_low == 4 {
        // SIB byte
        let sib = *bytes.get(p)?;
        p += 1;
        scale = sib >> 6;
        let index = (sib >> 3) & 7;
        if index != 4 || rexx != 0 {
            idx = index | rexx; // index==0b100 w/o REX.X means "no index"
        }
        let base_low = sib & 7;
        if base_low == 5 && mod_ == 0 {
            disp32_no_base = true; // base absent, disp32 follows
        } else {
            base = base_low | rexb;
        }
    } else if rm_low == 5 && mod_ == 0 {
        return None; // RIP-relative — needs the instruction address; bail
    } else {
        base = rm_low | rexb;
    }
    let disp: i32 = if mod_ == 1 {
        let d = *bytes.get(p)? as i8 as i32;
        p += 1;
        d
    } else if mod_ == 2 || disp32_no_base {
        let s = bytes.get(p..p + 4)?;
        p += 4;
        i32::from_le_bytes([s[0], s[1], s[2], s[3]])
    } else {
        0
    };
    Some(((base, idx, scale, disp), p))
}

/// Decode the IR for the bytes (no length). See [`decode_block`].
pub(crate) fn decode_x86(bytes: &[u8]) -> Vec<Op> {
    decode_block(bytes).0
}

/// How a basic block leaves — the control-flow front-end for region discovery (chaining). Target
/// rips are absolute GUEST addresses (computed from the rel displacement + the next-instruction rip).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Terminator {
    /// `jmp rel` — flows unconditionally to one in-region target.
    Jmp { target: u64 },
    /// `jcc rel` — taken target or fall-through (both candidate region blocks). `cc` = opcode nibble.
    Jcc { cc: u8, taken: u64, fall: u64 },
    /// Leave the JIT region — `ret`/`call`/indirect branch/uncovered instruction. The interpreter
    /// resumes at `rip` (the block's end), which is where `decode_block` stopped.
    Exit,
}

/// Decode the control-flow instruction at `bytes[p]` (block end), with `base` the block's start rip,
/// into a [`Terminator`] + its byte length. `None` for anything that isn't a *direct* `jmp`/`jcc`
/// (ret/call/indirect/uncovered → the region exits to the interpreter).
fn decode_terminator(bytes: &[u8], p: usize, base: u64) -> Option<(Terminator, usize)> {
    let next = |len: usize| base.wrapping_add((p + len) as u64);
    match *bytes.get(p)? {
        0xeb => {
            let rel = *bytes.get(p + 1)? as i8 as i64;
            Some((Terminator::Jmp { target: next(2).wrapping_add(rel as u64) }, 2))
        }
        0xe9 => {
            let s = bytes.get(p + 1..p + 5)?;
            let rel = i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as i64;
            Some((Terminator::Jmp { target: next(5).wrapping_add(rel as u64) }, 5))
        }
        op @ 0x70..=0x7f => {
            let rel = *bytes.get(p + 1)? as i8 as i64;
            let fall = next(2);
            Some((Terminator::Jcc { cc: op & 0xf, taken: fall.wrapping_add(rel as u64), fall }, 2))
        }
        0x0f if matches!(bytes.get(p + 1), Some(0x80..=0x8f)) => {
            let op = bytes[p + 1];
            let s = bytes.get(p + 2..p + 6)?;
            let rel = i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as i64;
            let fall = next(6);
            Some((Terminator::Jcc { cc: op & 0xf, taken: fall.wrapping_add(rel as u64), fall }, 6))
        }
        _ => None, // ret/call/indirect/uncovered → Exit
    }
}

/// One block of a [`Region`]: its guest entry rip, decoded ops, byte length, and how it leaves.
#[derive(Clone, Debug)]
pub(crate) struct RegionBlock {
    pub start: u64,
    pub ops: Vec<Op>,
    pub len: usize,
    pub term: Terminator,
}

/// A discovered TRACE: a set of directly-linked basic blocks the JIT compiles into one wasm unit
/// (the chaining region). `blocks[0]` is the entry; terminators reference blocks by their `start`
/// rip (resolve via `index_of`). A target that leaves the region (out of range, budget, or an
/// `Exit` terminator) becomes a region exit at run time (marshal out, interpreter resumes).
#[derive(Clone, Debug)]
pub(crate) struct Region {
    pub entry: u64,
    pub blocks: Vec<RegionBlock>,
}

impl Region {
    /// The block index for guest rip `rip`, if `rip` is a block entry in this region.
    #[must_use]
    pub(crate) fn index_of(&self, rip: u64) -> Option<usize> {
        self.blocks.iter().position(|b| b.start == rip)
    }
}

/// Discover a chaining region from `entry`, following DIRECT branch targets (`Jmp`/`Jcc`) through
/// the code image `code` (guest rip `R` → byte `code[R - base]`), up to `max_blocks`. A target that
/// falls outside the image or hits the budget is simply not added — at run time the dispatcher exits
/// to the interpreter there. Blocks are deduped by entry rip (so a loop back-edge reuses the block).
pub(crate) fn discover_region(code: &[u8], base: u64, max_blocks: usize) -> Region {
    let mut blocks: Vec<RegionBlock> = Vec::new();
    let mut work: Vec<u64> = alloc::vec![base];
    let in_range = |rip: u64| rip >= base && (rip - base) < code.len() as u64;
    while let Some(rip) = work.pop() {
        if blocks.len() >= max_blocks || blocks.iter().any(|b| b.start == rip) || !in_range(rip) {
            continue;
        }
        let off = (rip - base) as usize;
        let (ops, len, term) = decode_block_term(&code[off..], rip);
        // enqueue in-region successors (LIFO; order doesn't affect correctness, only layout)
        match term {
            Terminator::Jmp { target } => work.push(target),
            Terminator::Jcc { taken, fall, .. } => {
                work.push(fall);
                work.push(taken);
            }
            Terminator::Exit => {}
        }
        blocks.push(RegionBlock { start: rip, ops, len, term });
    }
    // ensure blocks[0] is the entry (pop order may have placed it elsewhere)
    if let Some(i) = blocks.iter().position(|b| b.start == base) {
        blocks.swap(0, i);
    }
    Region { entry: base, blocks }
}

/// Discover the region at guest rip `base` over `code` and compile it to a chained region wasm (the
/// `() -> ()` module the executors run) — the public codegen entry point. It returns the SAME
/// artifact `x64::jit_run_region` builds internally, so an executor or a differential test can obtain
/// a region wasm without reaching into the crate-private codegen. `max_iters` is the per-entry
/// interrupt-yield bound (the region self-limits to this many back-edges before yielding).
#[must_use]
pub fn compile_region_from_code(code: &[u8], base: u64, max_iters: u64) -> Vec<u8> {
    let region = discover_region(code, base, 16);
    compile_region(&region, max_iters)
}

/// Decode a basic block at guest rip `base`: the straight-line ops ([`decode_block`]), then the
/// terminating control-flow instruction. Returns `(ops, len, term)` where `len` INCLUDES the
/// terminator, so the next block begins at `base + len` (and on `Exit`, `len` is just the ops, so
/// the interpreter resumes exactly at the unmodelled instruction). The chaining front-end: a region
/// is these blocks linked by their `Jmp`/`Jcc` targets.
pub(crate) fn decode_block_term(bytes: &[u8], base: u64) -> (Vec<Op>, usize, Terminator) {
    let (ops, _offsets, len) = decode_block(bytes);
    match decode_terminator(bytes, len, base) {
        Some((t, tlen)) => (ops, len + tlen, t),
        None => (ops, len, Terminator::Exit),
    }
}

/// κ-keyed compiled-block cache (Rung 2). Keyed by the **BLAKE3** digest of a block's guest
/// bytes (κ) — a self-modifying write changes the bytes → changes κ → misses → recompiles
/// under the new key (free SMC invalidation). The cache is hash-agnostic: `run()` supplies
/// the digest (via the substrate's `kr_blake3`); the cache only counts hotness and stores
/// the compiled wasm once a block crosses the threshold.
pub(crate) struct BlockCache {
    entries: BTreeMap<[u8; 32], CacheEntry>,
    threshold: u32,
}

struct CacheEntry {
    hits: u32,
    wasm: Option<Vec<u8>>, // Some once compiled at the hotness threshold
}

impl BlockCache {
    pub(crate) fn new(threshold: u32) -> Self {
        Self { entries: BTreeMap::new(), threshold }
    }

    /// The compiled wasm for this block's κ if it is already compiled, else `None`.
    pub(crate) fn get(&self, key: &[u8; 32]) -> Option<&[u8]> {
        self.entries.get(key).and_then(|e| e.wasm.as_deref())
    }

    /// `(distinct blocks seen, how many have been compiled)` — for boot-time reporting.
    pub(crate) fn stats(&self) -> (usize, usize) {
        let compiled = self.entries.values().filter(|e| e.wasm.is_some()).count();
        (self.entries.len(), compiled)
    }

    /// Record one execution of the block at κ `key`; once hotness reaches the threshold,
    /// compile it (`compile_tlb_flags`) and cache the wasm. Returns the compiled wasm when
    /// available (so the caller can execute it this turn).
    pub(crate) fn record(&mut self, key: [u8; 32], ops: &[Op]) -> Option<&[u8]> {
        let e = self.entries.entry(key).or_insert(CacheEntry { hits: 0, wasm: None });
        e.hits = e.hits.saturating_add(1);
        if e.wasm.is_none() && e.hits >= self.threshold {
            e.wasm = Some(compile_tlb_flags(ops));
        }
        e.wasm.as_deref()
    }
}

/// x86-64 block discovery for the SHA-512 compression subset: reg-reg ALU/mov (ModRM mod=3)
/// AND memory-operand forms (`mov reg,[mem]`, `mov [mem],reg`, `add/or/and/sub/xor reg,[mem]`)
/// with full base+index*scale+disp addressing. Returns `(ops, offsets, len)`: the decoded
/// ops, the guest-byte offset where each op's instruction starts (so a bail at op `k` resumes
/// at `block_start + offsets[k]`), and the exact block byte length (`len`, for the κ key and
/// to advance `rip`). **Bails** (stops) at anything else — the JIT's "interpret what I don't
/// model" discipline; the unmodelled instruction's bytes are NOT counted, so the interpreter
/// resumes there.
pub(crate) fn decode_block(bytes: &[u8]) -> (Vec<Op>, Vec<u32>, usize) {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    let mut p = 0;
    let mut consumed = 0;
    while p < bytes.len() {
        let inst_start = p;
        let mut rex = 0u8;
        if bytes[p] & 0xf0 == 0x40 {
            rex = bytes[p];
            p += 1;
        }
        if p + 1 >= bytes.len() {
            break;
        }
        let opcode = bytes[p];
        let modrm = bytes[p + 1];
        let mod_ = modrm >> 6;
        let reg = ((modrm >> 3) & 7) | if rex & 0x04 != 0 { 8 } else { 0 }; // REX.R
        let rexw = rex & 0x08 != 0; // 64-bit operand size
        // push/pop r64 (0x50+r / 0x58+r): single-byte, no ModRM, always 64-bit in long mode (REX.W
        // irrelevant; REX.B extends the register). The first ops that touch the stack.
        if (0x50..=0x5f).contains(&opcode) {
            let r = (opcode & 7) | if rex & 0x01 != 0 { 8 } else { 0 }; // REX.B
            out.push(if opcode < 0x58 { Op::Push { s: r } } else { Op::Pop { d: r } });
            offsets.push(inst_start as u32);
            p += 1;
            consumed = p;
            continue;
        }
        // `mov r64, imm` (0xb8+r): NO ModRM — the register is in the opcode's low 3 bits, the
        // immediate follows. REX.W → imm64; else imm32 (32-bit mov ZERO-extends to 64, so `imm as
        // u64` is exact either way). Always correct, so decode regardless of REX.W.
        if (0xb8..=0xbf).contains(&opcode) {
            let d = (opcode & 7) | if rex & 0x01 != 0 { 8 } else { 0 }; // REX.B
            let imm_at = p + 1;
            let (imm, ilen) = if rexw {
                let s = match bytes.get(imm_at..imm_at + 8) {
                    Some(s) => s,
                    None => break,
                };
                (u64::from_le_bytes(s.try_into().unwrap()), 8)
            } else {
                let s = match bytes.get(imm_at..imm_at + 4) {
                    Some(s) => s,
                    None => break,
                };
                (u64::from(u32::from_le_bytes(s.try_into().unwrap())), 4)
            };
            out.push(Op::Movi { d, imm });
            offsets.push(inst_start as u32);
            p = imm_at + ilen;
            consumed = p;
            continue;
        }
        // 64-bit register-direct immediate forms (REX.W, mod==11): ALU `r/m64, imm8`(0x83, sign-ext)
        // / `r/m64, imm32`(0x81, sign-ext) and `mov r/m64, imm32`(0xc7 /0, sign-ext). Only when
        // REX.W is set — a 32-bit form zero-extends and would diverge from the 64-bit model (the
        // SHADOW gate would refuse it anyway), so leave those to the interpreter. Memory forms
        // (mod != 11) fall through to the load/store path's `_ => break` (a region exit), as before.
        if rexw && mod_ == 3 && matches!(opcode, 0x81 | 0x83 | 0xc7) {
            let rm = (modrm & 7) | if rex & 0x01 != 0 { 8 } else { 0 }; // REX.B
            let digit = (modrm >> 3) & 7; // the ALU sub-op / the mov `/0`
            let imm_at = p + 2;
            let (imm, ilen) = if opcode == 0x83 {
                match bytes.get(imm_at) {
                    Some(&b) => (b as i8 as i64 as u64, 1), // imm8 sign-extended
                    None => break,
                }
            } else {
                match bytes.get(imm_at..imm_at + 4) {
                    Some(s) => (i32::from_le_bytes(s.try_into().unwrap()) as i64 as u64, 4), // imm32 s-ext
                    None => break,
                }
            };
            let op = if opcode == 0xc7 {
                if digit != 0 {
                    break; // 0xc7 is only `/0` = mov; other digits are not this form
                }
                Op::Movi { d: rm, imm }
            } else {
                match digit {
                    0 => Op::BinImm { op: Bin::Add, d: rm, imm },
                    1 => Op::BinImm { op: Bin::Or, d: rm, imm },
                    4 => Op::BinImm { op: Bin::And, d: rm, imm },
                    5 => Op::BinImm { op: Bin::Sub, d: rm, imm },
                    6 => Op::BinImm { op: Bin::Xor, d: rm, imm },
                    7 => Op::CmpImm { a: rm, imm },
                    _ => break, // adc(/2)/sbb(/3) not modelled
                }
            };
            out.push(op);
            offsets.push(inst_start as u32);
            p = imm_at + ilen;
            consumed = p;
            continue;
        }
        if mod_ == 3 {
            // OPERAND SIZE: only 64-bit (REX.W) reg-reg forms are modelled. A 32-bit op ZERO-EXTENDS
            // its result into the upper 32 bits (and 32-bit cmp/test set flags on the low 32) — the
            // codegen here is 64-bit, so a non-REX.W form would diverge. Stop the block (the
            // interpreter handles it) rather than emit a wrong op the SHADOW gate must catch.
            if !rexw {
                break;
            }
            // reg-reg: `op r/m, r` (dst=r/m) or `op r, r/m` (dst=reg) / mov both ways
            let rm = (modrm & 7) | if rex & 0x01 != 0 { 8 } else { 0 };
            let dst_rm = |op| Op::Bin { op, d: rm, a: rm, b: reg };
            let dst_reg = |op| Op::Bin { op, d: reg, a: reg, b: rm };
            let op = match opcode {
                0x01 => dst_rm(Bin::Add),
                0x09 => dst_rm(Bin::Or),
                0x21 => dst_rm(Bin::And),
                0x29 => dst_rm(Bin::Sub),
                0x31 => dst_rm(Bin::Xor),
                0x89 => Op::Movr { d: rm, s: reg }, // mov r/m, r
                0x03 => dst_reg(Bin::Add),
                0x0b => dst_reg(Bin::Or),
                0x23 => dst_reg(Bin::And),
                0x2b => dst_reg(Bin::Sub),
                0x33 => dst_reg(Bin::Xor),
                0x8b => Op::Movr { d: reg, s: rm }, // mov r, r/m
                0x39 => Op::Cmp { a: rm, b: reg }, // cmp r/m, r → flags of r/m - r
                0x3b => Op::Cmp { a: reg, b: rm }, // cmp r, r/m → flags of r - r/m
                0x85 => Op::Test { a: rm, b: reg }, // test r/m, r → flags of r/m & r
                _ => break,
            };
            out.push(op);
            offsets.push(inst_start as u32);
            p += 2;
            consumed = p;
        } else {
            // OPERAND SIZE: only 64-bit (REX.W) memory forms are modelled. A 32-bit `mov [mem], reg`
            // writes 4 bytes (not 8) and a 32-bit load zero-extends; the 64-bit `i64.store`/`i64.load`
            // codegen would clobber/read 4 extra bytes (the real divergence root-caused on a stack
            // prologue). A 32-bit `lea` also truncates its address to 32 bits. Stop the block instead.
            if !rexw {
                break;
            }
            let ((base, idx, scale, disp), np) = match decode_mem(bytes, p + 1, rex, mod_, modrm) {
                Some(x) => x,
                None => break,
            };
            let loadop = |op| Op::LoadOp { op, d: reg, base, idx, scale, disp };
            let op = match opcode {
                0x8b => Op::Load { d: reg, base, idx, scale, disp }, // mov reg, [mem]
                0x89 => Op::Store { base, idx, scale, disp, s: reg }, // mov [mem], reg
                0x8d => Op::Lea { d: reg, base, idx, scale, disp }, // lea reg, [mem] (address only)
                0x03 => loadop(Bin::Add),
                0x0b => loadop(Bin::Or),
                0x23 => loadop(Bin::And),
                0x2b => loadop(Bin::Sub),
                0x33 => loadop(Bin::Xor),
                _ => break,
            };
            out.push(op);
            offsets.push(inst_start as u32);
            p = np;
            consumed = p;
        }
    }
    (out, offsets, consumed)
}

#[test]
fn x86_decoder_maps_reg_reg_alu_to_the_ir() {
    // add rax,rbx (48 01 d8): r/m=rax(0) += reg=rbx(3)
    assert_eq!(decode_x86(&[0x48, 0x01, 0xd8]), vec![Op::Bin { op: Bin::Add, d: 0, a: 0, b: 3 }]);
    // xor r9,r9 (4d 31 c9): REX.WRB → reg & rm both +8
    assert_eq!(decode_x86(&[0x4d, 0x31, 0xc9]), vec![Op::Bin { op: Bin::Xor, d: 9, a: 9, b: 9 }]);
    // and rcx,rdx (48 21 d1)
    assert_eq!(decode_x86(&[0x48, 0x21, 0xd1]), vec![Op::Bin { op: Bin::And, d: 1, a: 1, b: 2 }]);
    // mov rbx,rax (48 89 c3): rbx(3) = rax(0)
    assert_eq!(decode_x86(&[0x48, 0x89, 0xc3]), vec![Op::Movr { d: 3, s: 0 }]);
    // a two-instruction sequence
    assert_eq!(
        decode_x86(&[0x48, 0x01, 0xd8, 0x48, 0x31, 0xc8]),
        vec![Op::Bin { op: Bin::Add, d: 0, a: 0, b: 3 }, Op::Bin { op: Bin::Xor, d: 0, a: 0, b: 1 }]
    );
}

#[test]
fn x86_decoder_maps_immediates_to_the_ir() {
    // add rax, 5  (48 83 c0 05): 0x83 /0, imm8
    assert_eq!(decode_x86(&[0x48, 0x83, 0xc0, 0x05]), vec![Op::BinImm { op: Bin::Add, d: 0, imm: 5 }]);
    // add rax, -1 (48 83 c0 ff): imm8 SIGN-extended to 64
    assert_eq!(
        decode_x86(&[0x48, 0x83, 0xc0, 0xff]),
        vec![Op::BinImm { op: Bin::Add, d: 0, imm: u64::MAX }]
    );
    // and rdx, 0xf (48 83 e2 0f): /4
    assert_eq!(decode_x86(&[0x48, 0x83, 0xe2, 0x0f]), vec![Op::BinImm { op: Bin::And, d: 2, imm: 0xf }]);
    // cmp rax, 10 (48 83 f8 0a): /7 → flag-only CmpImm
    assert_eq!(decode_x86(&[0x48, 0x83, 0xf8, 0x0a]), vec![Op::CmpImm { a: 0, imm: 10 }]);
    // sub rcx, 0x100 (48 81 e9 00 01 00 00): 0x81 /5, imm32
    assert_eq!(
        decode_x86(&[0x48, 0x81, 0xe9, 0x00, 0x01, 0x00, 0x00]),
        vec![Op::BinImm { op: Bin::Sub, d: 1, imm: 0x100 }]
    );
    // mov rax, 0x123456 (48 c7 c0 56 34 12 00): 0xc7 /0, imm32 sign-extended
    assert_eq!(
        decode_x86(&[0x48, 0xc7, 0xc0, 0x56, 0x34, 0x12, 0x00]),
        vec![Op::Movi { d: 0, imm: 0x0012_3456 }]
    );
    // movabs rax, 0x1122334455667788 (48 b8 + imm64)
    assert_eq!(
        decode_x86(&[0x48, 0xb8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]),
        vec![Op::Movi { d: 0, imm: 0x1122_3344_5566_7788 }]
    );
    // mov eax, 0x10 (b8 + imm32, no REX.W): 32-bit mov zero-extends → exact as u64
    assert_eq!(decode_x86(&[0xb8, 0x10, 0x00, 0x00, 0x00]), vec![Op::Movi { d: 0, imm: 0x10 }]);
    // a real counted-loop body: add rax,1 ; cmp rax,8  decodes as TWO ops (no early exit)
    assert_eq!(
        decode_x86(&[0x48, 0x83, 0xc0, 0x01, 0x48, 0x83, 0xf8, 0x08]),
        vec![Op::BinImm { op: Bin::Add, d: 0, imm: 1 }, Op::CmpImm { a: 0, imm: 8 }]
    );
}

/// A minimal `() -> i32` module that loads rflags from `mem[RFLAGS_MEM]` and returns
/// [`emit_cc`]`(cc)` — to test the condition-code codegen in isolation (the chaining keystone's
/// `jcc` predicate, before the dispatcher exists).
#[cfg(test)]
fn compile_cc_probe(cc: u8) -> Vec<u8> {
    let mut code = Vec::new();
    uleb(1, &mut code); // one local group
    uleb(u64::from(RFLAGS_LOCAL) + 1, &mut code); // RFLAGS_LOCAL+1 × i64 (so index 17 is valid)
    code.push(0x7e);
    code.push(0x41);
    sleb(RFLAGS_MEM as i64, &mut code); // i32.const RFLAGS_MEM
    code.extend([0x29, 0x03, 0x00]); // i64.load
    code.push(0x21);
    uleb(u64::from(RFLAGS_LOCAL), &mut code); // local.set $rflags
    emit_cc(cc, &mut code); // → i32 on the stack (the function result)
    code.push(0x0b); // end function
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    section(1, vec![0x01, 0x60, 0x00, 0x01, 0x7f], &mut m); // () -> i32
    section(3, vec![0x01, 0x00], &mut m);
    section(5, vec![0x01, 0x00, 0x04], &mut m);
    let mut exp = vec![0x02];
    exp.extend([0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00]); // "mem"
    exp.extend([0x03, 0x72, 0x75, 0x6e, 0x00, 0x00]); // "run"
    section(7, exp, &mut m);
    let mut cs = Vec::new();
    uleb(1, &mut cs);
    uleb(code.len() as u64, &mut cs);
    cs.extend(code);
    section(10, cs, &mut m);
    m
}

#[test]
fn jit_condition_codes_match_the_interpreter() {
    let engine = Engine::default();
    let mut rng = Rng(0xc0ff_ee12_3456_789a);
    for cc in 0u8..16 {
        let module = Module::new(&engine, &compile_cc_probe(cc)).expect("cc-probe wasm is valid");
        for _ in 0..64 {
            let rflags = rng.next();
            let mut store = Store::new(&engine, ());
            let instance = Instance::new(&mut store, &module, &[]).unwrap();
            let mem = instance.get_memory(&mut store, "mem").unwrap();
            let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
            mem.write(&mut store, RFLAGS_MEM as usize, &rflags.to_le_bytes()).unwrap();
            let got = run.call(&mut store, ()).unwrap();
            assert_eq!(
                got != 0,
                cc_taken(cc, rflags),
                "cc={cc:#x} rflags={rflags:#018x}: in-wasm jcc codegen vs the oracle",
            );
        }
    }
}

#[test]
fn block_terminators_decode_to_absolute_targets() {
    let base = 0x1000u64;
    // xor rax,rax ; jmp +5   (48 31 c0 | eb 05)
    let (ops, len, term) = decode_block_term(&[0x48, 0x31, 0xc0, 0xeb, 0x05], base);
    assert_eq!(ops, vec![Op::Bin { op: Bin::Xor, d: 0, a: 0, b: 0 }]);
    assert_eq!(len, 5);
    assert_eq!(term, Terminator::Jmp { target: base + 5 + 5 }); // next(rip 0x1005) + 5
    // add rax,rbx ; jne -8   (48 01 d8 | 75 f8)  — a loop back-edge
    let (_ops, len, term) = decode_block_term(&[0x48, 0x01, 0xd8, 0x75, 0xf8], base);
    assert_eq!(len, 5);
    assert_eq!(
        term,
        Terminator::Jcc { cc: 5, taken: (base + 5).wrapping_sub(8), fall: base + 5 },
    );
    // add rax,rbx ; je +0x100 (near, 0f 84 ...)  — rel32 conditional
    let (_o, len, term) =
        decode_block_term(&[0x48, 0x01, 0xd8, 0x0f, 0x84, 0x00, 0x01, 0x00, 0x00], base);
    assert_eq!(len, 9);
    assert_eq!(term, Terminator::Jcc { cc: 4, taken: base + 9 + 0x100, fall: base + 9 });
    // add rax,rbx ; ret  (48 01 d8 | c3)  → Exit (region leaves to the interpreter)
    let (_o, len, term) = decode_block_term(&[0x48, 0x01, 0xd8, 0xc3], base);
    assert_eq!((len, term), (3, Terminator::Exit)); // len = ops only; resume at the ret
}

#[test]
fn compile_region_memcpy_loop_bit_identical() {
    // A memcpy self-loop: rax=[rsi]; [rdi]=rax; rsi+=8; rdi+=8; rcx-=1; jnz top.
    let base = 0x1000u64;
    let block = RegionBlock {
        start: base,
        len: 20,
        ops: alloc::vec![
            Op::Load { d: 0, base: 6, idx: NO_REG, scale: 0, disp: 0 }, // rax = [rsi]
            Op::Store { base: 7, idx: NO_REG, scale: 0, disp: 0, s: 0 }, // [rdi] = rax
            Op::Bin { op: Bin::Add, d: 6, a: 6, b: 15 }, // rsi += r15(8)
            Op::Bin { op: Bin::Add, d: 7, a: 7, b: 15 }, // rdi += r15(8)
            Op::Bin { op: Bin::Sub, d: 1, a: 1, b: 14 }, // rcx -= r14(1) → sets ZF at 0
        ],
        term: Terminator::Jcc { cc: 0x5, taken: base, fall: base + 0x1000 }, // jnz top; fall exits
    };
    let region = Region { entry: base, blocks: alloc::vec![block.clone()] };
    let src = 0x100usize;
    let dst = 0x800usize;
    let words: [u64; 4] = [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    let mut regs = [0u64; NREG];
    regs[6] = src as u64; // rsi
    regs[7] = dst as u64; // rdi
    regs[1] = words.len() as u64; // rcx = N
    regs[14] = 1; // decrement
    regs[15] = 8; // stride
    // Oracle: block-level interpret over guest-addressed RAM.
    let (mut wr, mut wf) = (regs, 0u64);
    let mut oram = alloc::vec![0u8; GUEST_LEN];
    for (i, w) in words.iter().enumerate() {
        oram[src + i * 8..src + i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let mut iters = 0u64;
    while wr[1] != 0 && iters < 100 {
        interpret_full(&block.ops, &mut wr, &mut oram, &mut wf);
        iters += 1;
    }
    // JIT: run the region; guest RAM lives at wasm GUEST_BASE + addr.
    let wasm = compile_region(&region, 100);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("memcpy region wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &0u64.to_le_bytes()).unwrap();
    // Software-TLB image: an identity entry for guest page 0 (src/dst both live there) — tag=vpage,
    // host_off=vpage*0x1000 → host = GUEST_BASE + va. The inline-TLB lookup in the region hits it.
    let mut tlb = alloc::vec![0u8; TLB_SIZE as usize * 16];
    tlb[0..8].copy_from_slice(&0u64.to_le_bytes()); // slot 0 tag = vpage 0
    tlb[8..16].copy_from_slice(&0u64.to_le_bytes()); // host_off = 0
    mem.write(&mut store, TLB_BASE as usize, &tlb).unwrap();
    for (i, w) in words.iter().enumerate() {
        mem.write(&mut store, GUEST_BASE as usize + src + i * 8, &w.to_le_bytes()).unwrap();
    }
    let _exit = run.call(&mut store, ()).unwrap();
    let mut got = [0u64; NREG];
    for (i, o) in got.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    assert_eq!(got, wr, "registers bit-identical after the memcpy region");
    // The destination words were copied (in both the oracle RAM and the wasm memory).
    for (i, w) in words.iter().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, GUEST_BASE as usize + dst + i * 8, &mut b).unwrap();
        assert_eq!(u64::from_le_bytes(b), *w, "word {i} copied to dst (wasm mem)");
        assert_eq!(
            u64::from_le_bytes(oram[dst + i * 8..dst + i * 8 + 8].try_into().unwrap()),
            *w,
            "word {i} copied (oracle)",
        );
    }
    assert_eq!(got[1], 0, "rcx hit 0");
    eprintln!("\n==== COMPILE_REGION MEMCPY ====\nload/store self-loop copied {} words in-wasm, bit-identical (regs + dst memory).\n====\n", words.len());
}

#[test]
#[cfg(feature = "jit")]
fn region_with_cmp_and_test_conditions_matches_interpreter() {
    // Two loops whose CONDITION is a cmp/test (not the sub's flags) — the common real-code shape:
    //   add rax,rcx ; sub rbx,rdx ; <cond> ; jnz top
    let cmp_code = [0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x48, 0x39, 0xde, 0x75, 0xf5]; // cmp rbx,rsi
    let test_code = [0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x48, 0x85, 0xdb, 0x75, 0xf5]; // test rbx,rbx
    for (label, code, has_cmp) in
        [("cmp", &cmp_code[..], true), ("test", &test_code[..], false)]
    {
        let region = discover_region(code, 0, 8);
        assert_eq!(region.blocks.len(), 1, "{label}: a single self-looping block");
        // The decoded ops include the condition op (Cmp or Test).
        let ops = &region.blocks[0].ops;
        if has_cmp {
            assert!(ops.iter().any(|o| matches!(o, Op::Cmp { .. })), "cmp decoded");
        } else {
            assert!(ops.iter().any(|o| matches!(o, Op::Test { .. })), "test decoded");
        }
        let n = 12u64;
        let mut entry = [0u64; NREG];
        entry[1] = 5; // rcx
        entry[3] = n; // rbx
        entry[2] = 1; // rdx
        entry[6] = 0; // rsi (cmp operand)
        // Oracle: interpret_full the block body + the jnz condition until it exits.
        let (mut wr, mut wf, mut wram) = (entry, 0u64, alloc::vec![0u8; GUEST_LEN]);
        loop {
            interpret_full(ops, &mut wr, &mut wram, &mut wf);
            if !cc_taken(0x5, wf) {
                break;
            }
        }
        // Region: one exec runs the whole loop in wasm.
        let wasm = compile_region(&region, n + 10);
        let (regs, rflags, _d, exit) =
            crate::emulator::jit_exec::exec_region_pooled([label.as_bytes()[0]; 32], &wasm, entry, 0, |_| None)
                .expect("region runs");
        assert_eq!(regs, wr, "{label}: registers bit-identical");
        assert_eq!(rflags, wf, "{label}: rflags bit-identical (the cmp/test condition)");
        assert_eq!(regs[0], n * 5, "{label}: rax = n * rcx");
        assert_eq!(regs[3], 0, "{label}: rbx hit 0");
        let _ = exit;
    }
}

#[test]
#[cfg(feature = "jit")]
fn exec_region_pooled_fills_pages_on_miss_and_completes() {
    // A memcpy region whose source (page 2) and dest (page 5) start UNMAPPED: the region misses,
    // writes the faulting vaddr, exits; the pooled executor fetches the page + retries from entry
    // until the loop completes — the live chaining executor with lazy paging.
    let base = 0x1000u64;
    let block = RegionBlock {
        start: base,
        len: 20,
        ops: alloc::vec![
            Op::Load { d: 0, base: 6, idx: NO_REG, scale: 0, disp: 0 },
            Op::Store { base: 7, idx: NO_REG, scale: 0, disp: 0, s: 0 },
            Op::Bin { op: Bin::Add, d: 6, a: 6, b: 15 },
            Op::Bin { op: Bin::Add, d: 7, a: 7, b: 15 },
            Op::Bin { op: Bin::Sub, d: 1, a: 1, b: 14 },
        ],
        term: Terminator::Jcc { cc: 0x5, taken: base, fall: 0x9999 }, // jnz top; fall exits region
    };
    let region = Region { entry: base, blocks: alloc::vec![block.clone()] };
    let (src, dst) = (0x2000usize, 0x5000usize);
    let words: [u64; 3] = [0xaa, 0xbb, 0xcc];
    let mut entry_regs = [0u64; NREG];
    entry_regs[6] = src as u64; // rsi → page 2
    entry_regs[7] = dst as u64; // rdi → page 5
    entry_regs[1] = words.len() as u64; // rcx
    entry_regs[14] = 1;
    entry_regs[15] = 8;
    // backing guest RAM (identity paging): src data populated.
    let mut ram = alloc::vec![0u8; 0x10000];
    for (i, w) in words.iter().enumerate() {
        ram[src + i * 8..src + i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    // Oracle.
    let (mut wr, mut wf, mut oram) = (entry_regs, 0u64, ram.clone());
    while wr[1] != 0 {
        interpret_full(&block.ops, &mut wr, &mut oram, &mut wf);
    }
    // Pooled executor: empty pool → fetch pages on miss (identity translation).
    let wasm = compile_region(&region, 1000);
    let fetch = |va: u64| {
        let f = (va as usize) & !0xfff;
        (f + 0x1000 <= ram.len()).then(|| (f, ram[f..f + 0x1000].to_vec()))
    };
    let (regs, rflags, dirty, exit) =
        crate::emulator::jit_exec::exec_region_pooled([7u8; 32], &wasm, entry_regs, 0, fetch)
            .expect("region completes after the pool fills both pages");
    // Commit the dirty pages (dry mode) and check the copy landed.
    for (pa, bytes) in &dirty {
        ram[*pa..*pa + bytes.len()].copy_from_slice(bytes);
    }
    assert_eq!(regs, wr, "pooled executor: registers bit-identical");
    assert_eq!(rflags, wf, "pooled executor: rflags bit-identical");
    assert_eq!(exit, 0x9999, "exited at the region's out-of-region target");
    for (i, w) in words.iter().enumerate() {
        assert_eq!(
            u64::from_le_bytes(ram[dst + i * 8..dst + i * 8 + 8].try_into().unwrap()),
            *w,
            "word {i} copied (lazily-paged through the pool)",
        );
    }
    eprintln!("\n==== EXEC_REGION_POOLED ====\nmemcpy region completed via lazy page-fill ({} dirty pages), bit-identical, exit 0x{:x}.\n====\n", dirty.len(), exit);
}

#[test]
#[cfg(feature = "jit")] // exec_region lives in the `jit`-feature executor module
fn exec_region_drives_a_region_over_the_executor() {
    // The live executor (`jit_exec::exec_region`) runs a memcpy region over architectural state with
    // identity paging (vaddr=pa; identity TLB) — the G3a proof that the executor marshals correctly.
    let base = 0x1000u64;
    let block = RegionBlock {
        start: base,
        len: 20,
        ops: alloc::vec![
            Op::Load { d: 0, base: 6, idx: NO_REG, scale: 0, disp: 0 },
            Op::Store { base: 7, idx: NO_REG, scale: 0, disp: 0, s: 0 },
            Op::Bin { op: Bin::Add, d: 6, a: 6, b: 15 },
            Op::Bin { op: Bin::Add, d: 7, a: 7, b: 15 },
            Op::Bin { op: Bin::Sub, d: 1, a: 1, b: 14 },
        ],
        term: Terminator::Jcc { cc: 0x5, taken: base, fall: base + 0x1000 },
    };
    let region = Region { entry: base, blocks: alloc::vec![block.clone()] };
    let (src, dst) = (0x100usize, 0x800usize);
    let words: [u64; 4] = [0xa1, 0xb2, 0xc3, 0xd4];
    let mut regs = [0u64; NREG];
    regs[6] = src as u64;
    regs[7] = dst as u64;
    regs[1] = words.len() as u64;
    regs[14] = 1;
    regs[15] = 8;
    // Oracle.
    let (mut wr, mut wf) = (regs, 0u64);
    let mut oram = alloc::vec![0u8; GUEST_LEN];
    for (i, w) in words.iter().enumerate() {
        oram[src + i * 8..src + i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    while wr[1] != 0 {
        interpret_full(&block.ops, &mut wr, &mut oram, &mut wf);
    }
    // Live executor: identity TLB (page 0), guest RAM carries the src data.
    let wasm = compile_region(&region, 100);
    let mut ram = alloc::vec![0u8; GUEST_LEN];
    for (i, w) in words.iter().enumerate() {
        ram[src + i * 8..src + i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let mut tlb = alloc::vec![0u8; TLB_SIZE as usize * 16];
    tlb[0..8].copy_from_slice(&0u64.to_le_bytes()); // page 0: tag=0
    tlb[8..16].copy_from_slice(&0u64.to_le_bytes()); // host_off=0
    let mut rflags = 0u64;
    let exit = crate::emulator::jit_exec::exec_region(&wasm, &mut regs, &mut rflags, &mut ram, &tlb);
    assert_eq!(regs, wr, "executor: registers bit-identical to the interpreter");
    assert_eq!(rflags, wf, "executor: rflags bit-identical");
    assert_eq!(exit, base + 0x1000, "executor: exit rip = the region's out-of-region target");
    for (i, w) in words.iter().enumerate() {
        assert_eq!(
            u64::from_le_bytes(ram[dst + i * 8..dst + i * 8 + 8].try_into().unwrap()),
            *w,
            "word {i} copied to dst through the executor",
        );
    }
    eprintln!("\n==== EXEC_REGION ====\nthe live executor ran a memcpy region over architectural state, bit-identical (regs+rflags+dst), exit 0x{:x}.\n====\n", exit);
}

#[test]
fn compile_region_tlb_miss_exits_at_faulting_block() {
    // A region whose load hits an UNMAPPED page must exit at the faulting block's rip (so the
    // interpreter faults the page in and the dispatcher re-enters) — the live-paging mechanism.
    let base = 0x1000u64;
    let block = RegionBlock {
        start: base,
        len: 8,
        ops: alloc::vec![Op::Load { d: 0, base: 6, idx: NO_REG, scale: 0, disp: 0 }],
        term: Terminator::Jmp { target: base }, // self-loop (never reached — the load misses first)
    };
    let region = Region { entry: base, blocks: alloc::vec![block] };
    let mut regs = [0u64; NREG];
    regs[0] = 0xdead; // rax sentinel — must be unchanged (load never completes)
    regs[6] = 0x5000; // rsi → vpage 5; the all-zero TLB has no entry (tag 0 ≠ 5) → miss
    let wasm = compile_region(&region, 1000);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, TLB_BASE as usize, &alloc::vec![0u8; TLB_SIZE as usize * 16]).unwrap();
    let exit = {
        run.call(&mut store, ()).unwrap();
        let mut eb = [0u8; 8];
        mem.read(&store, EXIT_RIP_MEM as usize, &mut eb).unwrap();
        u64::from_le_bytes(eb)
    };
    let mut rax = [0u8; 8];
    mem.read(&store, 0, &mut rax).unwrap();
    assert_eq!(exit, base, "TLB miss exits at the faulting block's start rip");
    assert_eq!(u64::from_le_bytes(rax), 0xdead, "rax untouched — the miss exited before the load");
}

#[test]
fn compile_region_runs_a_two_block_loop_bit_identical() {
    // The 2-block loop:  A: add rax,rbx ; jmp B   B: sub rcx,rdx ; jne A  (fall 0x100a exits)
    let base = 0x1000u64;
    let code = [
        0x48, 0x01, 0xd8, 0xeb, 0x00, // A: add rax,rbx ; jmp B(0x1005)
        0x48, 0x29, 0xd1, 0x75, 0xf6, // B: sub rcx,rdx ; jne A(0x1000)
    ];
    let region = discover_region(&code, base, 16);
    assert_eq!(region.blocks.len(), 2);
    let max = 5000u64;
    let mut regs = [0u64; NREG];
    regs[0] = 0; // rax
    regs[3] = 7; // rbx
    regs[1] = 1000; // rcx (counter)
    regs[2] = 1; // rdx
    // Oracle: interpret block-by-block exactly as the dispatcher does.
    let (mut wr, mut wflags, mut ram) = (regs, 0u64, alloc::vec![0u8; GUEST_LEN]);
    let mut pc = 0usize;
    let mut iters = 0u64;
    let exit_rip;
    loop {
        let blk = &region.blocks[pc];
        iters += 1;
        if iters >= max {
            exit_rip = blk.start;
            break;
        }
        interpret_full(&blk.ops, &mut wr, &mut ram, &mut wflags);
        let next = match blk.term {
            Terminator::Jmp { target } => target,
            Terminator::Jcc { cc, taken, fall } => {
                if cc_taken(cc, wflags) {
                    taken
                } else {
                    fall
                }
            }
            Terminator::Exit => {
                exit_rip = blk.start + blk.len as u64;
                break;
            }
        };
        match region.index_of(next) {
            Some(j) => pc = j,
            None => {
                exit_rip = next;
                break;
            }
        }
    }
    // JIT: one wasm call runs the whole loop in-wasm.
    let wasm = compile_region(&region, max);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("compile_region wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &0u64.to_le_bytes()).unwrap();
    let got_exit = {
        run.call(&mut store, ()).unwrap();
        let mut eb = [0u8; 8];
        mem.read(&store, EXIT_RIP_MEM as usize, &mut eb).unwrap();
        u64::from_le_bytes(eb)
    };
    let mut got = [0u64; NREG];
    for (i, o) in got.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    let mut fb = [0u8; 8];
    mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
    let got_flags = u64::from_le_bytes(fb);
    assert_eq!(got, wr, "registers bit-identical after the chained region");
    assert_eq!(got_flags, wflags, "rflags bit-identical");
    assert_eq!(got_exit, exit_rip, "exit rip matches the oracle");
    assert_eq!(got_exit, 0x100a, "exited at B's fall-through (rcx hit 0)");
    assert_eq!(got[0], 1000 * 7, "rax = N * rbx");
    assert_eq!(got[1], 0, "rcx hit 0");
    eprintln!(
        "\n==== COMPILE_REGION ====\n2-block loop (A:add;jmp B  B:sub;jne A) ran as ONE wasm region, \
         {} blocks, bit-identical (regs+rflags+exit rip 0x{:x}). br_table dispatcher works.\n====\n",
        region.blocks.len(), got_exit,
    );
}

#[test]
fn compile_region_immediate_counted_loop_bit_identical() {
    // A real immediate-driven counted loop in ONE block (the shape `lea`/reg loops can't express):
    //   top: add rax, 1 ; cmp rax, 100 ; jne top   (loops until rax == 100, then falls through)
    let base = 0x2000u64;
    let code = [
        0x48, 0x83, 0xc0, 0x01, // add rax, 1      (BinImm Add imm8)
        0x48, 0x83, 0xf8, 0x64, // cmp rax, 100    (CmpImm imm8)
        0x75, 0xf6, //             jne top (rel -10 → back to base)
    ];
    let region = discover_region(&code, base, 16);
    assert_eq!(region.blocks.len(), 1, "a self-looping single block");
    assert_eq!(
        region.blocks[0].ops,
        vec![Op::BinImm { op: Bin::Add, d: 0, imm: 1 }, Op::CmpImm { a: 0, imm: 100 }],
        "the loop body decoded fully (no early uncovered-op exit)",
    );
    let max = 100_000u64;
    // Oracle: interpret the block until the back-edge stops being taken (rax == 100).
    let (mut wr, mut wflags, mut ram) = ([0u64; NREG], 0u64, alloc::vec![0u8; GUEST_LEN]);
    let exit_rip;
    let mut iters = 0u64;
    loop {
        iters += 1;
        if iters >= max {
            exit_rip = base;
            break;
        }
        interpret_full(&region.blocks[0].ops, &mut wr, &mut ram, &mut wflags);
        match region.blocks[0].term {
            Terminator::Jcc { cc, taken, fall } => {
                if cc_taken(cc, wflags) {
                    if taken != base {
                        exit_rip = taken;
                        break;
                    }
                } else {
                    exit_rip = fall;
                    break;
                }
            }
            _ => unreachable!(),
        }
    }
    // JIT: one wasm call runs the whole loop.
    let wasm = compile_region(&region, max);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("compile_region wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    mem.write(&mut store, RFLAGS_MEM as usize, &0u64.to_le_bytes()).unwrap();
    run.call(&mut store, ()).unwrap();
    let mut got = [0u64; NREG];
    for (i, o) in got.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    let mut eb = [0u8; 8];
    mem.read(&store, EXIT_RIP_MEM as usize, &mut eb).unwrap();
    let got_exit = u64::from_le_bytes(eb);
    let mut fb = [0u8; 8];
    mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
    assert_eq!(got, wr, "registers bit-identical after the immediate-driven region");
    assert_eq!(u64::from_le_bytes(fb), wflags, "rflags bit-identical");
    assert_eq!(got_exit, exit_rip, "exit rip matches the oracle");
    assert_eq!(got[0], 100, "rax counted up to the immediate bound");
    assert_eq!(got_exit, base + code.len() as u64, "exited at the loop's fall-through");
}

#[test]
fn x86_decoder_maps_push_pop_to_the_ir() {
    // push rax (50) ; add rax,rbx (48 01 d8) — push then a covered op
    assert_eq!(
        decode_x86(&[0x50, 0x48, 0x01, 0xd8]),
        vec![Op::Push { s: 0 }, Op::Bin { op: Bin::Add, d: 0, a: 0, b: 3 }]
    );
    // push rbx (53) ; pop rcx (59) ; pop rdx (5a) — (trailing nop 0x90 so the last byte decodes)
    assert_eq!(
        decode_x86(&[0x53, 0x59, 0x5a, 0x90]),
        vec![Op::Push { s: 3 }, Op::Pop { d: 1 }, Op::Pop { d: 2 }]
    );
    // push r8 (41 50) — REX.B extends to r8 ; pop rsp (5c)
    assert_eq!(decode_x86(&[0x41, 0x50, 0x90]), vec![Op::Push { s: 8 }]);
    assert_eq!(decode_x86(&[0x5c, 0x90]), vec![Op::Pop { d: 4 }]);
}

#[test]
fn compile_region_push_pop_roundtrip_bit_identical() {
    // A real prologue-shaped straight-line region decoded from BYTES, with a paged stack:
    //   push rax ; push rbx ; pop rcx ; pop rdx ; ret   → rcx=rbx, rdx=rax, rsp restored.
    let base = 0x1000u64;
    let code = [0x50, 0x53, 0x59, 0x5a, 0xc3];
    let region = discover_region(&code, base, 16);
    assert_eq!(region.blocks.len(), 1);
    assert_eq!(
        region.blocks[0].ops,
        vec![Op::Push { s: 0 }, Op::Push { s: 3 }, Op::Pop { d: 1 }, Op::Pop { d: 2 }],
    );
    assert_eq!(region.blocks[0].term, Terminator::Exit);
    let (rax, rbx) = (0xAAAA_AAAA_AAAA_AAAAu64, 0xBBBB_BBBB_BBBB_BBBBu64);
    let mut regs = [0u64; NREG];
    regs[0] = rax;
    regs[3] = rbx;
    regs[RSP as usize] = 0x800; // stack pointer inside guest page 0 (grows down to 0x7f0)
    // Oracle.
    let (mut wr, mut wf) = (regs, 0u64);
    let mut oram = alloc::vec![0u8; GUEST_LEN];
    interpret_full(&region.blocks[0].ops, &mut wr, &mut oram, &mut wf);
    // JIT region with an identity TLB for page 0 (the stack lives there).
    let wasm = compile_region(&region, 100);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("push/pop region wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &0u64.to_le_bytes()).unwrap();
    let mut tlb = alloc::vec![0u8; TLB_SIZE as usize * 16];
    tlb[0..8].copy_from_slice(&0u64.to_le_bytes()); // slot 0 tag = vpage 0
    tlb[8..16].copy_from_slice(&0u64.to_le_bytes()); // host_off = 0 → host = GUEST_BASE + va
    mem.write(&mut store, TLB_BASE as usize, &tlb).unwrap();
    run.call(&mut store, ()).unwrap();
    let mut got = [0u64; NREG];
    for (i, o) in got.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    assert_eq!(got, wr, "registers bit-identical after the push/pop region");
    assert_eq!(got[1], rbx, "pop rcx got the second push (rbx)");
    assert_eq!(got[2], rax, "pop rdx got the first push (rax)");
    assert_eq!(got[RSP as usize], 0x800, "rsp restored after balanced push/pop");
}

#[test]
fn compile_region_runs_a_branchy_diamond_bit_identical() {
    // A diamond: A conditionally goes to C (then) or B (else); both re-merge at D, which exits.
    //   A@1000: sub rcx,rdx ; je C        (48 29 d1 | 74 05)   taken→C(100a), fall→B(1005)
    //   B@1005: add rax,rbx ; jmp D       (48 01 d8 | eb 05)
    //   C@100a: add rax,rsi ; jmp D       (48 01 f0 | eb 00)
    //   D@100f: xor rdx,rdx ; ret         (48 31 d2 | c3)      → Exit, resume 0x1012
    let base = 0x1000u64;
    let code = [
        0x48, 0x29, 0xd1, 0x74, 0x05, // A
        0x48, 0x01, 0xd8, 0xeb, 0x05, // B
        0x48, 0x01, 0xf0, 0xeb, 0x00, // C
        0x48, 0x31, 0xd2, 0xc3, // D
    ];
    let region = discover_region(&code, base, 16);
    assert_eq!(region.blocks.len(), 4, "A, B, C, D all discovered");
    let max = 1000u64;
    let run_oracle = |regs: [u64; NREG]| -> ([u64; NREG], u64, u64) {
        let (mut wr, mut wf, mut ram) = (regs, 0u64, alloc::vec![0u8; GUEST_LEN]);
        let (mut pc, mut iters) = (0usize, 0u64);
        loop {
            let blk = &region.blocks[pc];
            iters += 1;
            if iters >= max {
                return (wr, wf, blk.start);
            }
            interpret_full(&blk.ops, &mut wr, &mut ram, &mut wf);
            let next = match blk.term {
                Terminator::Jmp { target } => target,
                Terminator::Jcc { cc, taken, fall } => {
                    if cc_taken(cc, wf) {
                        taken
                    } else {
                        fall
                    }
                }
                Terminator::Exit => return (wr, wf, blk.start + blk.len as u64),
            };
            match region.index_of(next) {
                Some(j) => pc = j,
                None => return (wr, wf, next),
            }
        }
    };
    let wasm = compile_region(&region, max);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("diamond region wasm is valid");
    // Run BOTH branches: rcx==rdx → take C (add rsi); rcx!=rdx → fall to B (add rbx).
    for (rcx, rdx, label) in [(5u64, 5u64, "then/C"), (5, 3, "else/B")] {
        let mut regs = [0u64; NREG];
        regs[1] = rcx;
        regs[2] = rdx;
        regs[3] = 100; // rbx
        regs[6] = 7; // rsi
        let (wr, wf, wexit) = run_oracle(regs);
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let mem = instance.get_memory(&mut store, "mem").unwrap();
        let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
        for (i, v) in regs.iter().enumerate() {
            mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
        }
        mem.write(&mut store, RFLAGS_MEM as usize, &0u64.to_le_bytes()).unwrap();
        let exit = {
        run.call(&mut store, ()).unwrap();
        let mut eb = [0u8; 8];
        mem.read(&store, EXIT_RIP_MEM as usize, &mut eb).unwrap();
        u64::from_le_bytes(eb)
    };
        let mut got = [0u64; NREG];
        for (i, o) in got.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            mem.read(&store, i * 8, &mut b).unwrap();
            *o = u64::from_le_bytes(b);
        }
        let mut fb = [0u8; 8];
        mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
        assert_eq!(got, wr, "{label}: regs bit-identical");
        assert_eq!(u64::from_le_bytes(fb), wf, "{label}: rflags bit-identical");
        assert_eq!(exit, wexit, "{label}: exit rip matches");
        assert_eq!(exit, 0x1012, "{label}: exits after D");
        assert_eq!(got[2], 0, "{label}: rdx xored to 0 in D");
    }
    eprintln!("\n==== COMPILE_REGION DIAMOND ====\n4-block diamond (jcc → both in-region), both branches bit-identical.\n====\n");
}

#[test]
fn region_discovery_links_blocks_by_branch_targets() {
    // A two-block loop at base 0x1000:
    //   A@0x1000: add rax,rbx ; jmp B        (48 01 d8 | eb 00)            len 5 → B@0x1005
    //   B@0x1005: sub rcx,rdx ; jne A         (48 29 d1 | 75 f6)           len 5 → A@0x1000 / fall 0x100a
    let base = 0x1000u64;
    let code = [
        0x48, 0x01, 0xd8, 0xeb, 0x00, // A: add rax,rbx ; jmp +0 → 0x1005 (B, the next instruction)
        0x48, 0x29, 0xd1, 0x75, 0xf6, // B: sub rcx,rdx ; jne -10 → 0x1000 (A)
    ];
    let region = discover_region(&code, base, 16);
    assert_eq!(region.entry, base);
    assert_eq!(region.blocks.len(), 2, "found both blocks of the loop");
    assert_eq!(region.blocks[0].start, base, "entry block is first");
    let a = &region.blocks[region.index_of(0x1000).unwrap()];
    let b = &region.blocks[region.index_of(0x1005).unwrap()];
    assert_eq!(a.term, Terminator::Jmp { target: 0x1005 });
    assert_eq!(b.term, Terminator::Jcc { cc: 5, taken: 0x1000, fall: 0x100a });
    // Both targets of B resolve: the back-edge is in-region, the fall-through is out (→ exit).
    assert!(region.index_of(0x1000).is_some(), "back-edge target in region");
    assert!(region.index_of(0x100a).is_none(), "fall-through is out of the image → run-time exit");
}

#[test]
fn jcc_loop_chains_in_wasm_bit_identical() {
    // The real x86 loop: `add rax,rbx ; sub rcx,rdx ; jnz top` — loop while rcx != 0 (ZF clear).
    let body = [
        Op::Bin { op: Bin::Add, d: 0, a: 0, b: 3 }, // rax += rbx
        Op::Bin { op: Bin::Sub, d: 1, a: 1, b: 2 }, // rcx -= rdx (sets ZF when rcx hits 0)
    ];
    let cc_jnz = 0x5u8; // jnz: cc>>1=2 (ZF), cc&1=1 → !ZF → taken while rcx != 0
    let n = 500_000u64;
    let mut regs = [0u64; NREG];
    regs[1] = n; // rcx = N
    regs[2] = 1; // rdx = 1
    regs[3] = 9; // rbx
    // Oracle: interpret_full (flags-aware) + cc_taken, exactly like the in-wasm loop.
    let mut want = regs;
    let mut ram = vec![0u8; GUEST_LEN];
    let mut rflags = 0u64;
    let mut oracle_iters = 0u64;
    loop {
        interpret_full(&body, &mut want, &mut ram, &mut rflags);
        oracle_iters += 1;
        if oracle_iters >= n + 10 || !cc_taken(cc_jnz, rflags) {
            break;
        }
    }
    // JIT: one wasm call, regs+rflags persisting across all iterations.
    let wasm = compile_jcc_loop(&body, cc_jnz, n + 10);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("jcc-loop wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &0u64.to_le_bytes()).unwrap();
    let iters = run.call(&mut store, ()).unwrap() as u64;
    let mut got = [0u64; NREG];
    for (i, o) in got.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    let mut fb = [0u8; 8];
    mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
    let got_rflags = u64::from_le_bytes(fb);
    assert_eq!(iters, oracle_iters, "iteration count matches");
    assert_eq!(got, want, "final registers bit-identical");
    assert_eq!(got_rflags, rflags, "final rflags bit-identical");
    assert_eq!(got[1], 0, "rcx hit 0");
    assert_eq!(got[0], n * 9, "rax = N * rbx");
    assert_eq!(iters, n, "looped exactly N times");
    eprintln!(
        "\n==== JCC LOOP ({n} iters) ====\nreal x86 `add;sub;jnz` ran as ONE wasm region — regs+rflags persist, \
         bit-identical (regs+rflags), {n} iters. emit_cc + emit_flags composed in a live loop.\n====\n",
    );
}

#[test]
fn counted_loop_chains_in_wasm_bit_identical() {
    use std::time::Instant;
    // body: rax += rbx ; rcx -= r15(=1)  → loop while rcx != 0 (a classic counted loop).
    let body = [
        Op::Bin { op: Bin::Add, d: 0, a: 0, b: 3 }, // rax += rbx
        Op::Bin { op: Bin::Sub, d: 1, a: 1, b: 15 }, // rcx -= 1
    ];
    let n = 1_000_000u64;
    let mut regs = [0u64; NREG];
    regs[1] = n; // rcx = N (the counter)
    regs[3] = 7; // rbx
    regs[15] = 1; // decrement
    // Oracle: interpret the body until rcx hits 0.
    let mut want = regs;
    let mut ram = vec![0u8; GUEST_LEN];
    let mut oracle_iters = 0u64;
    while want[1] != 0 && oracle_iters < n + 10 {
        interpret(&body, &mut want, &mut ram);
        oracle_iters += 1;
    }
    // JIT: ONE wasm call runs all N iterations in-wasm, regs persisting in locals across them.
    let wasm = compile_counted_loop(&body, 1, n + 10);
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm).expect("counted-loop wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    let t = Instant::now();
    let iters = run.call(&mut store, ()).unwrap() as u64;
    let jit_ns = t.elapsed().as_nanos().max(1);
    let mut got = [0u64; NREG];
    for (i, o) in got.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    assert_eq!(iters, oracle_iters, "iteration count matches the interpreter");
    assert_eq!(got, want, "final registers bit-identical to the interpreter");
    assert_eq!(got[0], n * 7, "rax = N * rbx");
    assert_eq!(got[1], 0, "rcx decremented to 0");
    // Speedup vs interpreting the body N times (the per-iteration path the chaining eliminates).
    let mut r2 = regs;
    let mut ram2 = vec![0u8; GUEST_LEN];
    let t = Instant::now();
    for _ in 0..n {
        interpret(&body, &mut r2, &mut ram2);
    }
    let interp_ns = t.elapsed().as_nanos().max(1);
    eprintln!(
        "\n==== CHAINED LOOP ({n} iters, regs persist in wasm) ====\n\
         JIT (1 wasm call): {} µs   |   IR-interpret ×N: {} µs   → {:.1}x\n\
         (vs the REAL x86 Cpu::step at ~45 ns/op × 2 ops/iter the win is far larger; this is the\n\
          25× stay-in-wasm path proven: NO per-iteration marshalling.)\n====\n",
        jit_ns / 1000, interp_ns / 1000, interp_ns as f64 / jit_ns as f64,
    );
}

#[test]
fn lea_decodes_and_runs_bit_identical() {
    // lea rdx, [rax + 0x8]  (48 8d 50 08)
    assert_eq!(
        decode_x86(&[0x48, 0x8d, 0x50, 0x08]),
        vec![Op::Lea { d: 2, base: 0, idx: NO_REG, scale: 0, disp: 8 }],
    );
    // lea rax, [rbx + rcx*4 + 0x10]  (48 8d 44 8b 10)
    assert_eq!(
        decode_x86(&[0x48, 0x8d, 0x44, 0x8b, 0x10]),
        vec![Op::Lea { d: 0, base: 3, idx: 1, scale: 2, disp: 0x10 }],
    );
    // A block mixing lea with ALU runs bit-identically through the JIT and the interpreter.
    let block = decode_x86(&[
        0x48, 0x8d, 0x44, 0x8b, 0x10, // lea rax, [rbx + rcx*4 + 0x10]
        0x48, 0x01, 0xd0, // add rax, rdx
        0x48, 0x8d, 0x14, 0x00, // lea rdx, [rax + rax*1]  (= 2*rax)
    ]);
    assert_eq!(block.len(), 3, "all three decoded (lea no longer bails)");
    let mut regs = [0u64; NREG];
    regs[0] = 0x1111;
    regs[1] = 0x2000;
    regs[2] = 0x30;
    regs[3] = 0x7;
    let mut want = regs;
    let mut ram = vec![0u8; GUEST_LEN];
    interpret(&block, &mut want, &mut ram);
    assert_eq!(run_wasm(&compile(&block), regs, &mut ram), want, "lea block JIT ≡ interpret");
}

#[test]
fn decoded_x86_runs_through_the_jit_and_matches() {
    // mov rcx,rax ; add rcx,rbx ; xor rcx,rdx  → rcx = (rax + rbx) ^ rdx
    let bytes = [
        0x48, 0x89, 0xc1, // mov rcx, rax
        0x48, 0x01, 0xd9, // add rcx, rbx
        0x48, 0x31, 0xd1, // xor rcx, rdx
    ];
    let ir = decode_x86(&bytes);
    let mut regs = [0u64; NREG];
    regs[0] = 0x0102_0304_0506_0708; // rax
    regs[3] = 0x1111_1111_1111_1111; // rbx
    regs[2] = 0xffff_0000_ffff_0000; // rdx
    let mut want = regs;
    let mut ram = vec![0u8; GUEST_LEN];
    interpret(&ir, &mut want, &mut ram);
    // hand-check rcx (reg 1) and confirm the JIT (codegen→wasmtime) agrees end-to-end.
    assert_eq!(want[1], regs[0].wrapping_add(regs[3]) ^ regs[2]);
    assert_eq!(run_wasm(&compile(&ir), regs, &mut ram), want, "real x86 bytes → decode → JIT diverged");
}

/// Memory ops differential: random blocks mixing reg-ALU with Load/Store through base
/// pointers, interpret vs codegen→wasmtime over a shared guest RAM = bit-identical (regs
/// AND RAM). This is the `W[]`-schedule shape — the JIT touching guest memory, not just
/// registers, which is where the real (memory-bound) boot win lives.
#[test]
fn jit_memory_ops_are_bit_identical_to_the_interpreter() {
    let mut rng = Rng(0xd1b5_4a32_d192_ed03);
    // reg 11 = SIB index, regs 12..16 = base pointers — never written by the block.
    const BASE: [u8; 4] = [12, 13, 14, 15];
    for _ in 0..300 {
        let n = 1 + (rng.next() % 30);
        let mut block = Vec::new();
        for _ in 0..n {
            let d = (rng.next() % 11) as u8; // dst regs 0..11 only (keep base/index intact)
            let base = BASE[(rng.next() % 4) as usize];
            let scale = (rng.next() % 4) as u8; // 1,2,4,8
            let idx = if rng.next() & 1 == 0 { NO_REG } else { 11 }; // exercise both SIB & plain
            let disp = (rng.next() % 0x400) as i32; // in-range offset
            block.push(match rng.next() % 8 {
                0 => Op::Movi { d, imm: rng.next() },
                1 => Op::Bin { op: Bin::Add, d, a: rng.reg(), b: rng.reg() },
                2 => Op::Bin { op: Bin::Xor, d, a: rng.reg(), b: rng.reg() },
                3 => Op::Shift { op: Sh::Rotr, d, a: rng.reg(), sh: (rng.next() % 64) as u8 },
                4 => Op::Load { d, base, idx, scale, disp },
                5 => Op::Store { base, idx, scale, disp, s: rng.reg() },
                6 => Op::LoadOp { op: Bin::Add, d, base, idx, scale, disp },
                _ => Op::LoadOp { op: Bin::Xor, d, base, idx, scale, disp },
            });
        }
        // base/index spaced so [addr, +8) stays inside the test RAM (max ≈ 0x1700).
        let mut regs = [0u64; NREG];
        for r in regs[..11].iter_mut() {
            *r = rng.next();
        }
        regs[11] = 0x20; // index value (small; idx<<scale ≤ 0x100)
        regs[12] = 0x0000;
        regs[13] = 0x0600;
        regs[14] = 0x0c00;
        regs[15] = 0x1200;
        // random initial guest RAM
        let mut ram0 = vec![0u8; GUEST_LEN];
        for b in ram0.iter_mut() {
            *b = (rng.next() & 0xff) as u8;
        }
        let (mut regs_i, mut ram_i) = (regs, ram0.clone());
        interpret(&block, &mut regs_i, &mut ram_i);
        let mut ram_w = ram0;
        let regs_w = run_wasm(&compile(&block), regs, &mut ram_w);
        assert_eq!(regs_w, regs_i, "memory-op block: regs diverged");
        assert_eq!(ram_w, ram_i, "memory-op block: guest RAM diverged");
    }
}

#[test]
fn decoder_handles_x86_memory_forms() {
    // mov rax, [rsp+0x10]  (48 8b 44 24 10): rsp forces a SIB, disp8
    assert_eq!(
        decode_x86(&[0x48, 0x8b, 0x44, 0x24, 0x10]),
        vec![Op::Load { d: 0, base: 4, idx: NO_REG, scale: 0, disp: 0x10 }]
    );
    // mov [rsp+0x8], rbx  (48 89 5c 24 08)
    assert_eq!(
        decode_x86(&[0x48, 0x89, 0x5c, 0x24, 0x08]),
        vec![Op::Store { base: 4, idx: NO_REG, scale: 0, disp: 8, s: 3 }]
    );
    // add r15, [r13*8 + 0x100]  (4e 03 3c ed 00 01 00 00): SIB no-base table load — the
    // K[] round-constant shape (index=r13 via REX.X, scale=8, disp32, no base register)
    assert_eq!(
        decode_x86(&[0x4e, 0x03, 0x3c, 0xed, 0x00, 0x01, 0x00, 0x00]),
        vec![Op::LoadOp { op: Bin::Add, d: 15, base: NO_REG, idx: 13, scale: 3, disp: 0x100 }]
    );
}

#[test]
fn decoded_x86_memory_block_runs_through_the_jit() {
    // mov rax,[rsp+0x10] ; add rax,[rsp+0x20] ; mov [rsp+0x100],rax  (the W[] shape:
    // load two schedule words, sum, store back) — decoded from real bytes, run end-to-end.
    let bytes = [
        0x48, 0x8b, 0x44, 0x24, 0x10, // mov rax, [rsp+0x10]
        0x48, 0x03, 0x44, 0x24, 0x20, // add rax, [rsp+0x20]
        0x48, 0x89, 0x84, 0x24, 0x00, 0x01, 0x00, 0x00, // mov [rsp+0x100], rax (disp32)
    ];
    let ir = decode_x86(&bytes);
    assert_eq!(ir.len(), 3, "all three memory instructions decoded");
    let mut regs = [0u64; NREG];
    regs[4] = 0; // rsp → guest RAM base 0
    let mut ram0 = vec![0u8; GUEST_LEN];
    ram0[0x10..0x18].copy_from_slice(&0x1111_2222_3333_4444u64.to_le_bytes());
    ram0[0x20..0x28].copy_from_slice(&0x0000_0000_0001_0001u64.to_le_bytes());
    let (mut regs_i, mut ram_i) = (regs, ram0.clone());
    interpret(&ir, &mut regs_i, &mut ram_i);
    let mut ram_w = ram0;
    let regs_w = run_wasm(&compile(&ir), regs, &mut ram_w);
    assert_eq!(regs_w, regs_i, "decoded memory block: regs diverged");
    assert_eq!(ram_w, ram_i, "decoded memory block: guest RAM diverged");
    let stored = u64::from_le_bytes(ram_i[0x100..0x108].try_into().unwrap());
    assert_eq!(stored, 0x1111_2222_3333_4444u64.wrapping_add(0x0001_0001), "summed-and-stored");
}

/// Inline software-TLB differential: guest *virtual* addresses are translated through a
/// modeled TLB whose entries are a non-trivial page permutation, so a codegen that ignored
/// the TLB (used the vaddr directly) would diverge. Oracle = `interpret` over the flat
/// virtual RAM; JIT = `compile_tlb` translating over the permuted host RAM. Bit-identical
/// (regs AND every guest page under the permutation) proves the inline-TLB hit path — the
/// address translation the real paged boot depends on.
#[test]
fn jit_inline_tlb_translation_is_bit_identical() {
    let mut rng = Rng(0x51ed_270b_7c4f_a1c9);
    const PERM: [usize; 3] = [2, 0, 1]; // virtual page p → host page PERM[p]
    const BASES: [u8; 3] = [13, 14, 15]; // page-aligned base pointers, never written
    for _ in 0..300 {
        let n = 1 + (rng.next() % 24);
        let mut block = Vec::new();
        for _ in 0..n {
            let d = (rng.next() % 13) as u8;
            let base = BASES[(rng.next() % 3) as usize];
            let disp = ((rng.next() % 0x200) * 8) as i32; // 8-aligned, ≤ 0xff8 (within a page)
            block.push(match rng.next() % 6 {
                0 => Op::Movi { d, imm: rng.next() },
                1 => Op::Bin { op: Bin::Add, d, a: rng.reg(), b: rng.reg() },
                2 => Op::Shift { op: Sh::Rotr, d, a: rng.reg(), sh: (rng.next() % 64) as u8 },
                3 => Op::Load { d, base, idx: NO_REG, scale: 0, disp },
                4 => Op::Store { base, idx: NO_REG, scale: 0, disp, s: rng.reg() },
                _ => Op::LoadOp { op: Bin::Xor, d, base, idx: NO_REG, scale: 0, disp },
            });
        }
        let mut regs = [0u64; NREG];
        for r in regs[..13].iter_mut() {
            *r = rng.next();
        }
        regs[13] = 0x0000;
        regs[14] = 0x1000;
        regs[15] = 0x2000;
        // virtual RAM (oracle view); host RAM = each virtual page placed at its PERM page.
        let mut virt = vec![0u8; GUEST_LEN];
        for b in virt.iter_mut() {
            *b = (rng.next() & 0xff) as u8;
        }
        let mut host = vec![0u8; GUEST_LEN];
        for p in 0..3 {
            host[PERM[p] * 0x1000..PERM[p] * 0x1000 + 0x1000]
                .copy_from_slice(&virt[p * 0x1000..p * 0x1000 + 0x1000]);
        }
        // TLB image: entry p → (tag=p @0, host_off=PERM[p]*0x1000 @8)
        let mut tlb = vec![0u8; TLB_SIZE as usize * 16];
        for p in 0..3 {
            tlb[p * 16..p * 16 + 8].copy_from_slice(&(p as u64).to_le_bytes());
            tlb[p * 16 + 8..p * 16 + 16].copy_from_slice(&((PERM[p] as u64) * 0x1000).to_le_bytes());
        }
        let (mut regs_i, mut virt_i) = (regs, virt.clone());
        interpret(&block, &mut regs_i, &mut virt_i); // flat vaddr indexing — no translation
        let (regs_w, bail) = run_wasm_tlb(&compile_tlb(&block), regs, &mut host, &tlb);
        assert_eq!(bail as usize, block.len(), "all pages present — block must complete");
        assert_eq!(regs_w, regs_i, "inline-TLB block: regs diverged");
        for p in 0..3 {
            assert_eq!(
                &host[PERM[p] * 0x1000..PERM[p] * 0x1000 + 0x1000],
                &virt_i[p * 0x1000..p * 0x1000 + 0x1000],
                "inline-TLB block: guest page {p} diverged after translation"
            );
        }
    }
}

/// Inline-TLB **miss/bail**: when an access hits a page absent from the TLB, the block must
/// stop at that instruction, store back clean architectural state (exactly "before op k"),
/// and return `k` — what the interpreter resumes from (it fills the TLB via the #PF path,
/// then re-dispatches). Identity mapping (host == virtual) keeps the focus on the bail.
#[test]
fn jit_inline_tlb_bail_is_correct() {
    // page 0 & 2 present in the TLB; page 1 ABSENT (tag mismatch → miss).
    let mut tlb = vec![0u8; TLB_SIZE as usize * 16];
    for &p in &[0usize, 2] {
        tlb[p * 16..p * 16 + 8].copy_from_slice(&(p as u64).to_le_bytes());
        tlb[p * 16 + 8..p * 16 + 16].copy_from_slice(&((p as u64) * 0x1000).to_le_bytes());
    }
    tlb[1 * 16..1 * 16 + 8].copy_from_slice(&0xDEADu64.to_le_bytes()); // wrong tag for slot 1

    let block = [
        Op::Movi { d: 0, imm: 0x1234 },                                  // op0: reg
        Op::Store { base: 13, idx: NO_REG, scale: 0, disp: 0x10, s: 0 }, // op1: page 0 (present)
        Op::Load { d: 1, base: 14, idx: NO_REG, scale: 0, disp: 0x20 },  // op2: page 1 → BAIL
        Op::Movi { d: 2, imm: 0x9999 },                                  // op3: never runs
    ];
    let mut regs = [0u64; NREG];
    regs[1] = 0x5555; // must survive untouched (op2 bailed before writing it)
    regs[2] = 0x6666; // must survive untouched (op3 never ran)
    regs[13] = 0x0000; // page 0 base
    regs[14] = 0x1000; // page 1 base

    let mut virt = vec![0u8; GUEST_LEN];
    for b in virt.iter_mut() {
        *b = 0xAB;
    }
    let mut host = virt.clone(); // identity map

    // oracle: only ops 0..2 execute (op2 is the one that bails)
    let (mut regs_i, mut virt_i) = (regs, virt.clone());
    interpret(&block[..2], &mut regs_i, &mut virt_i);

    let (regs_w, bail) = run_wasm_tlb(&compile_tlb(&block), regs, &mut host, &tlb);
    assert_eq!(bail, 2, "must bail at op2 (first access to the absent page)");
    assert_eq!(regs_w, regs_i, "bailed block: regs must equal the oracle run of ops 0..2");
    assert_eq!(host, virt_i, "bailed block: guest RAM must equal the oracle run of ops 0..2");
    assert_eq!(regs_w[1], 0x5555, "the bailing load must not have written its dst");
    assert_eq!(regs_w[2], 0x6666, "the instruction after the bail must not have run");
    assert_eq!(u64::from_le_bytes(host[0x10..0x18].try_into().unwrap()), 0x1234, "op1 store landed");
}

/// The x86 ALU rflags oracle — the exact CF/PF/AF/ZF/SF/OF semantics `emit_flags` mirrors.
#[cfg(test)]
fn x86_alu_flags(op: Bin, a: u64, b: u64, r: u64, rflags: u64) -> u64 {
    let mut f = rflags & !ALU_FLAGS_MASK;
    if r == 0 {
        f |= 1 << 6; // ZF
    }
    if r >> 63 != 0 {
        f |= 1 << 7; // SF
    }
    if (r & 0xff).count_ones() % 2 == 0 {
        f |= 1 << 2; // PF (even parity of low byte)
    }
    match op {
        Bin::Add => {
            if r < a {
                f |= 1; // CF
            }
            if ((a ^ r) & (b ^ r)) >> 63 != 0 {
                f |= 1 << 11; // OF
            }
        }
        Bin::Sub => {
            if a < b {
                f |= 1; // CF (borrow)
            }
            if ((a ^ b) & (a ^ r)) >> 63 != 0 {
                f |= 1 << 11; // OF
            }
        }
        Bin::And | Bin::Or | Bin::Xor => {} // logical: CF = OF = 0 (AF untouched)
    }
    f
}

/// Reference interpreter that also tracks `rflags` (Movi/Movr/Bin only — the flags test set).
#[cfg(test)]
fn interpret_flags(block: &[Op], r: &mut [u64; NREG], rflags: &mut u64) {
    for op in block {
        match *op {
            Op::Movi { d, imm } => r[d as usize] = imm,
            Op::Movr { d, s } => r[d as usize] = r[s as usize],
            Op::Bin { op, d, a, b } => {
                let (av, bv) = (r[a as usize], r[b as usize]);
                let res = match op {
                    Bin::Add => av.wrapping_add(bv),
                    Bin::Sub => av.wrapping_sub(bv),
                    Bin::Xor => av ^ bv,
                    Bin::And => av & bv,
                    Bin::Or => av | bv,
                };
                r[d as usize] = res;
                *rflags = x86_alu_flags(op, av, bv, res, *rflags);
            }
            _ => unreachable!("the flags differential uses only Movi/Movr/Bin"),
        }
    }
}

/// Run a flags-mode block: regs at `0..128`, `rflags` at `RFLAGS_MEM`. Returns `(regs, rflags)`.
#[cfg(test)]
fn run_wasm_flags(bytes: &[u8], regs: [u64; NREG], rflags_in: u64) -> ([u64; NREG], u64) {
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("emitted wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &rflags_in.to_le_bytes()).unwrap();
    run.call(&mut store, ()).expect("run");
    let mut out = [0u64; NREG];
    for (i, o) in out.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    let mut fb = [0u8; 8];
    mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
    (out, u64::from_le_bytes(fb))
}

/// Flags differential: random Movi/Movr/Bin blocks, interpret (with the x86 flags oracle)
/// vs `compile_flags`→wasmtime, must agree on registers AND `rflags` — closing the last
/// correctness gap (the boot differential is regs+rflags). Non-ALU rflags bits are
/// preserved; CF/PF/AF/ZF/SF/OF computed per op.
#[cfg(test)]
#[test]
fn jit_flags_match_the_x86_oracle() {
    let mut rng = Rng(0x243f_6a88_85a3_08d3);
    for _ in 0..400 {
        let n = 1 + rng.next() % 30;
        let mut block = Vec::new();
        for _ in 0..n {
            let (d, a, b) = (rng.reg(), rng.reg(), rng.reg());
            block.push(match rng.next() % 6 {
                0 => Op::Movi { d, imm: rng.next() },
                1 => Op::Bin { op: Bin::Add, d, a, b },
                2 => Op::Bin { op: Bin::Sub, d, a, b },
                3 => Op::Bin { op: Bin::And, d, a, b },
                4 => Op::Bin { op: Bin::Or, d, a, b },
                _ => Op::Bin { op: Bin::Xor, d, a, b },
            });
        }
        let mut regs = [0u64; NREG];
        for r in regs.iter_mut() {
            *r = rng.next();
        }
        let rflags0 = (rng.next() & 0xffff) | 0x2; // random flags incl. non-ALU bits + reserved
        let (mut regs_i, mut rf_i) = (regs, rflags0);
        interpret_flags(&block, &mut regs_i, &mut rf_i);
        let (regs_w, rf_w) = run_wasm_flags(&compile_flags(&block), regs, rflags0);
        assert_eq!(regs_w, regs_i, "flags-mode block: regs diverged");
        assert_eq!(rf_w, rf_i, "flags-mode block: rflags diverged");
    }
}

/// Immediate ALU differential: random `mov r,imm` / `op r,imm` / `cmp r,imm` blocks, the full
/// interpreter (with the x86 flags oracle) vs `compile_flags`→wasmtime, must agree on registers AND
/// `rflags` — including SIGN-EXTENDED negative immediates (the classic decode/codegen bug). Proves
/// `BinImm`/`CmpImm`/`Movi` codegen is bit-identical, so real counted loops form trusted regions.
#[cfg(test)]
#[test]
fn imm_alu_and_mov_imm_match_the_x86_oracle() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut scratch = [0u8; 0x100]; // interpret_full takes a RAM ref; these blocks never touch it
    for _ in 0..400 {
        let n = 1 + rng.next() % 24;
        let mut block = Vec::new();
        for _ in 0..n {
            let d = rng.reg();
            // mix wide immediates and small SIGNED ones (imm8 range, incl. negatives)
            let imm = if rng.next() & 1 == 0 {
                rng.next()
            } else {
                (rng.next() % 256) as i8 as i64 as u64 // -128..=127 sign-extended
            };
            block.push(match rng.next() % 7 {
                0 => Op::Movi { d, imm },
                1 => Op::BinImm { op: Bin::Add, d, imm },
                2 => Op::BinImm { op: Bin::Sub, d, imm },
                3 => Op::BinImm { op: Bin::And, d, imm },
                4 => Op::BinImm { op: Bin::Or, d, imm },
                5 => Op::BinImm { op: Bin::Xor, d, imm },
                _ => Op::CmpImm { a: d, imm },
            });
        }
        let mut regs = [0u64; NREG];
        for r in regs.iter_mut() {
            *r = rng.next();
        }
        let rflags0 = (rng.next() & 0xffff) | 0x2;
        let (mut regs_i, mut rf_i) = (regs, rflags0);
        interpret_full(&block, &mut regs_i, &mut scratch, &mut rf_i);
        let (regs_w, rf_w) = run_wasm_flags(&compile_flags(&block), regs, rflags0);
        assert_eq!(regs_w, regs_i, "imm block: regs diverged");
        assert_eq!(rf_w, rf_i, "imm block: rflags diverged");
    }
}

/// Apply a binary ALU op (shared by the flags oracles).
#[cfg(test)]
fn bin_apply(op: Bin, a: u64, b: u64) -> u64 {
    match op {
        Bin::Add => a.wrapping_add(b),
        Bin::Sub => a.wrapping_sub(b),
        Bin::Xor => a ^ b,
        Bin::And => a & b,
        Bin::Or => a | b,
    }
}

/// Combined oracle: registers, guest RAM, AND rflags (Movi/Movr/Bin/Load/Store/LoadOp —
/// the `compile_tlb_flags` set, excluding Shift whose flags are not yet modelled).
#[cfg(test)]
fn interpret_full(block: &[Op], r: &mut [u64; NREG], ram: &mut [u8], rflags: &mut u64) {
    for op in block {
        match *op {
            Op::Movi { d, imm } => r[d as usize] = imm,
            Op::Movr { d, s } => r[d as usize] = r[s as usize],
            Op::Load { d, base, idx, scale, disp } => {
                let a = eff_addr(r, base, idx, scale, disp);
                r[d as usize] = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
            }
            Op::Store { base, idx, scale, disp, s } => {
                let a = eff_addr(r, base, idx, scale, disp);
                ram[a..a + 8].copy_from_slice(&r[s as usize].to_le_bytes());
            }
            Op::Bin { op, d, a, b } => {
                let (av, bv) = (r[a as usize], r[b as usize]);
                let res = bin_apply(op, av, bv);
                r[d as usize] = res;
                *rflags = x86_alu_flags(op, av, bv, res, *rflags);
            }
            Op::LoadOp { op, d, base, idx, scale, disp } => {
                let a = eff_addr(r, base, idx, scale, disp);
                let m = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
                let dv = r[d as usize];
                let res = bin_apply(op, dv, m);
                r[d as usize] = res;
                *rflags = x86_alu_flags(op, dv, m, res, *rflags);
            }
            Op::Lea { d, base, idx, scale, disp } => {
                r[d as usize] = eff_addr(r, base, idx, scale, disp) as u64; // address; no flags
            }
            Op::Cmp { a, b } => {
                let (av, bv) = (r[a as usize], r[b as usize]);
                let res = av.wrapping_sub(bv);
                *rflags = x86_alu_flags(Bin::Sub, av, bv, res, *rflags); // cmp: flags of a-b, no reg
            }
            Op::Test { a, b } => {
                let (av, bv) = (r[a as usize], r[b as usize]);
                *rflags = x86_alu_flags(Bin::And, av, bv, av & bv, *rflags); // test: flags of a&b, no reg
            }
            Op::BinImm { op, d, imm } => {
                let dv = r[d as usize];
                let res = bin_apply(op, dv, imm);
                r[d as usize] = res;
                *rflags = x86_alu_flags(op, dv, imm, res, *rflags);
            }
            Op::CmpImm { a, imm } => {
                let av = r[a as usize];
                let res = av.wrapping_sub(imm);
                *rflags = x86_alu_flags(Bin::Sub, av, imm, res, *rflags); // cmp r,imm: flags of a-imm
            }
            Op::Push { s } => {
                let val = r[s as usize]; // push/pop do NOT affect rflags
                let a = r[RSP as usize].wrapping_sub(8) as usize;
                ram[a..a + 8].copy_from_slice(&val.to_le_bytes());
                r[RSP as usize] = a as u64;
            }
            Op::Pop { d } => {
                let a = r[RSP as usize] as usize;
                let val = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
                r[RSP as usize] = r[RSP as usize].wrapping_add(8);
                r[d as usize] = val;
            }
            Op::Shift { .. } => unreachable!("the tlb+flags differential excludes Shift"),
        }
    }
}

/// Run a `compile_tlb_flags` block: regs `0..128`, rflags at `RFLAGS_MEM`, TLB at
/// `TLB_BASE`, guest RAM at `GUEST_BASE`. Returns `(regs, rflags, bail)`.
#[cfg(test)]
fn run_wasm_tlb_flags(
    bytes: &[u8],
    regs: [u64; NREG],
    ram: &mut [u8],
    tlb: &[u8],
    rflags_in: u64,
) -> ([u64; NREG], u64, i32) {
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("emitted wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    // Grow the module's linear memory so the guest-RAM region [GUEST_BASE, +ram.len()) fits —
    // the codegen declares only 4 pages, but real guest RAM / high physical addresses need
    // more. This is the executor capability `run()` relies on for arbitrary RAM sizes.
    let need_pages = (GUEST_BASE as usize + ram.len()).div_ceil(0x10000);
    let have_pages = mem.size(&store) as usize;
    if need_pages > have_pages {
        mem.grow(&mut store, (need_pages - have_pages) as u64).expect("grow guest RAM region");
    }
    let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &rflags_in.to_le_bytes()).unwrap();
    mem.write(&mut store, TLB_BASE as usize, tlb).unwrap();
    mem.write(&mut store, GUEST_BASE as usize, ram).unwrap();
    let bail = run.call(&mut store, ()).expect("run");
    let mut out = [0u64; NREG];
    for (i, o) in out.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
    let mut fb = [0u8; 8];
    mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
    mem.read(&store, GUEST_BASE as usize, ram).unwrap();
    (out, u64::from_le_bytes(fb), bail)
}

/// The codegen mode the live `run()` dispatch uses, end-to-end: inline-TLB translation
/// (page-permuted, so the TLB is genuinely consulted) composed with rflags maintenance and
/// memory ops. Oracle vs `compile_tlb_flags`→wasmtime = bit-identical on regs, rflags, AND
/// every guest page. Proves the two paths compose correctly.
#[cfg(test)]
#[test]
fn jit_tlb_flags_compose() {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    const PERM: [usize; 3] = [2, 0, 1];
    const BASES: [u8; 3] = [13, 14, 15];
    for _ in 0..200 {
        let n = 1 + rng.next() % 20;
        let mut block = Vec::new();
        for _ in 0..n {
            let d = (rng.next() % 13) as u8;
            let base = BASES[(rng.next() % 3) as usize];
            let disp = ((rng.next() % 0x200) * 8) as i32; // 8-aligned, within a page
            block.push(match rng.next() % 7 {
                0 => Op::Movi { d, imm: rng.next() },
                1 => Op::Bin { op: Bin::Add, d, a: rng.reg(), b: rng.reg() },
                2 => Op::Bin { op: Bin::Sub, d, a: rng.reg(), b: rng.reg() },
                3 => Op::Bin { op: Bin::Xor, d, a: rng.reg(), b: rng.reg() },
                4 => Op::Load { d, base, idx: NO_REG, scale: 0, disp },
                5 => Op::Store { base, idx: NO_REG, scale: 0, disp, s: rng.reg() },
                _ => Op::LoadOp { op: Bin::Add, d, base, idx: NO_REG, scale: 0, disp },
            });
        }
        let mut regs = [0u64; NREG];
        for r in regs[..13].iter_mut() {
            *r = rng.next();
        }
        regs[13] = 0x0000;
        regs[14] = 0x1000;
        regs[15] = 0x2000;
        let mut virt = vec![0u8; GUEST_LEN];
        for b in virt.iter_mut() {
            *b = (rng.next() & 0xff) as u8;
        }
        let mut host = vec![0u8; GUEST_LEN];
        for p in 0..3 {
            host[PERM[p] * 0x1000..PERM[p] * 0x1000 + 0x1000]
                .copy_from_slice(&virt[p * 0x1000..p * 0x1000 + 0x1000]);
        }
        let mut tlb = vec![0u8; TLB_SIZE as usize * 16];
        for p in 0..3 {
            tlb[p * 16..p * 16 + 8].copy_from_slice(&(p as u64).to_le_bytes());
            tlb[p * 16 + 8..p * 16 + 16].copy_from_slice(&((PERM[p] as u64) * 0x1000).to_le_bytes());
        }
        let rflags0 = (rng.next() & 0xffff) | 0x2;
        let (mut regs_i, mut virt_i, mut rf_i) = (regs, virt.clone(), rflags0);
        interpret_full(&block, &mut regs_i, &mut virt_i, &mut rf_i);
        let (regs_w, rf_w, bail) =
            run_wasm_tlb_flags(&compile_tlb_flags(&block), regs, &mut host, &tlb, rflags0);
        assert_eq!(bail as usize, block.len(), "all pages present — block completes");
        assert_eq!(regs_w, regs_i, "tlb+flags: regs diverged");
        assert_eq!(rf_w, rf_i, "tlb+flags: rflags diverged");
        for p in 0..3 {
            assert_eq!(
                &host[PERM[p] * 0x1000..PERM[p] * 0x1000 + 0x1000],
                &virt_i[p * 0x1000..p * 0x1000 + 0x1000],
                "tlb+flags: guest page {p} diverged"
            );
        }
    }
}

#[cfg(test)]
#[test]
fn decode_block_reports_length_and_bails_at_a_branch() {
    // two reg-reg ALU ops then a jmp rel8 (unmodelled) — the block covers only the 6 bytes;
    // the branch is left for the interpreter to resume at.
    let bytes = [
        0x48, 0x01, 0xd8, // add rax, rbx
        0x48, 0x31, 0xc8, // xor rax, rcx
        0xeb, 0xf0, // jmp -16 (control transfer → bail)
    ];
    let (ops, offsets, consumed) = decode_block(&bytes);
    assert_eq!(ops.len(), 2, "two ALU ops decoded before the branch");
    assert_eq!(consumed, 6, "only the two 3-byte ops are counted; the jmp is left to step()");
    // per-op byte offsets — a bail at op k resumes at block_start + offsets[k]
    assert_eq!(offsets, vec![0u32, 3], "op 0 at byte 0, op 1 at byte 3");

    // offsets must track variable-length instructions: a 3-byte reg-reg op, an 8-byte
    // disp32 memory op, then another 3-byte op.
    let mixed = [
        0x48, 0x01, 0xd8, // add rax, rbx            (3 bytes)  @0
        0x48, 0x8b, 0x84, 0x24, 0x00, 0x01, 0x00, 0x00, // mov rax,[rsp+0x100] (8 bytes) @3
        0x48, 0x01, 0xd8, // add rax, rbx            (3 bytes)  @11
    ];
    let (mops, moffsets, mlen) = decode_block(&mixed);
    assert_eq!(mops.len(), 3);
    assert_eq!(moffsets, vec![0u32, 3, 11], "offsets follow the real instruction lengths");
    assert_eq!(mlen, 14);
}

#[cfg(test)]
#[test]
fn block_cache_is_kappa_keyed_with_smc_invalidation() {
    let ops = decode_x86(&[0x48, 0x01, 0xd8]); // add rax, rbx
    let mut cache = BlockCache::new(3);
    let key = [0x11u8; 32];
    // below threshold: counted, not compiled
    assert!(cache.record(key, &ops).is_none());
    assert!(cache.record(key, &ops).is_none());
    assert!(cache.get(&key).is_none());
    // crossing the threshold compiles + caches a real wasm module
    assert!(cache.record(key, &ops).is_some(), "compiled at the hotness threshold");
    let wasm = cache.get(&key).expect("now cached").to_vec();
    assert!(wasm.starts_with(&[0x00, 0x61, 0x73, 0x6d]), "cached a real wasm module");
    // a different κ (self-modified bytes) is a miss — free SMC invalidation
    assert!(cache.get(&[0x22u8; 32]).is_none(), "changed bytes → changed κ → miss → recompile");
}

/// Executor capability: run a compiled block over guest RAM LARGER than the codegen's
/// default 4-page (256 KiB) module memory, with an access at a high physical address — the
/// executor must grow the wasm memory to fit. Identity TLB so host == virtual; oracle
/// `interpret_full` vs `run_wasm_tlb_flags` (which grows) agree on regs, rflags, and RAM.
#[cfg(test)]
#[test]
fn jit_executor_grows_memory_for_large_guest_ram() {
    const RAM_LEN: usize = 0x5_0000; // 320 KiB — exceeds the default 256 KiB module memory
    const HI: u64 = 0x4_8000; // a high, page-aligned physical address (beyond 4 wasm pages)

    let block = [
        Op::Movi { d: 0, imm: 0xDEAD_BEEF_CAFE_BABE },
        Op::Store { base: 13, idx: NO_REG, scale: 0, disp: 0, s: 0 }, // [r13] = rax
        Op::Load { d: 1, base: 13, idx: NO_REG, scale: 0, disp: 0 },  // rcx = [r13]
    ];
    let mut regs = [0u64; NREG];
    regs[13] = HI;

    // identity TLB entry for the touched page: vpage = HI>>12, host_off = HI
    let vpage = HI >> 12;
    let mut tlb = vec![0u8; TLB_SIZE as usize * 16];
    let slot = (vpage & (TLB_SIZE - 1)) as usize;
    tlb[slot * 16..slot * 16 + 8].copy_from_slice(&vpage.to_le_bytes());
    tlb[slot * 16 + 8..slot * 16 + 16].copy_from_slice(&HI.to_le_bytes());

    let mut virt = vec![0u8; RAM_LEN];
    let (mut regs_i, mut virt_i, mut rf_i) = (regs, virt.clone(), 0x2u64);
    interpret_full(&block, &mut regs_i, &mut virt_i, &mut rf_i);

    let (regs_w, rf_w, bail) = run_wasm_tlb_flags(&compile_tlb_flags(&block), regs, &mut virt, &tlb, 0x2);
    assert_eq!(bail as usize, block.len(), "block completes (page present in the TLB)");
    assert_eq!(regs_w, regs_i, "large-RAM block: regs diverged");
    assert_eq!(rf_w, rf_i, "large-RAM block: rflags diverged");
    assert_eq!(virt, virt_i, "large-RAM block: guest RAM diverged");
    assert_eq!(regs_w[1], 0xDEAD_BEEF_CAFE_BABE, "rcx round-tripped through high guest RAM");
}
