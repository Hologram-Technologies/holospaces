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
//! (import "env" "mem"   (memory 1))                          ;; register file
//! (import "env" "load"  (func (param i64 i32) (result i64))) ;; load(addr,size)
//! (import "env" "store" (func (param i64 i32 i64)))          ;; store(addr,size,val)
//! (func (export "run") (param $entry_rip i64) (result i64 i64) ... )
//! ```
//!
//! The guest register file lives in that memory at byte offset 0 as 16
//! little-endian `u64` registers (`r[0..16]`) followed by `rflags` at offset
//! 128. The function reads/writes a register via `i64.load`/`i64.store` at
//! `reg*8` (rflags at 128).
//!
//! ## `run` signature — `(entry_rip) -> (next_rip, insns)`
//!
//! The exported `run` takes the block's entry guest `rip` and returns two
//! `i64`s: the **next guest RIP** control should continue at, and the number of
//! guest instructions executed (the retired-instruction count the interpreter
//! would report). Returning the next RIP — not just a count — is what lets a
//! run-loop chain translated blocks by entry RIP. The `next_rip` is the first
//! result, `insns` the second.
//!
//! A block that *ends in a relative branch* (`JMP`/`Jcc`, the only terminators
//! this translator emits) returns the branch's resolved destination (taken
//! target for `JMP` / a taken `Jcc`, or the fall-through end for a not-taken
//! `Jcc`); the branch instruction itself is counted in `insns`. A block that
//! runs into a non-branch boundary (an unsupported opcode, a `CALL`/`RET`/
//! indirect control transfer, the end of the slice) ends *before* that boundary
//! and returns the fall-through RIP after the last translated instruction. All
//! other control transfers are left to the interpreter (see [`translate_block_at`]).
//!
//! ## Memory operands
//!
//! ModRM memory forms (`mod` 0/1/2, including SIB and RIP-relative) are
//! supported. The translator computes the effective address exactly as the
//! interpreter's [`super::x64::Cpu::modrm`] does — `EA = base + index*scale +
//! disp`, with REX.B/REX.X extending base/index, the `mod==0` `rm==5`
//! RIP-relative (relative to the *end* of the instruction) and SIB `base==5`
//! no-base special cases — and routes every guest memory access through two
//! imported host functions (`env.load` / `env.store`). The host keeps the
//! paging / MMIO / fault semantics (the interpreter's `rd`/`wr`); the JIT only
//! computes addresses. Operand `size_in_bytes` is 4 or 8 (16/8-bit forms still
//! stop the block). Because the absolute RIP-relative address depends on the
//! block's entry `rip`, [`translate_block_at`] takes it (and [`translate_block`]
//! assumes a zero entry `rip`, for register-only blocks where it is irrelevant).
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

/// Byte offset of the **fault-restart RIP** slot in the register file. Before each
/// guest-memory access the block stores the absolute guest `rip` of the
/// instruction performing it here, so that if the host `env.load`/`env.store`
/// import traps (a page fault / MMIO), the JIT driver can resume the interpreter at
/// exactly that instruction — with the register/flag state the regfile already
/// holds (every *prior* instruction's results are committed; this instruction's
/// are not yet, since its memory access is its first or last effect). This is what
/// makes a multi-memory-op block safely restartable after an abort. The slot is
/// written only by blocks that touch memory; register-only blocks never trap.
const FAULT_RIP_OFF: u64 = 136;

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
    /// Whether the block emits any guest-memory access (an `env.load`/`env.store`
    /// import call). A register-only block (`false`) needs no host memory primitive
    /// at run time, so the JIT driver can execute it without lending the `Cpu` to
    /// the host imports — a cheaper fast path.
    pub touches_mem: bool,
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
    pub const CALL: u8 = 0x10;
    pub const SELECT: u8 = 0x1b;
    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const I64_LOAD: u8 = 0x29;
    pub const I64_STORE: u8 = 0x37;
    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;
    pub const I32_EQZ: u8 = 0x45;
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
    /// Number of extra `i64` locals (beyond the single `entry_rip` parameter)
    /// declared.
    i64_locals: u32,
    /// Set once the body emits a guest-memory access (a `env.load`/`env.store`
    /// call) — surfaced as [`TranslatedBlock::touches_mem`].
    touches_mem: bool,
    /// The block-relative byte offset of the instruction currently being emitted —
    /// stamped into the [`FAULT_RIP_OFF`] slot before each memory access so an abort
    /// can be resumed at this instruction. Set by [`decode_one`] per instruction.
    cur_insn_off: usize,
}

/// Wasm function index of the imported `env.load(addr,size)->i64`.
const FN_LOAD: u32 = 0;
/// Wasm function index of the imported `env.store(addr,size,val)`.
const FN_STORE: u32 = 1;

/// Number of parameters of the exported `run` function. Index 0 is `$entry_rip`
/// (an `i64`); all reserved temporaries are locals after it.
const PARAM_COUNT: u32 = 1;
/// Local index of the `$entry_rip` parameter.
const ENTRY_RIP_LOCAL: u32 = 0;

impl Body {
    fn new() -> Self {
        Body {
            code: Vec::new(),
            i64_locals: 0,
            touches_mem: false,
            cur_insn_off: 0,
        }
    }

    /// Stamp the absolute guest `rip` of the current instruction
    /// (`entry_rip + cur_insn_off`) into the [`FAULT_RIP_OFF`] slot. Emitted before
    /// every guest-memory access so a trapped block resumes at the right
    /// instruction (see [`FAULT_RIP_OFF`]).
    fn store_fault_rip(&mut self) {
        let off = self.cur_insn_off as u64;
        self.i32_const(0); // address operand for the i64.store
        self.local_get(ENTRY_RIP_LOCAL);
        if off != 0 {
            self.i64_const(off as i64);
            self.binop(op::I64_ADD);
        }
        self.byte(op::I64_STORE);
        leb_u32(&mut self.code, 3); // align 8
        leb_u64(&mut self.code, FAULT_RIP_OFF);
    }

    /// Reserve a fresh `i64` local, returning its index. Local index 0 is the
    /// `entry_rip` parameter, so the first reserved local is index 1.
    fn local(&mut self) -> u32 {
        let idx = PARAM_COUNT + self.i64_locals;
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

    /// Emit a `call` to function index `f`.
    fn call(&mut self, f: u32) {
        self.byte(op::CALL);
        leb_u32(&mut self.code, f);
    }

    /// Emit `env.load(addr, size)` where the address is in `addr_local` and the
    /// loaded (host-zero-extended) value is left on the stack.
    fn emit_load(&mut self, addr_local: u32, size: u8) {
        self.touches_mem = true;
        self.store_fault_rip();
        self.local_get(addr_local);
        self.i32_const(i32::from(size));
        self.call(FN_LOAD);
    }

    /// Emit `env.store(addr, size, value)` where the address is in `addr_local`
    /// and the value is produced by `f` (pushed after the address/size operands,
    /// as the import's signature requires `(addr, size, value)`).
    fn emit_store(&mut self, addr_local: u32, size: u8, f: impl FnOnce(&mut Self)) {
        self.touches_mem = true;
        self.store_fault_rip();
        self.local_get(addr_local);
        self.i32_const(i32::from(size));
        f(self);
        self.call(FN_STORE);
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

// ── Condition codes + branch terminators ──────────────────────────────────────

/// Push the boolean (i64 0/1) value of a single RFLAGS bit (`mask` is the bit's
/// mask, a single set bit) onto the stack: `(rflags & mask) != 0`.
fn push_flag_bool(body: &mut Body, mask: u64) {
    body.load_rflags();
    body.i64_const(mask as i64);
    body.binop(op::I64_AND);
    let pos = mask.trailing_zeros();
    if pos != 0 {
        body.i64_const(i64::from(pos));
        body.binop(op::I64_SHR_U);
    }
}

/// Emit the *base* condition (i64 0/1) for `cc >> 1`, mirroring the
/// interpreter's [`super::x64::Cpu::cond`]:
///   0=O(of) 1=B(cf) 2=E(zf) 3=BE(cf|zf) 4=S(sf) 5=P(pf) 6=L(sf!=of) 7=LE((sf!=of)|zf)
fn emit_cond_base(body: &mut Body, group: u8) {
    match group {
        0 => push_flag_bool(body, OF), // O
        1 => push_flag_bool(body, CF), // B / C
        2 => push_flag_bool(body, ZF), // E / Z
        3 => {
            // BE = cf | zf
            push_flag_bool(body, CF);
            push_flag_bool(body, ZF);
            body.binop(op::I64_OR);
        }
        4 => push_flag_bool(body, SF), // S
        5 => push_flag_bool(body, PF), // P
        6 => {
            // L = sf != of
            push_flag_bool(body, SF);
            push_flag_bool(body, OF);
            body.binop(op::I64_NE);
            body.binop(op::I64_EXTEND_I32_U);
        }
        _ => {
            // LE = (sf != of) | zf
            push_flag_bool(body, SF);
            push_flag_bool(body, OF);
            body.binop(op::I64_NE);
            body.binop(op::I64_EXTEND_I32_U);
            push_flag_bool(body, ZF);
            body.binop(op::I64_OR);
        }
    }
}

/// Emit the full condition `cc` (the low nibble of a `Jcc` opcode) as an **i32**
/// 0/1 on the stack (usable directly as a `select` predicate). `cc & 1` inverts
/// the base, exactly as the interpreter does.
fn emit_cond_i32(body: &mut Body, cc: u8) {
    emit_cond_base(body, cc >> 1);
    // base is i64 0/1 → i32 0/1 via (base != 0).
    body.binop(op::I64_EQZ); // i32: 1 if base == 0
    if cc & 1 == 0 {
        // Want (base != 0): negate the eqz result.
        body.binop(op::I32_EQZ);
    }
    // If cc&1==1 (inverted condition) we want !base == (base == 0), which is
    // exactly the I64_EQZ result already on the stack.
}

/// Push `entry_rip + off` (the absolute guest RIP for a block-relative offset)
/// onto the stack. Branch targets are computed against the runtime `$entry_rip`
/// parameter so a translated block is position-independent.
fn push_rip_at(body: &mut Body, off: u64) {
    body.local_get(ENTRY_RIP_LOCAL);
    body.i64_const(off as i64);
    body.binop(op::I64_ADD);
}

/// Emit an *unconditional* branch terminator: `next_rip = entry_rip + taken_off`
/// (the resolved jump destination as a block-relative offset).
fn emit_jmp(body: &mut Body, next_rip_local: u32, taken_off: u64) {
    push_rip_at(body, taken_off);
    body.local_set(next_rip_local);
}

/// Emit a *conditional* branch terminator: `next_rip = cond ? entry+taken_off :
/// entry+fallthru_off`, selected at runtime from the block's computed RFLAGS.
fn emit_jcc(body: &mut Body, next_rip_local: u32, cc: u8, taken_off: u64, fallthru_off: u64) {
    // select pops (taken, fallthru, cond_i32) → taken if cond else fallthru.
    push_rip_at(body, taken_off);
    push_rip_at(body, fallthru_off);
    emit_cond_i32(body, cc);
    body.byte(op::SELECT);
    body.local_set(next_rip_local);
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

/// A decoded effective-address recipe for a ModRM memory operand, mirroring
/// [`super::x64::Cpu::modrm`]'s computation: `EA = base + index*scale + disp`
/// with the `mod==0` `rm==5` RIP-relative and SIB `base==5` no-base special
/// cases. Resolved to an `i64` address by [`Body`] code at emit time.
struct MemEa {
    /// Base register (REX.B-extended), or `None` for the no-base SIB form and
    /// the RIP-relative form.
    base: Option<u32>,
    /// Index register (REX.X-extended) shifted by `scale`, or `None`.
    index: Option<u32>,
    /// SIB scale shift (0..=3); only meaningful when `index` is `Some`.
    scale: u8,
    /// Displacement. For a RIP-relative operand (`rip_rel`), this is the raw
    /// disp32; the absolute address is `entry_rip + insn_end_off + disp`.
    disp: i64,
    /// Whether this is the `mod==0` `rm==5` RIP-relative form.
    rip_rel: bool,
}

/// A decoded ModRM operand: the (REX-extended) `reg` field plus the r/m operand,
/// which is either a register or a memory effective address.
struct ModRm {
    reg: u32,
    rm: RmLoc,
}

/// The r/m operand location: a register index or a memory effective address.
enum RmLoc {
    Reg(u32),
    Mem(MemEa),
}

/// Decode the ModRM (and any SIB / displacement) into `(reg, rm)`. Returns
/// `None` only on truncation. Memory forms are now *supported* (`RmLoc::Mem`),
/// matching the interpreter's [`super::x64::Cpu::modrm`] byte-for-byte.
fn decode_modrm(dec: &mut Decoder, rex: u8) -> Option<ModRm> {
    let m = dec.u8()?;
    let md = m >> 6;
    let reg = u32::from((m >> 3) & 7) | (u32::from((rex >> 2) & 1) << 3); // REX.R
    let rm_field = m & 7;
    if md == 3 {
        let rm = u32::from(rm_field) | (u32::from(rex & 1) << 3); // REX.B
        return Some(ModRm {
            reg,
            rm: RmLoc::Reg(rm),
        });
    }

    // Memory operand.
    let base: Option<u32>;
    let mut index: Option<u32> = None;
    let mut scale: u8 = 0;
    let mut disp: i64 = 0;

    if rm_field == 4 {
        // SIB byte.
        let sib = dec.u8()?;
        scale = sib >> 6;
        let idx = u32::from((sib >> 3) & 7) | (u32::from((rex >> 1) & 1) << 3); // REX.X
        let base_field = u32::from(sib & 7) | (u32::from(rex & 1) << 3); // REX.B
        if idx != 4 {
            index = Some(idx); // index==4 (no REX.X) means "no index"
        }
        if (sib & 7) == 5 && md == 0 {
            // disp32, no base.
            base = None;
            disp = i64::from(dec.u32_le()? as i32);
        } else {
            base = Some(base_field);
        }
    } else if rm_field == 5 && md == 0 {
        // RIP-relative: disp32 relative to the instruction-end rip.
        let d = i64::from(dec.u32_le()? as i32);
        return Some(ModRm {
            reg,
            rm: RmLoc::Mem(MemEa {
                base: None,
                index: None,
                scale: 0,
                disp: d,
                rip_rel: true,
            }),
        });
    } else {
        base = Some(u32::from(rm_field) | (u32::from(rex & 1) << 3)); // REX.B
    }

    match md {
        1 => disp = disp.wrapping_add(i64::from(dec.u8()? as i8)),
        2 => disp = disp.wrapping_add(i64::from(dec.u32_le()? as i32)),
        _ => {}
    }

    Some(ModRm {
        reg,
        rm: RmLoc::Mem(MemEa {
            base,
            index,
            scale,
            disp,
            rip_rel: false,
        }),
    })
}

/// Emit code that computes the effective address of `ea` into a fresh i64 local,
/// returning the local index. `insn_end_off` is the offset of the *end* of the
/// current instruction within the block (the interpreter's instruction-end
/// `rip` offset), used to resolve a RIP-relative operand against `entry_rip`.
fn emit_ea(body: &mut Body, ea: &MemEa, insn_end_off: usize) -> u32 {
    let addr = body.local();
    if ea.rip_rel {
        // Absolute address = entry_rip + insn_end_off + disp (seg base is 0 in
        // the flat long-mode segments this translator targets). `entry_rip` is
        // the runtime `$entry_rip` parameter so a block re-used at a different
        // entry resolves RIP-relative operands against the call's RIP.
        body.local_get(ENTRY_RIP_LOCAL);
        body.i64_const((insn_end_off as u64).wrapping_add(ea.disp as u64) as i64);
        body.binop(op::I64_ADD);
        body.local_set(addr);
        return addr;
    }
    // Start from the base (or 0), add index*scale, add disp — all wrapping i64.
    if let Some(b) = ea.base {
        body.load_reg(b);
    } else {
        body.i64_const(0);
    }
    if let Some(i) = ea.index {
        body.load_reg(i);
        if ea.scale != 0 {
            body.i64_const(i64::from(ea.scale));
            body.binop(op::I64_SHL);
        }
        body.binop(op::I64_ADD);
    }
    if ea.disp != 0 {
        body.i64_const(ea.disp);
        body.binop(op::I64_ADD);
    }
    body.local_set(addr);
    addr
}

/// Read the value of an r/m operand into a fresh i64 local, returning its index.
/// A register operand is masked to `size`; a memory operand is fetched via the
/// host `env.load(EA, size)` (which already zero-extends), then masked to `size`
/// for parity with the interpreter's `& mask(size)`. `addr_local` must hold the
/// pre-computed EA for the memory form.
fn read_rm(body: &mut Body, rm: &RmLoc, size: u8, addr_local: Option<u32>) -> u32 {
    let v = body.local();
    match rm {
        RmLoc::Reg(reg) => {
            push_masked_reg(body, *reg, size);
        }
        RmLoc::Mem(_) => {
            body.emit_load(addr_local.expect("memory operand needs an EA"), size);
            if size < 8 {
                body.i64_const(size_mask(size));
                body.binop(op::I64_AND);
            }
        }
    }
    body.local_set(v);
    v
}

/// Write `val_local` into an r/m operand. A register destination honours the
/// x86-64 zero-extension rule (a 32-bit write clears the upper 32 bits); a memory
/// destination stores `size` bytes via `env.store(EA, size, val & mask)`.
fn write_rm(body: &mut Body, rm: &RmLoc, size: u8, addr_local: Option<u32>, val_local: u32) {
    match rm {
        RmLoc::Reg(reg) => write_reg(body, *reg, size, val_local),
        RmLoc::Mem(_) => {
            let addr = addr_local.expect("memory operand needs an EA");
            body.emit_store(addr, size, |b| {
                b.local_get(val_local);
                if size < 8 {
                    b.i64_const(size_mask(size));
                    b.binop(op::I64_AND);
                }
            });
        }
    }
}

/// Emit one ALU op `dst (op)= src_b`. `dst` is the destination operand (a
/// register or a memory location, skipped for CMP); `a_local` holds the
/// destination's current value, `b_local` the source. `dst_addr` is the
/// pre-computed EA when `dst` is memory. Flags are set per the op.
#[allow(clippy::too_many_arguments)]
fn emit_alu(
    body: &mut Body,
    op: AluOp,
    dst: &RmLoc,
    dst_addr: Option<u32>,
    a_local: u32,
    b_local: u32,
    size: u8,
) {
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
        write_rm(body, dst, size, dst_addr, r);
    }
}

/// Translate a linear run of supported instructions starting at the block entry.
///
/// `code` is a contiguous slice of guest machine code from the block entry. The
/// translator decodes supported register-direct integer instructions until it
/// hits an unsupported byte, a non-branch control transfer, a 16-bit operand, a
/// relative branch (`JMP`/`Jcc` — which it *includes* as the block terminator),
/// or the end of the slice, then emits **one** Wasm module covering the
/// instructions it consumed. Returns `None` if the *first* instruction is
/// unsupported (the caller interprets that instruction instead).
#[must_use]
pub fn translate_block(code: &[u8]) -> Option<TranslatedBlock> {
    translate_block_at(code, 0)
}

/// Translate a block whose entry guest `rip` is `entry_rip`. Identical to
/// [`translate_block`] except RIP-relative memory operands and relative branch
/// targets are resolved against `entry_rip`. Note that `entry_rip` is also passed
/// to `run` at call time (the `$entry_rip` parameter), so a register-only block
/// with no RIP-relative operand or branch is insensitive to the value baked here.
#[must_use]
pub fn translate_block_at(code: &[u8], entry_rip: u64) -> Option<TranslatedBlock> {
    let _ = entry_rip; // RIP is supplied at call time via the `$entry_rip` param.
    let mut body = Body::new();
    // `next_rip` accumulator local — branches overwrite it; otherwise it ends as
    // the fall-through RIP. Reserved first so its index is stable.
    let next_rip_local = body.local();
    let mut dec = Decoder::new(code);
    let mut insns: u32 = 0;
    let mut terminated = false;

    loop {
        let start = dec.pos;
        if dec.peek().is_none() {
            break; // end of slice
        }
        // Record this instruction's block-relative offset so a memory access can
        // stamp its absolute rip for fault restart (see `Body::store_fault_rip`).
        body.cur_insn_off = start;
        match decode_one(&mut dec, &mut body, next_rip_local) {
            DecodeResult::Ok => {
                insns += 1;
            }
            DecodeResult::Terminator => {
                // A relative JMP/Jcc: included in the block; it has set
                // `next_rip_local`. End the block here.
                insns += 1;
                terminated = true;
                break;
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

    if !terminated {
        // Fall-through: next_rip = entry_rip + bytes consumed.
        emit_jmp(&mut body, next_rip_local, dec.pos as u64);
    }

    // `run` returns (next_rip, insns).
    body.local_get(next_rip_local);
    body.i64_const(i64::from(insns));

    let touches_mem = body.touches_mem;
    let wasm = encode_module(&body);
    Some(TranslatedBlock {
        wasm,
        insns,
        bytes: dec.pos as u32,
        touches_mem,
    })
}

enum DecodeResult {
    /// A non-control instruction was decoded and emitted; continue the block.
    Ok,
    /// A relative branch (`JMP`/`Jcc`) was decoded and emitted as the block
    /// terminator (it set `next_rip`); end the block, counting this instruction.
    Terminator,
    /// The instruction is unsupported / a non-branch control transfer; end the
    /// block *before* it (roll back, fall through to the interpreter).
    Stop,
}

/// Decode and emit one instruction. On `Stop`, `body` may have had partial code
/// appended — but `translate_block` only emits once a full instruction succeeds,
/// because a `Stop` ends the loop and the trailing (return-value) code is the
/// only thing appended after the last good instruction. Each `Ok`/`Terminator`
/// path appends a complete, self-contained instruction. `next_rip_local` is the
/// accumulator a branch terminator writes its resolved destination into.
fn decode_one(dec: &mut Decoder, body: &mut Body, next_rip_local: u32) -> DecodeResult {
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
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let Some(alu) = AluOp::from_digit(op >> 3) else {
                return DecodeResult::Stop;
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_alu_rm_reg(body, alu, &d.rm, addr, d.reg, size);
            DecodeResult::Ok
        }
        0x03 | 0x0b | 0x23 | 0x2b | 0x33 | 0x3b => {
            // op r, r/m  : dst = reg, a = reg, b = r/m
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let Some(alu) = AluOp::from_digit(op >> 3) else {
                return DecodeResult::Stop;
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_alu_reg_rm(body, alu, d.reg, &d.rm, addr, size);
            DecodeResult::Ok
        }
        // ── group1: 0x81 /digit imm32-sext, 0x83 /digit imm8-sext ──
        0x81 | 0x83 => {
            let Some(d) = decode_modrm(dec, rex) else {
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
            // EA resolves against the instruction-end rip (after the immediate),
            // exactly as the interpreter fetches the immediate before load_rm.
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_alu_rm_imm(body, alu, &d.rm, addr, imm, size);
            DecodeResult::Ok
        }
        // ── mov r/m, r (0x89) ──
        0x89 => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_mov_rm_reg(body, &d.rm, addr, d.reg, size);
            DecodeResult::Ok
        }
        // ── mov r, r/m (0x8B) ──
        0x8b => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_mov_reg_rm(body, d.reg, &d.rm, addr, size);
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
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            if d.reg & 7 != 0 {
                return DecodeResult::Stop; // only /0 is MOV
            }
            let Some(v) = dec.u32_le() else {
                return DecodeResult::Stop;
            };
            let imm = (v as i32 as i64) as u64;
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_mov_rm_imm(body, &d.rm, addr, imm, size);
            DecodeResult::Ok
        }
        // ── inc/dec via 0xFF /0 (inc) and /1 (dec) ──
        0xff => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let digit = d.reg & 7;
            if digit != 0 && digit != 1 {
                return DecodeResult::Stop; // call/jmp/push — control flow
            }
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_inc_dec(body, &d.rm, addr, digit == 1, size);
            DecodeResult::Ok
        }
        // ── relative branch terminators (these END the block, included) ──
        // JMP rel8 (0xEB): next_rip = instruction_end + sext(rel8).
        0xeb => {
            let Some(rel) = dec.u8() else {
                return DecodeResult::Stop;
            };
            let taken = (dec.pos as u64).wrapping_add(rel as i8 as i64 as u64);
            emit_jmp(body, next_rip_local, taken);
            DecodeResult::Terminator
        }
        // JMP rel32 (0xE9): next_rip = instruction_end + sext(rel32).
        0xe9 => {
            let Some(rel) = dec.u32_le() else {
                return DecodeResult::Stop;
            };
            let taken = (dec.pos as u64).wrapping_add(rel as i32 as i64 as u64);
            emit_jmp(body, next_rip_local, taken);
            DecodeResult::Terminator
        }
        // Jcc rel8 (0x70..0x7F): taken iff cond(op-0x70); fall-through otherwise.
        0x70..=0x7f => {
            let Some(rel) = dec.u8() else {
                return DecodeResult::Stop;
            };
            let fallthru = dec.pos as u64;
            let taken = fallthru.wrapping_add(rel as i8 as i64 as u64);
            emit_jcc(body, next_rip_local, op - 0x70, taken, fallthru);
            DecodeResult::Terminator
        }
        // 0x0F two-byte: only Jcc rel32 (0x80..0x8F) is a supported terminator;
        // every other 0x0F opcode stops the block (interpreter handles it).
        0x0f => {
            let Some(op2) = dec.u8() else {
                return DecodeResult::Stop;
            };
            if (0x80..=0x8f).contains(&op2) {
                let Some(rel) = dec.u32_le() else {
                    return DecodeResult::Stop;
                };
                let fallthru = dec.pos as u64;
                let taken = fallthru.wrapping_add(rel as i32 as i64 as u64);
                emit_jcc(body, next_rip_local, op2 - 0x80, taken, fallthru);
                DecodeResult::Terminator
            } else {
                DecodeResult::Stop
            }
        }
        _ => DecodeResult::Stop,
    }
}

/// If `rm` is a memory operand, emit code computing its EA into a fresh local and
/// return that local; for a register operand return `None`. `insn_end_off` is the
/// decoder position at the *end* of the instruction (for RIP-relative resolution).
fn maybe_ea(body: &mut Body, rm: &RmLoc, insn_end_off: usize) -> Option<u32> {
    match rm {
        RmLoc::Reg(_) => None,
        RmLoc::Mem(ea) => Some(emit_ea(body, ea, insn_end_off)),
    }
}

/// `op rm, reg` (digit forms 0x01.. and the digit-in-rm sense): the destination
/// is `rm` (its current value is operand `a`), the source is register `reg_src`.
/// `rm_addr` is the pre-computed EA when `rm` is memory.
fn emit_alu_rm_reg(
    body: &mut Body,
    alu: AluOp,
    rm: &RmLoc,
    rm_addr: Option<u32>,
    reg_src: u32,
    size: u8,
) {
    let a = read_rm(body, rm, size, rm_addr);
    let b = body.local();
    push_masked_reg(body, reg_src, size);
    body.local_set(b);
    emit_alu(body, alu, rm, rm_addr, a, b, size);
}

/// `op reg, rm` (0x03.. forms): the destination is register `reg_dst` (its value
/// is operand `a`), the source is `rm`. `rm_addr` is the EA when `rm` is memory.
fn emit_alu_reg_rm(
    body: &mut Body,
    alu: AluOp,
    reg_dst: u32,
    rm: &RmLoc,
    rm_addr: Option<u32>,
    size: u8,
) {
    let a = body.local();
    push_masked_reg(body, reg_dst, size);
    body.local_set(a);
    let b = read_rm(body, rm, size, rm_addr);
    emit_alu(body, alu, &RmLoc::Reg(reg_dst), None, a, b, size);
}

/// `op rm, imm` (group-1 0x81/0x83; imm already sign-extended to a u64). The
/// destination and operand `a` are `rm`; the source is the immediate.
fn emit_alu_rm_imm(
    body: &mut Body,
    alu: AluOp,
    rm: &RmLoc,
    rm_addr: Option<u32>,
    imm: u64,
    size: u8,
) {
    let a = read_rm(body, rm, size, rm_addr);
    let b = body.local();
    // b = imm & mask(size)
    body.i64_const(imm as i64);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(b);
    emit_alu(body, alu, rm, rm_addr, a, b, size);
}

/// `mov dst, src` where `src` is a register and `dst` is an r/m operand (0x89),
/// flag-neutral.
fn emit_mov_rm_reg(body: &mut Body, dst: &RmLoc, dst_addr: Option<u32>, src: u32, size: u8) {
    let v = body.local();
    push_masked_reg(body, src, size);
    body.local_set(v);
    write_rm(body, dst, size, dst_addr, v);
}

/// `mov reg, src` where `src` is an r/m operand (0x8B), flag-neutral.
fn emit_mov_reg_rm(body: &mut Body, dst: u32, src: &RmLoc, src_addr: Option<u32>, size: u8) {
    let v = read_rm(body, src, size, src_addr);
    write_reg(body, dst, size, v);
}

/// `mov dst, imm` where `dst` is an r/m operand (0xC7 /0), flag-neutral.
fn emit_mov_rm_imm(body: &mut Body, dst: &RmLoc, dst_addr: Option<u32>, imm: u64, size: u8) {
    let v = body.local();
    body.i64_const(imm as i64);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(v);
    write_rm(body, dst, size, dst_addr, v);
}

/// `mov reg, imm` (0xB8+r — always a register destination), flag-neutral. For
/// size 4 the imm is already a 32-bit value (zero-extended); for size 8 it is
/// the full imm64.
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

/// `inc`/`dec` r/m. These set OF/SF/ZF/PF from the result but PRESERVE CF
/// (matching the interpreter's 0xFF /0,/1 handler: `flags_arith` with `b=1`,
/// then CF restored). `dst_addr` is the EA when `dst` is memory.
fn emit_inc_dec(body: &mut Body, dst: &RmLoc, dst_addr: Option<u32>, sub: bool, size: u8) {
    let a = read_rm(body, dst, size, dst_addr);
    let b = body.local();
    let r = body.local();
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

    write_rm(body, dst, size, dst_addr, r);
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

    // Type section — three types:
    //   0: (i64) -> (i64, i64)        (the exported `run`: entry_rip -> next_rip,insns)
    //   1: (i64, i32) -> i64          (env.load(addr, size) -> value)
    //   2: (i64, i32, i64) -> ()      (env.store(addr, size, value))
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 3); // 3 types
                            // type 0: (i64) -> (i64, i64)
        p.push(0x60);
        leb_u32(&mut p, 1); // 1 param
        p.push(0x7e); // i64 ($entry_rip)
        leb_u32(&mut p, 2); // 2 results
        p.push(0x7e); // i64 (next_rip)
        p.push(0x7e); // i64 (insns)
                      // type 1: (i64, i32) -> i64
        p.push(0x60);
        leb_u32(&mut p, 2); // 2 params
        p.push(0x7e); // i64
        p.push(0x7f); // i32
        leb_u32(&mut p, 1); // 1 result
        p.push(0x7e); // i64
                      // type 2: (i64, i32, i64) -> ()
        p.push(0x60);
        leb_u32(&mut p, 3); // 3 params
        p.push(0x7e); // i64
        p.push(0x7f); // i32
        p.push(0x7e); // i64
        leb_u32(&mut p, 0); // 0 results
        push_section(&mut out, sec::TYPE, &p);
    }

    // Import section:
    //   (import "env" "mem"   (memory 1))
    //   (import "env" "load"  (func (type 1)))   ;; func index 0
    //   (import "env" "store" (func (type 2)))   ;; func index 1
    {
        let push_name = |p: &mut Vec<u8>, s: &[u8]| {
            leb_u32(p, s.len() as u32);
            p.extend_from_slice(s);
        };
        let mut p = Vec::new();
        leb_u32(&mut p, 3); // 3 imports
                            // memory
        push_name(&mut p, b"env");
        push_name(&mut p, b"mem");
        p.push(0x02); // import kind: memory
        p.push(0x00); // limits: min only
        leb_u32(&mut p, 1); // min 1 page
                            // load
        push_name(&mut p, b"env");
        push_name(&mut p, b"load");
        p.push(0x00); // import kind: func
        leb_u32(&mut p, 1); // type index 1
                            // store
        push_name(&mut p, b"env");
        push_name(&mut p, b"store");
        p.push(0x00); // import kind: func
        leb_u32(&mut p, 2); // type index 2
        push_section(&mut out, sec::IMPORT, &p);
    }

    // Function section: one defined function of type 0 (becomes func index 2,
    // after the two imported functions).
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 1); // 1 function
        leb_u32(&mut p, 0); // type index 0
        push_section(&mut out, sec::FUNCTION, &p);
    }

    // Export section: export "run" → func index 2 (the defined function).
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 1); // 1 export
        let name = b"run";
        leb_u32(&mut p, name.len() as u32);
        p.extend_from_slice(name);
        p.push(0x00); // export kind: func
        leb_u32(&mut p, 2); // func index 2
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
