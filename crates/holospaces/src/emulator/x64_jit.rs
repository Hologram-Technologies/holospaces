//! **x86-64 → WebAssembly dynamic binary translator** — the core of the CC-48
//! substrate fast-execution path.
//!
//! The system emulator's x86-64 core ([`super::x64`]) is a faithful
//! interpreter: every guest instruction is decoded and dispatched on every
//! execution. For a *hot* basic block — a straight-line run of register-only
//! arithmetic and moves — that per-instruction dispatch dominates. This module
//! removes it by *translating* such a block, once, into a single WebAssembly
//! function that the host runs natively on its Wasm engine (the same
//! `wasmi`/Wasmtime surface the rest of holospaces runs on). The translated
//! function operates directly on the guest register file held in the host's
//! linear memory, so a re-execution of the block costs one Wasm call instead of
//! N interpreter dispatches.
//!
//! The translator is deliberately small and conservative — it is the *core*, not
//! the whole DBT. It decodes a **linear** run of register-direct integer
//! instructions (the common hot-loop shape) and stops at the first thing it does
//! not handle (control flow, a memory operand, a 16-bit operand, an unsupported
//! opcode, or the end of the slice), leaving the interpreter to take over there.
//! Every instruction it does emit is validated **bit-for-bit** against the
//! interpreter — the qemu-validated authority ([`super::x64::Cpu`], CC-44) — by
//! the differential test (`tests/cc48_jit.rs`).
//!
//! ## Emitted module shape
//!
//! ```wat
//! (import "env" "mem" (memory 1))      ;; host-provided linear memory
//! (func (export "run") (result i32) ... )  ;; returns guest instructions executed
//! ```
//!
//! The guest register file lives in that memory at byte offset 0 as 16
//! little-endian `u64` registers (`r[0..16]`) followed by `rflags` at offset
//! 128. The function reads/writes a register via `i64.load`/`i64.store` at
//! `reg*8` (rflags at 128) and returns the number of guest instructions it
//! executed (the same retired-instruction count the interpreter would report).
//!
//! `no_std` + `alloc`: the WebAssembly encoder is hand-rolled (LEB128 + a
//! function-body/module builder) so the translator compiles into the same Wasm
//! container and bare-metal core the interpreter does — no host-only encoder
//! crate.

#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::vec::Vec;

// ── Guest flag bits (must match `super::x64::flag`) ───────────────────────────
const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const OF: u64 = 1 << 11;
/// The five arithmetic flags this translator computes — every other RFLAGS bit
/// (reserved bits, IF/DF/…) is preserved unchanged across the block.
const ARITH_FLAGS: u64 = CF | PF | ZF | SF | OF;

/// Byte offset of the `rflags` slot in the register file.
const RFLAGS_OFF: u64 = 128;

/// A translated basic block: a complete, self-contained WebAssembly module plus
/// the guest instruction/byte counts it covers (so the caller can advance `rip`
/// and the retired-instruction counter past the translated region).
pub struct TranslatedBlock {
    /// The encoded WebAssembly module (importing `env.mem`, exporting `run`).
    pub wasm: Vec<u8>,
    /// The number of guest instructions the block executes (the `run` return).
    pub insns: u32,
    /// The number of guest code bytes the block consumes.
    pub bytes: u32,
}

// ── LEB128 ────────────────────────────────────────────────────────────────────

/// Append `v` as an unsigned LEB128.
fn leb_u32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Append `v` as an unsigned LEB128 (u64 width, for memory offsets > 4 GiB-safe).
fn leb_u64(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Append `v` as a signed LEB128 (`i64.const` operands).
fn leb_i64(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7; // arithmetic shift
        let sign = byte & 0x40 != 0;
        if (v == 0 && !sign) || (v == -1 && sign) {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// Append `v` as a signed LEB128 (`i32.const` operands).
fn leb_i32(out: &mut Vec<u8>, v: i32) {
    leb_i64(out, i64::from(v));
}

// ── Wasm opcodes used by the emitter ──────────────────────────────────────────
mod op {
    pub const END: u8 = 0x0b;
    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const I64_LOAD: u8 = 0x29;
    pub const I64_STORE: u8 = 0x37;
    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;
    pub const I64_EQZ: u8 = 0x50;
    pub const I64_EQ: u8 = 0x51;
    pub const I64_NE: u8 = 0x52;
    pub const I64_LT_U: u8 = 0x54;
    pub const I64_ADD: u8 = 0x7c;
    pub const I64_SUB: u8 = 0x7d;
    pub const I64_AND: u8 = 0x83;
    pub const I64_OR: u8 = 0x84;
    pub const I64_XOR: u8 = 0x85;
    pub const I64_SHL: u8 = 0x86;
    pub const I64_SHR_U: u8 = 0x88;
    pub const I64_POPCNT: u8 = 0x7b;
    pub const I64_EXTEND_I32_U: u8 = 0xad;
}

// ── Function body builder ─────────────────────────────────────────────────────

/// Builds the body (instruction stream) of the `run` function. Temporaries are
/// `i64` locals allocated on demand; the body operates on the register file in
/// the imported memory.
struct Body {
    code: Vec<u8>,
    /// Number of extra `i64` locals (beyond the zero parameters) declared.
    i64_locals: u32,
}

impl Body {
    fn new() -> Self {
        Body {
            code: Vec::new(),
            i64_locals: 0,
        }
    }

    /// Reserve a fresh `i64` local, returning its index.
    fn local(&mut self) -> u32 {
        let idx = self.i64_locals;
        self.i64_locals += 1;
        idx
    }

    fn byte(&mut self, b: u8) {
        self.code.push(b);
    }

    fn local_get(&mut self, i: u32) {
        self.byte(op::LOCAL_GET);
        leb_u32(&mut self.code, i);
    }
    fn local_set(&mut self, i: u32) {
        self.byte(op::LOCAL_SET);
        leb_u32(&mut self.code, i);
    }
    fn i64_const(&mut self, v: i64) {
        self.byte(op::I64_CONST);
        leb_i64(&mut self.code, v);
    }
    fn i32_const(&mut self, v: i32) {
        self.byte(op::I32_CONST);
        leb_i32(&mut self.code, v);
    }

    /// Load `reg` from the register file onto the stack (`i64.load` @ reg*8).
    fn load_reg(&mut self, reg: u32) {
        self.i32_const(0); // base address operand for i64.load
        self.byte(op::I64_LOAD);
        // memarg: align (3 = 8-byte), then offset
        leb_u32(&mut self.code, 3);
        leb_u64(&mut self.code, u64::from(reg) * 8);
    }

    /// Load the rflags slot onto the stack.
    fn load_rflags(&mut self) {
        self.i32_const(0);
        self.byte(op::I64_LOAD);
        leb_u32(&mut self.code, 3);
        leb_u64(&mut self.code, RFLAGS_OFF);
    }

    /// Emit `i64.store` to `reg` of the value produced by `f` (the value
    /// expression is generated after the address constant, as Wasm requires).
    fn store_reg(&mut self, reg: u32, f: impl FnOnce(&mut Self)) {
        self.i32_const(0); // address operand
        f(self); // value operand
        self.byte(op::I64_STORE);
        leb_u32(&mut self.code, 3);
        leb_u64(&mut self.code, u64::from(reg) * 8);
    }

    /// Emit `i64.store` to the rflags slot of the value produced by `f`.
    fn store_rflags(&mut self, f: impl FnOnce(&mut Self)) {
        self.i32_const(0);
        f(self);
        self.byte(op::I64_STORE);
        leb_u32(&mut self.code, 3);
        leb_u64(&mut self.code, RFLAGS_OFF);
    }

    fn binop(&mut self, b: u8) {
        self.byte(b);
    }
}

// ── Register file accessors (operand width semantics) ─────────────────────────

/// The mask for an operand size in bytes (matching `Cpu::mask`): 8 → all bits,
/// 4 → low 32. Only 4 and 8 are produced (16/8-bit operands stop the block).
fn size_mask(size: u8) -> i64 {
    if size >= 8 {
        -1 // u64::MAX as i64
    } else {
        0xffff_ffff
    }
}

/// Push the masked source value of `reg` at operand `size` (the value an ALU
/// reads: `r[reg] & mask(size)`).
fn push_masked_reg(body: &mut Body, reg: u32, size: u8) {
    body.load_reg(reg);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
}

/// Write `val_expr`'s result into `reg` honouring the x86-64 zero-extension rule:
/// a 32-bit write clears the upper 32 bits; a 64-bit write is full. (`val_expr`
/// is assumed already masked to `size` by the caller.)
fn write_reg(body: &mut Body, reg: u32, size: u8, val_local: u32) {
    body.store_reg(reg, |b| {
        b.local_get(val_local);
        if size < 8 {
            // zero-extend: keep only the low `size` bytes (32-bit clears upper).
            b.i64_const(size_mask(size));
            b.binop(op::I64_AND);
        }
    });
}

// ── Flag emission (must match `Cpu::flags_arith` / `Cpu::flags_logic`) ─────────

/// The bit index of the sign bit for an operand `size` (in bytes): `size*8-1`.
fn sign_bit(size: u8) -> u32 {
    (size as u32) * 8 - 1
}

/// Emit code that pushes (as an `i64` 0/1) ZF for the masked result in
/// `r_local`: `(r == 0)`.
fn push_zf(body: &mut Body, r_local: u32) {
    body.local_get(r_local);
    body.binop(op::I64_EQZ); // → i32 (0/1)
    body.binop(op::I64_EXTEND_I32_U);
}

/// Emit SF (0/1) for the masked result: top bit of the size.
fn push_sf(body: &mut Body, r_local: u32, size: u8) {
    body.local_get(r_local);
    body.i64_const(i64::from(sign_bit(size)));
    body.binop(op::I64_SHR_U);
    body.i64_const(1);
    body.binop(op::I64_AND);
}

/// Emit PF (0/1) for the masked result: parity of the LOW BYTE, set when the
/// popcount is even (`count_ones() % 2 == 0`).
fn push_pf(body: &mut Body, r_local: u32) {
    body.local_get(r_local);
    body.i64_const(0xff);
    body.binop(op::I64_AND);
    body.binop(op::I64_POPCNT);
    body.i64_const(1);
    body.binop(op::I64_AND);
    // pf = 1 - (popcount & 1)  → even parity
    body.binop(op::I64_EQZ);
    body.binop(op::I64_EXTEND_I32_U);
}

/// Shift a 0/1 flag value (on the stack) into its RFLAGS bit position.
fn shift_into(body: &mut Body, bit: u64) {
    let pos = bit.trailing_zeros();
    if pos != 0 {
        body.i64_const(i64::from(pos));
        body.binop(op::I64_SHL);
    }
}

/// Build the new rflags value on the stack from a closure that pushes each of
/// CF/PF/ZF/SF/OF (already shifted into position) OR-combined, starting from the
/// old rflags with the arithmetic flags cleared. `f` is invoked to push the
/// OR-accumulated flag contributions; the running accumulator is already on the
/// stack when `f` is called and `f` must leave exactly one i64 (the full new
/// rflags) on the stack.
fn write_arith_flags(body: &mut Body, f: impl FnOnce(&mut Body)) {
    body.store_rflags(|b| {
        b.load_rflags();
        b.i64_const(!(ARITH_FLAGS as i64));
        b.binop(op::I64_AND);
        f(b);
    });
}

/// Emit the logical-op flags (`flags_logic`): ZF/SF/PF from the result, CF=0,
/// OF=0. `r_local` holds the masked result.
fn emit_flags_logic(body: &mut Body, r_local: u32, size: u8) {
    write_arith_flags(body, |b| {
        // base (rflags with arith bits cleared) is on the stack.
        push_zf(b, r_local);
        shift_into(b, ZF);
        b.binop(op::I64_OR);
        push_sf(b, r_local, size);
        shift_into(b, SF);
        b.binop(op::I64_OR);
        push_pf(b, r_local);
        shift_into(b, PF);
        b.binop(op::I64_OR);
        // CF and OF stay 0.
    });
}

/// Emit the arithmetic flags (`flags_arith`) for `a (op) b = res`. `a_local`,
/// `b_local` hold the *masked* operands; `r_local` holds the masked result.
/// `sub` selects subtraction semantics for CF/OF.
fn emit_flags_arith(
    body: &mut Body,
    a_local: u32,
    b_local: u32,
    r_local: u32,
    size: u8,
    sub: bool,
) {
    write_arith_flags(body, |b| {
        push_zf(b, r_local);
        shift_into(b, ZF);
        b.binop(op::I64_OR);
        push_sf(b, r_local, size);
        shift_into(b, SF);
        b.binop(op::I64_OR);
        push_pf(b, r_local);
        shift_into(b, PF);
        b.binop(op::I64_OR);

        // CF.
        if sub {
            // CF = (a & m) < (b & m)
            b.local_get(a_local);
            b.local_get(b_local);
            b.binop(op::I64_LT_U);
            b.binop(op::I64_EXTEND_I32_U);
        } else {
            // CF = (r < a)   (r and a both masked)
            b.local_get(r_local);
            b.local_get(a_local);
            b.binop(op::I64_LT_U);
            b.binop(op::I64_EXTEND_I32_U);
        }
        shift_into(b, CF);
        b.binop(op::I64_OR);

        // OF — computed from sign bits of a, b, r.
        // sub: OF = (sign(a)!=sign(b)) && (sign(a)!=sign(r))
        // add: OF = (sign(a)==sign(b)) && (sign(a)!=sign(r))
        emit_of(b, a_local, b_local, r_local, size, sub);
        shift_into(b, OF);
        b.binop(op::I64_OR);
    });
}

/// Push OF (0/1) for `a (op) b = r` (operands already masked).
fn emit_of(body: &mut Body, a_local: u32, b_local: u32, r_local: u32, size: u8, sub: bool) {
    let bit = i64::from(sign_bit(size));
    // helper: push sign(x) as 0/1
    let push_sign = |body: &mut Body, local: u32| {
        body.local_get(local);
        body.i64_const(bit);
        body.binop(op::I64_SHR_U);
        body.i64_const(1);
        body.binop(op::I64_AND);
    };
    // term1 = (sign(a) (== or !=) sign(b))
    push_sign(body, a_local);
    push_sign(body, b_local);
    if sub {
        body.binop(op::I64_NE);
    } else {
        body.binop(op::I64_EQ);
    }
    body.binop(op::I64_EXTEND_I32_U);
    // term2 = (sign(a) != sign(r))
    push_sign(body, a_local);
    push_sign(body, r_local);
    body.binop(op::I64_NE);
    body.binop(op::I64_EXTEND_I32_U);
    // OF = term1 & term2
    body.binop(op::I64_AND);
}

// ── Instruction decode + emit ─────────────────────────────────────────────────

/// One supported ALU operation (the group-1 digit / opcode high-nibble>>3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AluOp {
    Add,
    Or,
    And,
    Sub,
    Xor,
    Cmp,
}

impl AluOp {
    /// Map an ALU group digit (0=add,1=or,4=and,5=sub,6=xor,7=cmp) to an op.
    /// adc(2)/sbb(3) are unsupported (they read CF) → `None`.
    fn from_digit(d: u8) -> Option<AluOp> {
        Some(match d {
            0 => AluOp::Add,
            1 => AluOp::Or,
            4 => AluOp::And,
            5 => AluOp::Sub,
            6 => AluOp::Xor,
            7 => AluOp::Cmp,
            _ => return None,
        })
    }
}

/// A decoder cursor over the guest code slice.
struct Decoder<'a> {
    code: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(code: &'a [u8]) -> Self {
        Decoder { code, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.code.get(self.pos).copied()
    }

    fn u8(&mut self) -> Option<u8> {
        let b = self.code.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn u32_le(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..4 {
            v |= u32::from(self.u8()?) << (i * 8);
        }
        Some(v)
    }

    fn u64_le(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..8 {
            v |= u64::from(self.u8()?) << (i * 8);
        }
        Some(v)
    }
}

/// A decoded register-direct ModRM: the (REX-extended) reg and rm register
/// indices. Returns `None` if the operand is a memory form (`mod != 3`).
struct ModRm {
    reg: u32,
    rm: u32,
}

fn decode_modrm(dec: &mut Decoder, rex: u8) -> Option<ModRm> {
    let m = dec.u8()?;
    let md = m >> 6;
    if md != 3 {
        return None; // memory operand — unsupported, stop the block
    }
    let reg = u32::from((m >> 3) & 7) | (u32::from((rex >> 2) & 1) << 3); // REX.R
    let rm = u32::from(m & 7) | (u32::from(rex & 1) << 3); // REX.B
    Some(ModRm { reg, rm })
}

/// Emit one ALU op `dst (op)= src_b` where both operands are register values.
/// `dst` is the register written (skipped for CMP); `a` is the destination's
/// current value, `b` is the source. Flags are set per the op.
#[allow(clippy::too_many_arguments)]
fn emit_alu(body: &mut Body, op: AluOp, dst: u32, a_local: u32, b_local: u32, size: u8) {
    // result local
    let r = body.local();
    // compute the raw result, then mask it.
    let m = size_mask(size);
    let mask_if = |b: &mut Body| {
        if size < 8 {
            b.i64_const(m);
            b.binop(op::I64_AND);
        }
    };
    match op {
        AluOp::Add => {
            body.local_get(a_local);
            body.local_get(b_local);
            body.binop(op::I64_ADD);
            mask_if(body);
            body.local_set(r);
            emit_flags_arith(body, a_local, b_local, r, size, false);
        }
        AluOp::Sub | AluOp::Cmp => {
            body.local_get(a_local);
            body.local_get(b_local);
            body.binop(op::I64_SUB);
            mask_if(body);
            body.local_set(r);
            emit_flags_arith(body, a_local, b_local, r, size, true);
        }
        AluOp::Or => {
            body.local_get(a_local);
            body.local_get(b_local);
            body.binop(op::I64_OR);
            mask_if(body);
            body.local_set(r);
            emit_flags_logic(body, r, size);
        }
        AluOp::And => {
            body.local_get(a_local);
            body.local_get(b_local);
            body.binop(op::I64_AND);
            mask_if(body);
            body.local_set(r);
            emit_flags_logic(body, r, size);
        }
        AluOp::Xor => {
            body.local_get(a_local);
            body.local_get(b_local);
            body.binop(op::I64_XOR);
            mask_if(body);
            body.local_set(r);
            emit_flags_logic(body, r, size);
        }
    }
    // Write the destination unless this is CMP (which discards the result).
    if op != AluOp::Cmp {
        write_reg(body, dst, size, r);
    }
}

/// Translate a linear run of supported instructions starting at the block entry.
///
/// `code` is a contiguous slice of guest machine code from the block entry. The
/// translator decodes supported register-direct integer instructions until it
/// hits an unsupported byte, a control-flow / memory operand, a 16-bit operand,
/// or the end of the slice, then emits **one** Wasm module covering the
/// instructions it consumed. Returns `None` if the *first* instruction is
/// unsupported (the caller interprets that instruction instead).
#[must_use]
pub fn translate_block(code: &[u8]) -> Option<TranslatedBlock> {
    let mut body = Body::new();
    let mut dec = Decoder::new(code);
    let mut insns: u32 = 0;

    loop {
        let start = dec.pos;
        if dec.peek().is_none() {
            break; // end of slice
        }
        match decode_one(&mut dec, &mut body) {
            DecodeResult::Ok => {
                insns += 1;
            }
            DecodeResult::Stop => {
                // Roll back any partial decode of this instruction.
                dec.pos = start;
                break;
            }
        }
    }

    if insns == 0 {
        return None; // first instruction unsupported — let the interpreter run
    }

    // `run` returns the number of guest instructions executed.
    body.i32_const(insns as i32);

    let wasm = encode_module(&body);
    Some(TranslatedBlock {
        wasm,
        insns,
        bytes: dec.pos as u32,
    })
}

enum DecodeResult {
    Ok,
    Stop,
}

/// Decode and emit one instruction. On `Stop`, `body` may have had partial code
/// appended — but `translate_block` only emits once a full instruction succeeds,
/// because a `Stop` ends the loop and the trailing (return-value) code is the
/// only thing appended after the last good instruction. Each `Ok` path appends a
/// complete, self-contained instruction.
fn decode_one(dec: &mut Decoder, body: &mut Body) -> DecodeResult {
    // We must not append partial code on Stop. So decode fully into locals first
    // by peeking; we use a scratch sub-decoder for the header, then commit.
    let mut rex = 0u8;
    let mut rex_present = false;

    // Optional single REX prefix (0x40-0x4f). A 0x66 / other prefix is not
    // supported and stops the block.
    let b0 = match dec.peek() {
        Some(b) => b,
        None => return DecodeResult::Stop,
    };
    if (0x40..=0x4f).contains(&b0) {
        rex = b0;
        rex_present = true;
        dec.u8();
    } else if b0 == 0x66 {
        return DecodeResult::Stop; // 16-bit operand size — unsupported
    }

    let size: u8 = if rex & 8 != 0 { 8 } else { 4 };

    let op = match dec.u8() {
        Some(o) => o,
        None => return DecodeResult::Stop,
    };

    // To avoid emitting partial code on Stop, decode the full operand set into a
    // small staging description, then emit. ModRm decode can fail (memory form
    // or truncation) — handle before any emission.
    let _ = rex_present;

    match op {
        // ── ALU reg forms: r/m,r (0x01..) and r,r/m (0x03..) ──
        0x01 | 0x09 | 0x21 | 0x29 | 0x31 | 0x39 => {
            // op r/m, r  : dst = r/m, a = r/m, b = reg
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let Some(alu) = AluOp::from_digit(op >> 3) else {
                return DecodeResult::Stop;
            };
            emit_reg_reg(body, alu, d.rm, d.rm, d.reg, size);
            DecodeResult::Ok
        }
        0x03 | 0x0b | 0x23 | 0x2b | 0x33 | 0x3b => {
            // op r, r/m  : dst = reg, a = reg, b = r/m
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let Some(alu) = AluOp::from_digit(op >> 3) else {
                return DecodeResult::Stop;
            };
            emit_reg_reg(body, alu, d.reg, d.reg, d.rm, size);
            DecodeResult::Ok
        }
        // ── group1: 0x81 /digit imm32-sext, 0x83 /digit imm8-sext ──
        0x81 | 0x83 => {
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let digit = d.reg & 7; // /digit lives in the reg field (low 3 bits)
            let Some(alu) = AluOp::from_digit(digit as u8) else {
                return DecodeResult::Stop; // adc/sbb (2/3) unsupported
            };
            // immediate (sign-extended to operand size, then masked like fetch).
            let imm: u64 = if op == 0x83 {
                let Some(b) = dec.u8() else {
                    return DecodeResult::Stop;
                };
                (b as i8 as i64) as u64
            } else {
                let Some(v) = dec.u32_le() else {
                    return DecodeResult::Stop;
                };
                (v as i32 as i64) as u64
            };
            emit_reg_imm(body, alu, d.rm, imm, size);
            DecodeResult::Ok
        }
        // ── mov r/m, r (0x89) ──
        0x89 => {
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            emit_mov_reg(body, d.rm, d.reg, size);
            DecodeResult::Ok
        }
        // ── mov r, r/m (0x8B) ──
        0x8b => {
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            emit_mov_reg(body, d.reg, d.rm, size);
            DecodeResult::Ok
        }
        // ── mov r, imm (0xB8+r): imm64 if REX.W else imm32 zero-extended ──
        0xb8..=0xbf => {
            let reg = u32::from(op - 0xb8) | (u32::from(rex & 1) << 3);
            let imm: u64 = if size == 8 {
                match dec.u64_le() {
                    Some(v) => v,
                    None => return DecodeResult::Stop,
                }
            } else {
                match dec.u32_le() {
                    Some(v) => u64::from(v),
                    None => return DecodeResult::Stop,
                }
            };
            emit_mov_imm(body, reg, imm, size);
            DecodeResult::Ok
        }
        // ── mov r/m, imm32-sext (0xC7 /0) ──
        0xc7 => {
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            if d.reg & 7 != 0 {
                return DecodeResult::Stop; // only /0 is MOV
            }
            let Some(v) = dec.u32_le() else {
                return DecodeResult::Stop;
            };
            let imm = (v as i32 as i64) as u64;
            emit_mov_imm(body, d.rm, imm, size);
            DecodeResult::Ok
        }
        // ── inc/dec via 0xFF /0 (inc) and /1 (dec) ──
        0xff => {
            let Some(d) = stage_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            match d.reg & 7 {
                0 => {
                    emit_inc_dec(body, d.rm, false, size);
                    DecodeResult::Ok
                }
                1 => {
                    emit_inc_dec(body, d.rm, true, size);
                    DecodeResult::Ok
                }
                _ => DecodeResult::Stop, // call/jmp/push — control flow / memory
            }
        }
        _ => DecodeResult::Stop,
    }
}

/// Decode a register-direct ModRM, returning `None` (→ Stop) on a memory form or
/// truncation.
fn stage_modrm(dec: &mut Decoder, rex: u8) -> Option<ModRm> {
    decode_modrm(dec, rex)
}

/// `op dst, b` where both `a` (dst's value) and `b` are registers.
fn emit_reg_reg(body: &mut Body, alu: AluOp, dst: u32, a_reg: u32, b_reg: u32, size: u8) {
    let a = body.local();
    let b = body.local();
    push_masked_reg(body, a_reg, size);
    body.local_set(a);
    push_masked_reg(body, b_reg, size);
    body.local_set(b);
    emit_alu(body, alu, dst, a, b, size);
}

/// `op dst, imm` (imm already sign-extended to a u64).
fn emit_reg_imm(body: &mut Body, alu: AluOp, dst: u32, imm: u64, size: u8) {
    let a = body.local();
    let b = body.local();
    push_masked_reg(body, dst, size);
    body.local_set(a);
    // b = imm & mask(size)
    body.i64_const(imm as i64);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(b);
    emit_alu(body, alu, dst, a, b, size);
}

/// `mov dst, src` (register to register), flag-neutral.
fn emit_mov_reg(body: &mut Body, dst: u32, src: u32, size: u8) {
    let v = body.local();
    push_masked_reg(body, src, size);
    body.local_set(v);
    write_reg(body, dst, size, v);
}

/// `mov dst, imm`, flag-neutral. For size 4 the imm is already a 32-bit value
/// (zero-extended); for size 8 it is the full imm64.
fn emit_mov_imm(body: &mut Body, dst: u32, imm: u64, size: u8) {
    let v = body.local();
    body.i64_const(imm as i64);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(v);
    write_reg(body, dst, size, v);
}

/// `inc`/`dec` r/m (register form). These set OF/SF/ZF/PF from the result but
/// PRESERVE CF (matching the interpreter's 0xFF /0,/1 handler: `flags_arith`
/// with `b=1`, then CF restored).
fn emit_inc_dec(body: &mut Body, dst: u32, sub: bool, size: u8) {
    let a = body.local();
    let b = body.local();
    let r = body.local();
    push_masked_reg(body, dst, size);
    body.local_set(a);
    body.i64_const(1);
    body.local_set(b);
    // r = (a +/- 1) & mask
    body.local_get(a);
    body.local_get(b);
    body.binop(if sub { op::I64_SUB } else { op::I64_ADD });
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(r);

    // Save CF before computing flags; restore it after (inc/dec preserve CF).
    let saved_cf = body.local();
    body.load_rflags();
    body.i64_const(CF as i64);
    body.binop(op::I64_AND);
    body.local_set(saved_cf);

    emit_flags_arith(body, a, b, r, size, sub);

    // rflags = (rflags & !CF) | saved_cf
    body.store_rflags(|bd| {
        bd.load_rflags();
        bd.i64_const(!(CF as i64));
        bd.binop(op::I64_AND);
        bd.local_get(saved_cf);
        bd.binop(op::I64_OR);
    });

    write_reg(body, dst, size, r);
}

// ── Module encoding ───────────────────────────────────────────────────────────

/// Section ids.
mod sec {
    pub const TYPE: u8 = 1;
    pub const IMPORT: u8 = 2;
    pub const FUNCTION: u8 = 3;
    pub const EXPORT: u8 = 7;
    pub const CODE: u8 = 10;
}

/// Append a section: id, then the LEB128 length of `payload`, then `payload`.
fn push_section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    leb_u32(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

/// Encode the full module around the built `run` body.
fn encode_module(body: &Body) -> Vec<u8> {
    let mut out = Vec::new();
    // Magic + version.
    out.extend_from_slice(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);

    // Type section: one type — () -> i32.
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 1); // count
        p.push(0x60); // func type
        leb_u32(&mut p, 0); // 0 params
        leb_u32(&mut p, 1); // 1 result
        p.push(0x7f); // i32
        push_section(&mut out, sec::TYPE, &p);
    }

    // Import section: (import "env" "mem" (memory 1)).
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 1); // 1 import
        let module = b"env";
        leb_u32(&mut p, module.len() as u32);
        p.extend_from_slice(module);
        let name = b"mem";
        leb_u32(&mut p, name.len() as u32);
        p.extend_from_slice(name);
        p.push(0x02); // import kind: memory
        p.push(0x00); // limits: min only (no max)
        leb_u32(&mut p, 1); // min 1 page
        push_section(&mut out, sec::IMPORT, &p);
    }

    // Function section: one function of type 0.
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 1); // 1 function
        leb_u32(&mut p, 0); // type index 0
        push_section(&mut out, sec::FUNCTION, &p);
    }

    // Export section: export "run" func 0.
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 1); // 1 export
        let name = b"run";
        leb_u32(&mut p, name.len() as u32);
        p.extend_from_slice(name);
        p.push(0x00); // export kind: func
        leb_u32(&mut p, 0); // func index 0
        push_section(&mut out, sec::EXPORT, &p);
    }

    // Code section: the single function body.
    {
        // Local declarations: one group of `i64_locals` i64s (if any).
        let mut func = Vec::new();
        if body.i64_locals == 0 {
            leb_u32(&mut func, 0); // 0 local groups
        } else {
            leb_u32(&mut func, 1); // 1 group
            leb_u32(&mut func, body.i64_locals); // count
            func.push(0x7e); // i64
        }
        func.extend_from_slice(&body.code);
        func.push(op::END);

        let mut p = Vec::new();
        leb_u32(&mut p, 1); // 1 function body
        leb_u32(&mut p, func.len() as u32);
        p.extend_from_slice(&func);
        push_section(&mut out, sec::CODE, &p);
    }

    out
}
