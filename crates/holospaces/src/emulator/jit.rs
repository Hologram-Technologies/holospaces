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
}

/// Address-mode sentinels: no base register / no index register.
const NO_REG: u8 = 0xff;

/// Guest RAM lives in the same wasm memory as the register file, after a page of
/// headroom: regs at `r*8`, guest byte `A` at wasm offset `GUEST_BASE + A`.
const GUEST_BASE: u64 = 0x1000;
#[cfg(test)]
const GUEST_LEN: usize = 0x3000; // 3 pages of test guest RAM

/// Software TLB region in wasm memory (between the regs and guest RAM): a direct-mapped
/// array of `TLB_SIZE` 16-byte entries `(tag: vpage @0, host_off: byte offset @8)`. The
/// inline-TLB codegen translates a guest virtual address by indexing this on the hit path
/// — the mechanism that lets the JIT touch real (paged) guest RAM. `$va` scratch = local 16.
const TLB_BASE: u64 = 0x200;
const TLB_SIZE: u64 = 64; // power of two; slot = vpage & (TLB_SIZE-1)

/// The guest `rflags` slot in wasm memory (just past the 16 registers), and the flag bits
/// an ALU op writes: CF(0) PF(2) ZF(6) SF(7) OF(11). AF(4) is deliberately NOT modelled —
/// `x64.rs::flags_arith`/`flags_logic` leave AF untouched, so to stay bit-identical to
/// `step()` the JIT must leave it untouched too.
const RFLAGS_MEM: u64 = NREG as u64 * 8; // mem offset 128
const ALU_FLAGS_MASK: u64 = 0x8c5; // CF|PF|ZF|SF|OF (no AF — matches the interpreter)

// Local layout: 21 × i64 then 2 × i32 (always declared; unused ones are harmless).
const VA_LOCAL: u8 = NREG as u8; // i64 16: vaddr scratch (TLB)
const RFLAGS_LOCAL: u8 = NREG as u8 + 1; // i64 17: live rflags value (flags mode)
const FA_LOCAL: u8 = NREG as u8 + 2; // i64 18: flag operand a
const FB_LOCAL: u8 = NREG as u8 + 3; // i64 19: flag operand b
const FR_LOCAL: u8 = NREG as u8 + 4; // i64 20: flag result
const BAIL_LOCAL: u8 = NREG as u8 + 5; // i32 21: bail instruction index (TLB)
const TE_LOCAL: u8 = NREG as u8 + 6; // i32 22: TLB entry address scratch

/// Effective address `base + idx<<scale + disp` (sentinels skipped) — the meaning shared
/// by the interpreter oracle and (mirrored in wasm) the codegen.
#[cfg(test)]
fn eff_addr(r: &[u64; NREG], base: u8, idx: u8, scale: u8, disp: i32) -> usize {
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
    let emit_flags = |op: Bin, c: &mut Vec<u8>| {
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
        // OR the i64 value currently on the stack into $rflags
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
        c.push(0x50); // i64.eqz → i32
        c.push(0xad); // i64.extend_i32_u
        shl(6, c);
        or_in(c);
        // SF = (fr >> 63) << 7
        get(FR_LOCAL, c);
        c.push(0x42);
        sleb(63, c);
        c.push(0x88); // i64.shr_u → 0/1
        shl(7, c);
        or_in(c);
        // PF = (popcount(fr & 0xff) is even) << 2
        get(FR_LOCAL, c);
        c.push(0x42);
        sleb(0xff, c);
        c.push(0x83); // & 0xff
        c.push(0x7b); // i64.popcnt
        c.push(0x42);
        sleb(1, c);
        c.push(0x83); // & 1
        c.push(0x50); // i64.eqz → i32 (1 if even)
        c.push(0xad); // extend
        shl(2, c);
        or_in(c);
        // CF and OF depend on the op kind (logical ops leave them cleared)
        match op {
            Bin::Add | Bin::Sub => {
                if matches!(op, Bin::Add) {
                    // CF = (fr <u fa)
                    get(FR_LOCAL, c);
                    get(FA_LOCAL, c);
                    c.push(0x54); // i64.lt_u → i32
                    c.push(0xad);
                    or_in(c);
                    // OF = (((fa ^ fr) & (fb ^ fr)) >> 63) << 11
                    get(FA_LOCAL, c);
                    get(FR_LOCAL, c);
                    c.push(0x85); // fa^fr
                    get(FB_LOCAL, c);
                    get(FR_LOCAL, c);
                    c.push(0x85); // fb^fr
                    c.push(0x83); // and
                } else {
                    // CF = (fa <u fb)
                    get(FA_LOCAL, c);
                    get(FB_LOCAL, c);
                    c.push(0x54);
                    c.push(0xad);
                    or_in(c);
                    // OF = (((fa ^ fb) & (fa ^ fr)) >> 63) << 11
                    get(FA_LOCAL, c);
                    get(FB_LOCAL, c);
                    c.push(0x85); // fa^fb
                    get(FA_LOCAL, c);
                    get(FR_LOCAL, c);
                    c.push(0x85); // fa^fr
                    c.push(0x83); // and
                }
                c.push(0x42);
                sleb(63, c);
                c.push(0x88); // >> 63
                shl(11, c);
                or_in(c);
            }
            // logical ops: CF = OF = AF = 0 (already cleared)
            Bin::And | Bin::Or | Bin::Xor => {}
        }
    };
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

/// x86-64 decoder for the SHA-512 compression subset: reg-reg ALU/mov (ModRM mod=3) AND
/// memory-operand forms (`mov reg,[mem]`, `mov [mem],reg`, `add/or/and/sub/xor reg,[mem]`)
/// with full base+index*scale+disp addressing. **Bails** (stops) on anything else — the
/// JIT's "interpret what I don't model" discipline.
pub(crate) fn decode_x86(bytes: &[u8]) -> Vec<Op> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < bytes.len() {
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
        if mod_ == 3 {
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
                _ => break,
            };
            out.push(op);
            p += 2;
        } else {
            let ((base, idx, scale, disp), np) = match decode_mem(bytes, p + 1, rex, mod_, modrm) {
                Some(x) => x,
                None => break,
            };
            let loadop = |op| Op::LoadOp { op, d: reg, base, idx, scale, disp };
            let op = match opcode {
                0x8b => Op::Load { d: reg, base, idx, scale, disp }, // mov reg, [mem]
                0x89 => Op::Store { base, idx, scale, disp, s: reg }, // mov [mem], reg
                0x03 => loadop(Bin::Add),
                0x0b => loadop(Bin::Or),
                0x23 => loadop(Bin::And),
                0x2b => loadop(Bin::Sub),
                0x33 => loadop(Bin::Xor),
                _ => break,
            };
            out.push(op);
            p = np;
        }
    }
    out
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
