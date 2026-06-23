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
//! the whole DBT. It decodes a **linear** run of integer instructions (the common
//! hot-loop shape) — ALU/MOV (register and memory operands), INC/DEC, PUSH/POP
//! (register and r/m, via the stack-memory path), LEA, TEST, MOVZX/MOVSX/MOVSXD,
//! the SHL/SHR/SAR shifts, SETcc, NEG/NOT, and IMUL (2-/3-operand) plus the
//! single-operand MUL/IMUL writing RDX:RAX — and stops at the first thing it does
//! not handle (a non-branch control transfer, a 16-bit (`0x66`) or 8-bit operand,
//! DIV/IDIV or the rotates, any other unsupported opcode, or the end of the
//! slice), leaving the interpreter to take over there. Relative `JMP`/`Jcc` are
//! included as block terminators. Every instruction it does emit is validated
//! **bit-for-bit** against the interpreter — the qemu-validated authority
//! ([`super::x64::Cpu`], CC-44) — by the differential test (`tests/cc48_jit.rs`).
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

/// Byte offset of the **retired-instruction** slot in the register file. A
/// *region* (see [`translate_region_at`]) stamps the number of guest instructions
/// it has fully retired *before* the instruction it is about to execute a memory
/// access for, alongside [`FAULT_RIP_OFF`]. If that access traps, the driver reads
/// this slot to credit the timer/interrupt bookkeeping with the instructions the
/// region actually ran before the abort (a region can chain many blocks in one
/// call, so unlike a single basic block the lost-on-trap count is not negligible).
const RETIRED_OFF: u64 = 144;

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

/// A translated **region** (trace): one WebAssembly function covering several
/// basic blocks reachable from a hot entry by *direct* branches, with an internal
/// `br_table` dispatch loop so a hot guest loop runs entirely inside ONE Wasm call
/// (no per-block driver round-trip). The exported `run` has signature
/// `(entry_rip: i64, budget: i64) -> (exit_rip: i64, insns: i64)` — it runs guest
/// instructions starting at `entry_rip` (region block 0) until it either leaves the
/// region (an indirect/out-of-region transfer or an unsupported instruction) or the
/// instruction `budget` is exhausted, returning the guest `rip` to continue at and
/// the number of guest instructions it retired. See [`translate_region_at`].
pub struct TranslatedRegion {
    /// The encoded WebAssembly module (importing `env.mem`, exporting `run`).
    pub wasm: Vec<u8>,
    /// The number of guest code bytes the region spans from the entry (the SMC
    /// invalidation extent — a write anywhere in `[entry, entry+bytes)` stales it).
    pub bytes: u32,
    /// The number of basic blocks discovered and emitted in the region.
    pub blocks: u32,
    /// The total number of guest instructions across all discovered blocks (a
    /// region-size probe; the *runtime* retired count is the `run` second result).
    pub insns: u32,
    /// Whether any block emits a guest-memory access (see [`TranslatedBlock`]).
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
    pub const BLOCK: u8 = 0x02;
    pub const LOOP: u8 = 0x03;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x0b;
    pub const BR: u8 = 0x0c;
    pub const BR_TABLE: u8 = 0x0e;
    pub const RETURN: u8 = 0x0f;
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
    pub const I64_LE_S: u8 = 0x57;
    pub const I64_ADD: u8 = 0x7c;
    pub const I64_SUB: u8 = 0x7d;
    pub const I64_MUL: u8 = 0x7e;
    pub const I64_AND: u8 = 0x83;
    pub const I64_OR: u8 = 0x84;
    pub const I64_XOR: u8 = 0x85;
    pub const I64_SHL: u8 = 0x86;
    pub const I64_SHR_S: u8 = 0x87;
    pub const I64_SHR_U: u8 = 0x88;
    pub const I64_POPCNT: u8 = 0x7b;
    pub const I32_WRAP_I64: u8 = 0xa7;
    pub const I64_EXTEND_I32_U: u8 = 0xad;
    pub const I64_EXTEND8_S: u8 = 0xc2;
    pub const I64_EXTEND16_S: u8 = 0xc3;
    pub const I64_EXTEND32_S: u8 = 0xc4;
}

// ── Function body builder ─────────────────────────────────────────────────────

/// Builds the body (instruction stream) of the `run` function. Temporaries are
/// `i64` locals allocated on demand; the body operates on the register file in
/// the imported memory.
struct Body {
    code: Vec<u8>,
    /// Number of extra `i64` locals (beyond the function parameters) declared.
    i64_locals: u32,
    /// Number of `i64` parameters of the function this body belongs to: 1 for a
    /// single block (`$entry_rip`), 2 for a region (`$entry_rip`, `$budget`).
    /// On-demand locals are numbered after the parameters.
    param_count: u32,
    /// Set once the body emits a guest-memory access (a `env.load`/`env.store`
    /// call) — surfaced as [`TranslatedBlock::touches_mem`].
    touches_mem: bool,
    /// The block-relative byte offset of the instruction currently being emitted —
    /// stamped into the [`FAULT_RIP_OFF`] slot before each memory access so an abort
    /// can be resumed at this instruction. Set by [`decode_one`] per instruction.
    cur_insn_off: usize,
    /// For a *region* body: the local holding the running count of guest
    /// instructions retired *before* the current instruction, stamped into
    /// [`RETIRED_OFF`] before each memory access so a trap credits the timer with
    /// the work the region actually did. `None` for a single-block body (which
    /// retires at most one block, so the lost-on-trap count is negligible).
    retired_local: Option<u32>,
}

/// Wasm function index of the imported `env.load(addr,size)->i64`.
const FN_LOAD: u32 = 0;
/// Wasm function index of the imported `env.store(addr,size,val)`.
const FN_STORE: u32 = 1;

/// Local index of the `$entry_rip` parameter (parameter 0 in both the single-block
/// and the region `run` functions).
const ENTRY_RIP_LOCAL: u32 = 0;
/// Local index of the `$budget` parameter (region `run` only — parameter 1).
const BUDGET_LOCAL: u32 = 1;

impl Body {
    fn new() -> Self {
        Body {
            code: Vec::new(),
            i64_locals: 0,
            param_count: 1,
            touches_mem: false,
            cur_insn_off: 0,
            retired_local: None,
        }
    }

    /// A body for a *region* `run` function (`$entry_rip`, `$budget` parameters).
    fn new_region() -> Self {
        Body {
            code: Vec::new(),
            i64_locals: 0,
            param_count: 2,
            touches_mem: false,
            cur_insn_off: 0,
            retired_local: None,
        }
    }

    /// Stamp the absolute guest `rip` of the current instruction
    /// (`entry_rip + cur_insn_off`) into the [`FAULT_RIP_OFF`] slot. Emitted before
    /// every guest-memory access so a trapped block resumes at the right
    /// instruction (see [`FAULT_RIP_OFF`]). For a region body it also stamps the
    /// instructions-retired-so-far into [`RETIRED_OFF`] so a trap credits the timer.
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

        if let Some(rl) = self.retired_local {
            // RETIRED_OFF = instructions fully retired before this instruction.
            self.i32_const(0);
            self.local_get(rl);
            self.byte(op::I64_STORE);
            leb_u32(&mut self.code, 3);
            leb_u64(&mut self.code, RETIRED_OFF);
        }
    }

    /// Reserve a fresh `i64` local, returning its index. Locals are numbered after
    /// the function's parameters (`param_count`).
    fn local(&mut self) -> u32 {
        let idx = self.param_count + self.i64_locals;
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
/// 4 → low 32, 2 → low 16, 1 → low 8. The 1/2-byte masks are only used by the
/// narrow *source* read of `MOVZX`/`MOVSX`/`MOVSXD` and the narrow *destination*
/// write of `SETcc` (general 8/16-bit ALU is still deferred — the block stops).
fn size_mask(size: u8) -> i64 {
    match size {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => -1, // 8: u64::MAX as i64
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

// ── Narrow (8/16-bit) source reads for MOVZX/MOVSX/MOVSXD ──────────────────────

/// Read an 8/16-bit r/m *source* into a fresh i64 local (zero-extended), returning
/// its index. Mirrors the interpreter's `load_rm(rm, size)` for `size` 1/2: a
/// register source is masked to `size`, EXCEPT the legacy AH/CH/DH/BH high-byte
/// encoding (`size==1`, no `REX`, register field `4..=7` → `bits[15:8]` of the low
/// register); a memory source is fetched via `env.load(EA, size)`. Used only by
/// MOVZX/MOVSX/MOVSXD, whose narrow read is explicitly allowed in this increment.
fn read_rm_narrow(
    body: &mut Body,
    rm: &RmLoc,
    size: u8,
    addr_local: Option<u32>,
    rex_present: bool,
) -> u32 {
    let v = body.local();
    match rm {
        RmLoc::Reg(reg) => {
            if size == 1 && !rex_present && (4..8).contains(reg) {
                // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                body.load_reg(reg - 4);
                body.i64_const(8);
                body.binop(op::I64_SHR_U);
                body.i64_const(0xff);
                body.binop(op::I64_AND);
            } else {
                body.load_reg(*reg);
                body.i64_const(size_mask(size));
                body.binop(op::I64_AND);
            }
        }
        RmLoc::Mem(_) => {
            body.emit_load(addr_local.expect("memory operand needs an EA"), size);
            body.i64_const(size_mask(size));
            body.binop(op::I64_AND);
        }
    }
    body.local_set(v);
    v
}

/// Write a *narrow* (here always 1-byte) value into an r/m destination, honouring
/// the legacy AH/CH/DH/BH high-byte encoding and the partial-register merge (a
/// 1-byte write preserves the surrounding bits, unlike a 4-byte write). Mirrors
/// the interpreter's `store_rm(rm, 1, v)`. Used only by SETcc. `val_local` holds
/// the 0/1 byte value.
fn write_rm_byte(
    body: &mut Body,
    rm: &RmLoc,
    addr_local: Option<u32>,
    rex_present: bool,
    val_local: u32,
) {
    match rm {
        RmLoc::Reg(reg) => {
            if !rex_present && (4..8).contains(reg) {
                // AH/CH/DH/BH: r[reg-4] = (r[reg-4] & !0xff00) | ((v & 0xff) << 8)
                let lr = reg - 4;
                body.store_reg(lr, |b| {
                    b.load_reg(lr);
                    b.i64_const(!0xff00i64);
                    b.binop(op::I64_AND);
                    b.local_get(val_local);
                    b.i64_const(0xff);
                    b.binop(op::I64_AND);
                    b.i64_const(8);
                    b.binop(op::I64_SHL);
                    b.binop(op::I64_OR);
                });
            } else {
                // Low byte merge: r[reg] = (r[reg] & !0xff) | (v & 0xff).
                let reg = *reg;
                body.store_reg(reg, |b| {
                    b.load_reg(reg);
                    b.i64_const(!0xffi64);
                    b.binop(op::I64_AND);
                    b.local_get(val_local);
                    b.i64_const(0xff);
                    b.binop(op::I64_AND);
                    b.binop(op::I64_OR);
                });
            }
        }
        RmLoc::Mem(_) => {
            let addr = addr_local.expect("memory operand needs an EA");
            body.emit_store(addr, 1, |b| {
                b.local_get(val_local);
                b.i64_const(0xff);
                b.binop(op::I64_AND);
            });
        }
    }
}

// ── Stack ops (PUSH/POP) ───────────────────────────────────────────────────────

/// Index of RSP in the register file.
const RSP: u32 = 4;

/// `PUSH val` where the 64-bit value is produced by `f`. Mirrors the interpreter's
/// `push`: the pushed value goes to `RSP - 8` and `RSP` is decremented by 8.
///
/// **Fault-restart safety:** the memory store at `RSP - 8` is emitted *before* the
/// `RSP -= 8` commit, so if the store traps (a stack-growth / guard-page #PF — very
/// common in userspace), the register file still holds the *original* RSP. The JIT
/// driver then re-interprets the PUSH from `FAULT_RIP` with an unchanged RSP, and
/// the interpreter decrements it exactly once — no double-decrement. (Committing RSP
/// first, as a naive translation would, double-decrements on every faulting push.)
fn emit_push(body: &mut Body, f: impl FnOnce(&mut Body)) {
    // value to push
    let v = body.local();
    f(body);
    body.local_set(v);
    // store address = RSP - 8 (NOT yet committed to the register file)
    let sp = body.local();
    body.load_reg(RSP);
    body.i64_const(8);
    body.binop(op::I64_SUB);
    body.local_set(sp);
    // store first (may trap → RSP is still the original value in the regfile)
    body.emit_store(sp, 8, |b| b.local_get(v));
    // store succeeded — now commit RSP -= 8
    body.store_reg(RSP, |b| b.local_get(sp));
}

/// `POP` into a fresh local: load 8 bytes from RSP, then `r[RSP] += 8`. Returns
/// the popped value's local. No flags. (Per the interpreter's `pop`, the read uses
/// the pre-increment RSP.)
fn emit_pop(body: &mut Body) -> u32 {
    let sp = body.local();
    body.load_reg(RSP);
    body.local_set(sp);
    let v = body.local();
    body.emit_load(sp, 8);
    body.local_set(v);
    // r[RSP] += 8
    body.store_reg(RSP, |b| {
        b.local_get(sp);
        b.i64_const(8);
        b.binop(op::I64_ADD);
    });
    v
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

// ── TEST (logical AND, flags only) ─────────────────────────────────────────────

/// `TEST` `a & b` — set the logical flags (CF=0,OF=0,ZF/SF/PF from result),
/// writing nothing. `a_local`/`b_local` hold the masked operands.
fn emit_test(body: &mut Body, a_local: u32, b_local: u32, size: u8) {
    let r = body.local();
    body.local_get(a_local);
    body.local_get(b_local);
    body.binop(op::I64_AND);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(r);
    emit_flags_logic(body, r, size);
}

// ── LEA (effective address into a register, no memory access) ───────────────────

/// `LEA reg, m` — write the effective address (already in `addr_local`) into the
/// destination register, masked/zero-extended to the operand size. No flags, no
/// memory access (mirrors the interpreter's `store_rm(Reg, size, addr & mask)`).
fn emit_lea(body: &mut Body, dst: u32, addr_local: u32, size: u8) {
    write_reg(body, dst, size, addr_local);
}

// ── MOVZX / MOVSX / MOVSXD (extend a narrow source into a wide register) ────────

/// `MOVZX`/`MOVSX` `dst, src`. `src_size` is 1 or 2 (the narrow source); for
/// MOVSXD it is 4. `signed` selects sign- vs zero-extension. The narrow source has
/// already been read (zero-extended) into `src_local`; this extends it to the
/// operand `size` and writes the destination register. No flags.
fn emit_movx(body: &mut Body, dst: u32, src_local: u32, src_size: u8, signed: bool, size: u8) {
    let v = body.local();
    if signed {
        body.local_get(src_local);
        body.binop(match src_size {
            1 => op::I64_EXTEND8_S,
            2 => op::I64_EXTEND16_S,
            _ => op::I64_EXTEND32_S,
        });
    } else {
        // zero-extend: the source local is already `& mask(src_size)`.
        body.local_get(src_local);
    }
    body.local_set(v);
    // write_reg masks to `size` (a 32-bit destination clears the upper 32 bits).
    write_reg(body, dst, size, v);
}

// ── Shifts SHL/SHR/SAR ─────────────────────────────────────────────────────────

/// The supported shift kinds (group-2 digit): SHL(4/6), SHR(5), SAR(7).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShiftOp {
    Shl,
    Shr,
    Sar,
}

impl ShiftOp {
    fn from_digit(d: u8) -> Option<ShiftOp> {
        Some(match d {
            4 | 6 => ShiftOp::Shl, // SHL / SAL
            5 => ShiftOp::Shr,     // SHR
            7 => ShiftOp::Sar,     // SAR
            _ => return None,      // ROL/ROR/RCL/RCR (0..=3) deferred — stop
        })
    }
}

/// `SHL/SHR/SAR rm, cnt`. `cnt_local` holds the *raw* count (imm8 zero-extended,
/// or `CL` for the `0xD3` form); it is masked to 0x3f (size 8) / 0x1f (size 4)
/// here, matching the interpreter. A zero (masked) count leaves the result AND all
/// flags unchanged. CF = last bit shifted out; ZF/SF/PF from the result; OF is
/// left unchanged (the interpreter's `shift_rotate` never writes OF).
fn emit_shift(
    body: &mut Body,
    sop: ShiftOp,
    rm: &RmLoc,
    rm_addr: Option<u32>,
    cnt_local: u32,
    size: u8,
) {
    let bits = i64::from(size) * 8;
    let cnt_mask: i64 = if size == 8 { 63 } else { 31 };

    // cnt = raw & cnt_mask
    let cnt = body.local();
    body.local_get(cnt_local);
    body.i64_const(cnt_mask);
    body.binop(op::I64_AND);
    body.local_set(cnt);

    // The source value (masked to size).
    let a = read_rm(body, rm, size, rm_addr);

    // result and CF, computed unconditionally for cnt != 0; the cnt==0 case selects
    // back the original operand / original flags so nothing changes.
    let res = body.local();
    let cf = body.local();
    match sop {
        ShiftOp::Shl => {
            // res = (a << cnt) & mask
            body.local_get(a);
            body.local_get(cnt);
            body.binop(op::I64_SHL);
            if size < 8 {
                body.i64_const(size_mask(size));
                body.binop(op::I64_AND);
            }
            body.local_set(res);
            // cf = (a >> (bits - cnt)) & 1
            body.local_get(a);
            body.i64_const(bits);
            body.local_get(cnt);
            body.binop(op::I64_SUB);
            body.binop(op::I64_SHR_U);
            body.i64_const(1);
            body.binop(op::I64_AND);
            body.local_set(cf);
        }
        ShiftOp::Shr => {
            // res = a >> cnt   (a already masked → logical)
            body.local_get(a);
            body.local_get(cnt);
            body.binop(op::I64_SHR_U);
            body.local_set(res);
            // cf = (a >> (cnt - 1)) & 1
            body.local_get(a);
            body.local_get(cnt);
            body.i64_const(1);
            body.binop(op::I64_SUB);
            body.binop(op::I64_SHR_U);
            body.i64_const(1);
            body.binop(op::I64_AND);
            body.local_set(cf);
        }
        ShiftOp::Sar => {
            // sign-extend a to 64 bits, arithmetic-shift, re-mask.
            // sa = (a << (64-bits)) >>s (64-bits)
            body.local_get(a);
            if bits < 64 {
                body.i64_const(64 - bits);
                body.binop(op::I64_SHL);
                body.i64_const(64 - bits);
                body.binop(op::I64_SHR_S);
            }
            body.local_get(cnt);
            body.binop(op::I64_SHR_S);
            if size < 8 {
                body.i64_const(size_mask(size));
                body.binop(op::I64_AND);
            }
            body.local_set(res);
            // cf = (a >> (cnt - 1)) & 1
            body.local_get(a);
            body.local_get(cnt);
            body.i64_const(1);
            body.binop(op::I64_SUB);
            body.binop(op::I64_SHR_U);
            body.i64_const(1);
            body.binop(op::I64_AND);
            body.local_set(cf);
        }
    }

    // Build the new flags (CF + ZF/SF/PF from res), then SELECT between old and new
    // flags on (cnt != 0) so a zero count leaves rflags unchanged.
    let oldflags = body.local();
    body.load_rflags();
    body.local_set(oldflags);
    let newflags = body.local();
    // start from oldflags with CF/ZF/SF/PF cleared (OF preserved — interp leaves it).
    body.local_get(oldflags);
    body.i64_const(!((CF | ZF | SF | PF) as i64));
    body.binop(op::I64_AND);
    // | CF (CF is bit 0 — no shift needed)
    body.local_get(cf);
    body.binop(op::I64_OR);
    // | ZF
    push_zf(body, res);
    shift_into(body, ZF);
    body.binop(op::I64_OR);
    // | SF
    push_sf(body, res, size);
    shift_into(body, SF);
    body.binop(op::I64_OR);
    // | PF
    push_pf(body, res);
    shift_into(body, PF);
    body.binop(op::I64_OR);
    body.local_set(newflags);

    // rflags = (cnt != 0) ? newflags : oldflags
    body.store_rflags(|b| {
        b.local_get(newflags);
        b.local_get(oldflags);
        // predicate: cnt != 0  (i32)
        b.local_get(cnt);
        b.binop(op::I64_EQZ); // i32 1 if cnt==0
        b.binop(op::I32_EQZ); // i32 1 if cnt!=0
        b.byte(op::SELECT);
    });

    // Write the destination = (cnt != 0) ? res : original. A zero count leaves the
    // operand entirely unchanged in the interpreter (no store), so for a *register*
    // destination the not-shifted value must be the FULL 64-bit register (a 32-bit
    // store would wrongly clear the upper half); we therefore build the final value
    // already zero-extended/merged and do a single full 64-bit register write.
    match rm {
        RmLoc::Reg(reg) => {
            let out = body.local();
            // shifted = res zero-extended to the operand size (size 4 clears upper).
            body.local_get(res);
            if size < 8 {
                body.i64_const(size_mask(size));
                body.binop(op::I64_AND);
            }
            // original = the FULL register value (unchanged when cnt == 0).
            body.load_reg(*reg);
            body.local_get(cnt);
            body.binop(op::I64_EQZ);
            body.binop(op::I32_EQZ); // cnt != 0
            body.byte(op::SELECT);
            body.local_set(out);
            // full 64-bit store (the value already carries the right extension).
            body.store_reg(*reg, |b| b.local_get(out));
        }
        RmLoc::Mem(_) => {
            // For memory, re-storing the original `size` bytes when cnt == 0 is a
            // no-op (identical bytes), so a plain size-`size` store of the selected
            // value is correct.
            let out = body.local();
            body.local_get(res);
            body.local_get(a);
            body.local_get(cnt);
            body.binop(op::I64_EQZ);
            body.binop(op::I32_EQZ); // cnt != 0
            body.byte(op::SELECT);
            body.local_set(out);
            write_rm(body, rm, size, rm_addr, out);
        }
    }
}

// ── SETcc (write 0/1 byte by condition) ────────────────────────────────────────

/// `SETcc rm` — write the byte `cond(cc)` (0/1) to the r/m destination (no flags).
fn emit_setcc(body: &mut Body, cc: u8, rm: &RmLoc, rm_addr: Option<u32>, rex_present: bool) {
    let v = body.local();
    emit_cond_base(body, cc >> 1); // i64 0/1
    if cc & 1 == 1 {
        // invert: 1 - base  ==  (base == 0)
        body.binop(op::I64_EQZ);
        body.binop(op::I64_EXTEND_I32_U);
    }
    body.local_set(v);
    write_rm_byte(body, rm, rm_addr, rex_present, v);
}

// ── NEG / NOT (group3 /3, /2) ──────────────────────────────────────────────────

/// `NOT rm` — bitwise complement, no flags.
fn emit_not(body: &mut Body, rm: &RmLoc, rm_addr: Option<u32>, size: u8) {
    let a = read_rm(body, rm, size, rm_addr);
    let r = body.local();
    body.local_get(a);
    body.i64_const(-1);
    body.binop(op::I64_XOR);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(r);
    write_rm(body, rm, size, rm_addr, r);
}

/// `NEG rm` — `0 - a`; flags as `flags_arith(0, a, r, size, sub=true)`, then
/// CF = (a != 0) (the interpreter overrides the borrow CF with `a != 0`).
fn emit_neg(body: &mut Body, rm: &RmLoc, rm_addr: Option<u32>, size: u8) {
    let a = read_rm(body, rm, size, rm_addr);
    let zero = body.local();
    body.i64_const(0);
    body.local_set(zero);
    let r = body.local();
    body.i64_const(0);
    body.local_get(a);
    body.binop(op::I64_SUB);
    if size < 8 {
        body.i64_const(size_mask(size));
        body.binop(op::I64_AND);
    }
    body.local_set(r);
    // flags_arith(0, a, r, sub) sets CF = (0 < a) which equals (a != 0); the
    // interpreter then explicitly sets CF = (a != 0) — identical — so the standard
    // arithmetic flags already match.
    emit_flags_arith(body, zero, a, r, size, true);
    write_rm(body, rm, size, rm_addr, r);
}

// ── IMUL (2/3-operand) — truncated product into a register, no flags ───────────

/// 2-operand `IMUL reg, rm` (0x0F 0xAF) — `reg = (reg * rm)` truncated to the
/// operand size, written to the register. The interpreter sets NO flags for this
/// form, so neither do we.
fn emit_imul2(body: &mut Body, dst: u32, rm: &RmLoc, rm_addr: Option<u32>, size: u8) {
    let a = read_rm(body, &RmLoc::Reg(dst), size, None);
    let b = read_rm(body, rm, size, rm_addr);
    let r = body.local();
    body.local_get(a);
    body.local_get(b);
    body.binop(op::I64_MUL);
    body.local_set(r);
    write_reg(body, dst, size, r);
}

/// 3-operand `IMUL reg, rm, imm` (0x69 imm32 / 0x6B imm8) — `reg = sext(rm) * imm`
/// truncated to the operand size. The immediate is already sign-extended to i64 by
/// the decoder. The interpreter sets NO flags for this form. The `rm` source is
/// sign-extended to the operand width before the multiply.
fn emit_imul3(body: &mut Body, dst: u32, rm: &RmLoc, rm_addr: Option<u32>, imm: i64, size: u8) {
    let a_raw = read_rm(body, rm, size, rm_addr);
    let r = body.local();
    // sign-extend the (masked) rm value to the operand width, then * imm.
    body.local_get(a_raw);
    if size == 4 {
        body.binop(op::I64_EXTEND32_S);
    }
    body.i64_const(imm);
    body.binop(op::I64_MUL);
    body.local_set(r);
    write_reg(body, dst, size, r);
}

// ── MUL / IMUL single-operand (group3 /4,/5) — write RDX:RAX, set CF/OF ─────────

/// Index of RAX / RDX in the register file.
const RAX: u32 = 0;
const RDX: u32 = 2;

/// Single-operand `MUL`/`IMUL` (0xF7 /4,/5): `RDX:RAX = RAX * rm`. `signed`
/// selects IMUL. Sets CF=OF=(high half nonzero), mirroring `store_mul_result`;
/// the other arithmetic flags (ZF/SF/PF) are left as the interpreter leaves them
/// (it does not touch them here). Only operand sizes 4/8 reach this path.
fn emit_muldiv_mul(body: &mut Body, rm: &RmLoc, rm_addr: Option<u32>, signed: bool, size: u8) {
    // a = RAX & mask ; b = rm & mask
    let a = read_rm(body, &RmLoc::Reg(RAX), size, None);
    let b = read_rm(body, rm, size, rm_addr);

    if size == 8 {
        // 64x64 → 128. Wasm has no i128, so compute the 128-bit product by halves.
        emit_mul64_128(body, a, b, signed);
    } else {
        // 32x32 → 64 fits in one i64. Sign-extend operands for IMUL.
        let prod = body.local();
        body.local_get(a);
        if signed {
            body.binop(op::I64_EXTEND32_S);
        }
        body.local_get(b);
        if signed {
            body.binop(op::I64_EXTEND32_S);
        }
        body.binop(op::I64_MUL);
        body.local_set(prod);
        // RAX = prod & 0xffffffff (zero-extends upper, per store_rm size 4)
        let lo = body.local();
        body.local_get(prod);
        body.i64_const(size_mask(4));
        body.binop(op::I64_AND);
        body.local_set(lo);
        write_reg(body, RAX, 4, lo);
        // hi = (prod >> 32) & 0xffffffff
        let hi = body.local();
        body.local_get(prod);
        body.i64_const(32);
        body.binop(op::I64_SHR_U);
        body.i64_const(size_mask(4));
        body.binop(op::I64_AND);
        body.local_set(hi);
        write_reg(body, RDX, 4, hi);
        // CF=OF = hi != 0   (hi is the size-masked high half)
        emit_mul_overflow_flags(body, hi);
    }
}

/// Set CF=OF = (`hi_local` != 0); ZF/SF/PF preserved (matching `store_mul_result`).
fn emit_mul_overflow_flags(body: &mut Body, hi_local: u32) {
    body.store_rflags(|b| {
        b.load_rflags();
        b.i64_const(!((CF | OF) as i64));
        b.binop(op::I64_AND);
        // ov = hi != 0  (i64 0/1)
        let ov = b.local();
        b.local_get(hi_local);
        b.binop(op::I64_EQZ);
        b.binop(op::I32_EQZ);
        b.binop(op::I64_EXTEND_I32_U);
        b.local_set(ov);
        b.local_get(ov);
        // CF bit 0
        b.binop(op::I64_OR);
        b.local_get(ov);
        shift_into(b, OF);
        b.binop(op::I64_OR);
    });
}

/// Emit a full 64×64→128 multiply of `a_local`*`b_local`, storing the low 64 bits
/// into RAX and the high 64 bits into RDX, then setting CF=OF=(high != 0).
/// `signed` selects IMUL (signed) vs MUL (unsigned). The 128-bit product is built
/// from 32-bit lane partial products (Wasm lacks `i64.mul_wide`).
fn emit_mul64_128(body: &mut Body, a_local: u32, b_local: u32, signed: bool) {
    // Compute the UNSIGNED 128-bit product first via 32-bit lanes, then correct
    // the high half for signedness (two's-complement: subtract b if a<0, a if b<0).
    let mask32: i64 = 0xffff_ffff;

    let a0 = body.local(); // a low 32
    let a1 = body.local(); // a high 32
    let b0 = body.local();
    let b1 = body.local();
    body.local_get(a_local);
    body.i64_const(mask32);
    body.binop(op::I64_AND);
    body.local_set(a0);
    body.local_get(a_local);
    body.i64_const(32);
    body.binop(op::I64_SHR_U);
    body.local_set(a1);
    body.local_get(b_local);
    body.i64_const(mask32);
    body.binop(op::I64_AND);
    body.local_set(b0);
    body.local_get(b_local);
    body.i64_const(32);
    body.binop(op::I64_SHR_U);
    body.local_set(b1);

    // ll = a0*b0 ; lh = a1*b0 ; hl = a0*b1 ; hh = a1*b1
    let ll = body.local();
    body.local_get(a0);
    body.local_get(b0);
    body.binop(op::I64_MUL);
    body.local_set(ll);
    let lh = body.local();
    body.local_get(a1);
    body.local_get(b0);
    body.binop(op::I64_MUL);
    body.local_set(lh);
    let hl = body.local();
    body.local_get(a0);
    body.local_get(b1);
    body.binop(op::I64_MUL);
    body.local_set(hl);
    let hh = body.local();
    body.local_get(a1);
    body.local_get(b1);
    body.binop(op::I64_MUL);
    body.local_set(hh);

    // mid = (ll >> 32) + (lh & mask32) + (hl & mask32)
    let mid = body.local();
    body.local_get(ll);
    body.i64_const(32);
    body.binop(op::I64_SHR_U);
    body.local_get(lh);
    body.i64_const(mask32);
    body.binop(op::I64_AND);
    body.binop(op::I64_ADD);
    body.local_get(hl);
    body.i64_const(mask32);
    body.binop(op::I64_AND);
    body.binop(op::I64_ADD);
    body.local_set(mid);

    // lo = (ll & mask32) | (mid << 32)
    let lo = body.local();
    body.local_get(ll);
    body.i64_const(mask32);
    body.binop(op::I64_AND);
    body.local_get(mid);
    body.i64_const(32);
    body.binop(op::I64_SHL);
    body.binop(op::I64_OR);
    body.local_set(lo);

    // hi = hh + (lh >> 32) + (hl >> 32) + (mid >> 32)
    let hi = body.local();
    body.local_get(hh);
    body.local_get(lh);
    body.i64_const(32);
    body.binop(op::I64_SHR_U);
    body.binop(op::I64_ADD);
    body.local_get(hl);
    body.i64_const(32);
    body.binop(op::I64_SHR_U);
    body.binop(op::I64_ADD);
    body.local_get(mid);
    body.i64_const(32);
    body.binop(op::I64_SHR_U);
    body.binop(op::I64_ADD);
    body.local_set(hi);

    if signed {
        // Signed correction: hi -= (a<0 ? b : 0) + (b<0 ? a : 0).
        // a<0 ? b : 0
        body.local_get(hi);
        body.local_get(b_local);
        body.i64_const(0);
        // predicate a < 0  → (a >> 63) & 1 as i32
        body.local_get(a_local);
        body.i64_const(63);
        body.binop(op::I64_SHR_U);
        body.binop(op::I32_WRAP_I64);
        body.byte(op::SELECT); // a<0 ? b : 0
        body.binop(op::I64_SUB);
        // - (b<0 ? a : 0)
        body.local_get(a_local);
        body.i64_const(0);
        body.local_get(b_local);
        body.i64_const(63);
        body.binop(op::I64_SHR_U);
        body.binop(op::I32_WRAP_I64);
        body.byte(op::SELECT); // b<0 ? a : 0
        body.binop(op::I64_SUB);
        body.local_set(hi);
    }

    // RAX = lo ; RDX = hi (full 64-bit writes).
    write_reg(body, RAX, 8, lo);
    write_reg(body, RDX, 8, hi);

    // CF=OF = (hi != 0). This matches the interpreter's `store_mul_result`, which
    // sets the overflow from `(prod >> bits) != 0` over the *unsigned* 128-bit
    // two's-complement product (so a negative IMUL result, whose high half is all
    // ones, also sets the flags) — `hi` here is exactly that top 64-bit half.
    emit_mul_overflow_flags(body, hi);
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

// ── Region (trace) translation ─────────────────────────────────────────────────

/// The maximum number of basic blocks a region may contain. A larger cap captures
/// bigger traces (more throughput): a hot trace of straight-line blocks joined by
/// direct branches then retires in a single Wasm `run` call instead of one call per
/// few blocks, so the per-call overhead (regfile sync + the memory-touching swap +
/// the wasmtime dispatch) is amortised over far more guest instructions. Interrupt
/// cadence is unaffected — a region's internal dispatch loop honours the per-call
/// instruction `budget` (capped by the driver at the next-timer deadline), so it
/// exits to deliver an IRQ regardless of how many blocks it *could* contain. Beyond
/// this, discovery treats further direct-branch targets as region exits.
const MAX_REGION_BLOCKS: usize = 32;
/// The maximum guest-byte span of a region from its entry (a region never crosses a
/// guest page — the driver fetches at most a page — and is further capped here).
const MAX_REGION_BYTES: usize = 4096;

/// The control-flow classification of one scanned instruction (discovery pass).
enum Scan {
    /// An ordinary supported, non-control instruction of `len` bytes.
    Linear { len: usize },
    /// An unconditional relative `JMP` of `len` bytes whose target is at byte
    /// offset `target` from the region entry.
    Jmp { len: usize, target: usize },
    /// A conditional `Jcc` (`cc` = low nibble) of `len` bytes; `target` is the taken
    /// offset from the region entry, the fall-through is the next instruction.
    Jcc { len: usize, cc: u8, target: usize },
    /// An unsupported instruction / a non-direct control transfer — a region edge.
    Stop,
}

/// Scan one instruction at byte offset `at` within the region `code` (entry-based
/// offsets), classifying its control flow and length **without emitting** anything.
/// This mirrors [`decode_one`]'s opcode coverage exactly so the discovered block
/// boundaries match what the emitter (`decode_one`) will produce; a branch's
/// `target` is resolved to a region-entry-relative byte offset (`insn_end + rel`).
/// Returns `None` only on truncation (the instruction runs off the slice).
fn scan_one(code: &[u8], at: usize) -> Option<Scan> {
    let mut dec = Decoder::new(&code[at..]);
    // Optional REX prefix; 0x66 (16-bit) is unsupported.
    let mut rex = 0u8;
    let b0 = dec.peek()?;
    if (0x40..=0x4f).contains(&b0) {
        rex = b0;
        dec.u8();
    } else if b0 == 0x66 {
        return Some(Scan::Stop);
    }
    let op = dec.u8()?;

    // A helper that, after the decoder has consumed the whole instruction, yields a
    // `Linear` scan of the right length (region-entry-relative end minus start).
    macro_rules! linear {
        () => {
            Some(Scan::Linear { len: dec.pos })
        };
    }

    match op {
        // ALU reg forms (r/m,r and r,r/m) — adc/sbb (digit 2/3) unsupported.
        0x01 | 0x09 | 0x21 | 0x29 | 0x31 | 0x39 | 0x03 | 0x0b | 0x23 | 0x2b | 0x33 | 0x3b => {
            if AluOp::from_digit(op >> 3).is_none() {
                return Some(Scan::Stop);
            }
            decode_modrm(&mut dec, rex)?;
            linear!()
        }
        // group1 imm32 / imm8.
        0x81 | 0x83 => {
            let d = decode_modrm(&mut dec, rex)?;
            if AluOp::from_digit((d.reg & 7) as u8).is_none() {
                return Some(Scan::Stop);
            }
            if op == 0x83 {
                dec.u8()?;
            } else {
                dec.u32_le()?;
            }
            linear!()
        }
        // mov r/m,r ; mov r,r/m ; LEA ; TEST r/m,r.
        0x89 | 0x8b | 0x8d | 0x85 => {
            let d = decode_modrm(&mut dec, rex)?;
            if op == 0x8d && matches!(d.rm, RmLoc::Reg(_)) {
                return Some(Scan::Stop); // LEA reg,reg is #UD
            }
            linear!()
        }
        // mov r, imm (imm64 if REX.W else imm32).
        0xb8..=0xbf => {
            if rex & 8 != 0 {
                dec.u64_le()?;
            } else {
                dec.u32_le()?;
            }
            linear!()
        }
        // mov r/m, imm32 (0xC7 /0 only).
        0xc7 => {
            let d = decode_modrm(&mut dec, rex)?;
            if d.reg & 7 != 0 {
                return Some(Scan::Stop);
            }
            dec.u32_le()?;
            linear!()
        }
        // 0xFF /0,/1 (inc/dec), /6 (push r/m); else control flow → Stop.
        0xff => {
            let d = decode_modrm(&mut dec, rex)?;
            match d.reg & 7 {
                0 | 1 | 6 => linear!(),
                _ => Some(Scan::Stop),
            }
        }
        // JMP rel8 / rel32 (terminators).
        0xeb => {
            let rel = dec.u8()? as i8 as i64;
            let target = (at + dec.pos).wrapping_add(rel as usize);
            Some(Scan::Jmp {
                len: dec.pos,
                target,
            })
        }
        0xe9 => {
            let rel = dec.u32_le()? as i32 as i64;
            let target = (at + dec.pos).wrapping_add(rel as usize);
            Some(Scan::Jmp {
                len: dec.pos,
                target,
            })
        }
        // Jcc rel8.
        0x70..=0x7f => {
            let rel = dec.u8()? as i8 as i64;
            let target = (at + dec.pos).wrapping_add(rel as usize);
            Some(Scan::Jcc {
                len: dec.pos,
                cc: op - 0x70,
                target,
            })
        }
        // PUSH/POP r64.
        0x50..=0x5f => linear!(),
        // POP r/m (0x8F /0). Register and non-RSP-relative memory destinations are
        // translated; an RSP-relative `pop [rsp±…]` is left to the interpreter for
        // fault-restart correctness (see `decode_one`).
        0x8f => {
            let d = decode_modrm(&mut dec, rex)?;
            if d.reg & 7 != 0 {
                return Some(Scan::Stop);
            }
            if let RmLoc::Mem(ref ea) = d.rm {
                if ea.base == Some(RSP) || ea.index == Some(RSP) {
                    return Some(Scan::Stop);
                }
            }
            linear!()
        }
        // TEST eAX,imm.
        0xa9 => {
            dec.u32_le()?;
            linear!()
        }
        // MOVSXD.
        0x63 => {
            decode_modrm(&mut dec, rex)?;
            linear!()
        }
        // IMUL r,r/m,imm32 / imm8.
        0x69 | 0x6b => {
            decode_modrm(&mut dec, rex)?;
            if op == 0x6b {
                dec.u8()?;
            } else {
                dec.u32_le()?;
            }
            linear!()
        }
        // shifts.
        0xc1 | 0xd1 | 0xd3 => {
            let d = decode_modrm(&mut dec, rex)?;
            if ShiftOp::from_digit((d.reg & 7) as u8).is_none() {
                return Some(Scan::Stop);
            }
            if op == 0xc1 {
                dec.u8()?;
            }
            linear!()
        }
        // group3 (0xF7): TEST/NOT/NEG/MUL/IMUL; DIV/IDIV → Stop.
        0xf7 => {
            let d = decode_modrm(&mut dec, rex)?;
            match d.reg & 7 {
                0 | 1 => {
                    dec.u32_le()?;
                    linear!()
                }
                2..=5 => linear!(),
                _ => Some(Scan::Stop),
            }
        }
        // 0x0F two-byte.
        0x0f => {
            let op2 = dec.u8()?;
            match op2 {
                // Jcc rel32 (terminator).
                0x80..=0x8f => {
                    let rel = dec.u32_le()? as i32 as i64;
                    let target = (at + dec.pos).wrapping_add(rel as usize);
                    Some(Scan::Jcc {
                        len: dec.pos,
                        cc: op2 - 0x80,
                        target,
                    })
                }
                // SETcc / MOVZX / MOVSX / 2-op IMUL.
                0x90..=0x9f | 0xb6 | 0xb7 | 0xbe | 0xbf | 0xaf => {
                    decode_modrm(&mut dec, rex)?;
                    linear!()
                }
                _ => Some(Scan::Stop),
            }
        }
        _ => Some(Scan::Stop),
    }
}

/// A discovered basic block within a region: its byte offset from the region entry,
/// the instructions it spans, and how it leaves (its successors).
struct RegionBlock {
    /// Byte offset of the block's first instruction from the region entry.
    start: usize,
    /// The number of guest instructions in the block (including any terminator).
    insns: u32,
    /// How the block exits.
    exit: BlockExit,
}

/// How a discovered region block transfers control.
enum BlockExit {
    /// Falls through (no branch) to byte offset `next` (a discovered in-region
    /// block — fall-through is only used when the next block was discovered).
    Fallthrough { next: usize },
    /// An unconditional `JMP` to byte offset `target`.
    Jmp { target: usize },
    /// A `Jcc cc`: taken → `target`, not-taken → `fallthru` (both byte offsets).
    Jcc {
        cc: u8,
        target: usize,
        fallthru: usize,
    },
    /// Leaves the region at guest `rip == entry_rip + exit_off` (an out-of-region
    /// direct branch target, an unsupported/indirect transfer, or the end of the
    /// scanned slice). The driver resumes there (the JIT or the interpreter).
    Leave { exit_off: usize },
}

/// Translate a **region** (trace) starting at the hot entry `code[0]` (guest
/// `rip == entry_rip`). Discovers the basic blocks reachable from the entry by
/// *direct* branches that stay within the scanned slice (a single guest page, capped
/// by [`MAX_REGION_BLOCKS`] / [`MAX_REGION_BYTES`]), assigns each an index, and emits
/// ONE Wasm function with an internal `br_table` dispatch loop so a hot guest loop
/// runs entirely inside one Wasm call. Returns `None` if the entry instruction is
/// itself untranslatable (the caller interprets it).
///
/// The emitted `run(entry_rip, budget) -> (exit_rip, insns)` executes guest
/// instructions from block 0 until it leaves the region or the instruction `budget`
/// is exhausted, then returns the guest `rip` to continue at and the retired count.
#[must_use]
pub fn translate_region_at(code: &[u8], entry_rip: u64) -> Option<TranslatedRegion> {
    let _ = entry_rip; // supplied at call time via the `$entry_rip` parameter.
    let slice_len = code.len().min(MAX_REGION_BYTES);

    // ── Discovery: BFS over block starts reachable by direct branches ──
    // `index_of[off]` maps a discovered block-start offset → its region block index.
    let mut starts: Vec<usize> = Vec::new();
    let mut index_of: alloc::collections::BTreeMap<usize, usize> =
        alloc::collections::BTreeMap::new();
    let mut blocks: Vec<RegionBlock> = Vec::new();
    let mut worklist: Vec<usize> = Vec::new();

    // Register a block-start offset (if in range and not seen), returning whether it
    // is (now) an in-region block. Out-of-range starts are region exits.
    let want = |off: usize,
                starts: &mut Vec<usize>,
                index_of: &mut alloc::collections::BTreeMap<usize, usize>,
                worklist: &mut Vec<usize>|
     -> bool {
        if off >= slice_len {
            return false;
        }
        if index_of.contains_key(&off) {
            return true;
        }
        if starts.len() >= MAX_REGION_BLOCKS {
            return false; // cap reached — treat as a region exit
        }
        let idx = starts.len();
        starts.push(off);
        index_of.insert(off, idx);
        worklist.push(off);
        true
    };

    want(0, &mut starts, &mut index_of, &mut worklist);

    // Process the worklist. Each entry is a block start; scan instructions until a
    // terminator or a region edge, recording the block and enqueuing direct targets.
    while let Some(block_start) = worklist.pop() {
        let mut pos = block_start;
        let mut insns: u32 = 0;
        let exit;
        loop {
            // A discovered block-start that is not this block ends the current block
            // (fall into it) — keeps blocks single-entry for the dispatch.
            if pos != block_start && index_of.contains_key(&pos) {
                exit = BlockExit::Fallthrough { next: pos };
                break;
            }
            if pos >= slice_len {
                exit = BlockExit::Leave { exit_off: pos };
                break;
            }
            match scan_one(code, pos) {
                None | Some(Scan::Stop) => {
                    // The instruction at `pos` is untranslatable / a region edge: the
                    // block ends *before* it and leaves the region at `pos`.
                    if pos == block_start {
                        // The block's first instruction is itself untranslatable.
                        if block_start == 0 {
                            return None; // entry instruction unsupported
                        }
                        // A branch target landed on an unsupported instruction — the
                        // block is just a region exit to `pos` (no instructions).
                    }
                    exit = BlockExit::Leave { exit_off: pos };
                    break;
                }
                Some(Scan::Linear { len }) => {
                    insns += 1;
                    pos += len;
                }
                Some(Scan::Jmp { len: _, target }) => {
                    insns += 1;
                    let in_region = want(target, &mut starts, &mut index_of, &mut worklist);
                    exit = if in_region {
                        BlockExit::Jmp { target }
                    } else {
                        BlockExit::Leave { exit_off: target }
                    };
                    break;
                }
                Some(Scan::Jcc { len, cc, target }) => {
                    insns += 1;
                    pos += len;
                    let fallthru = pos;
                    // Enqueue both successors (in-region ones become blocks).
                    want(target, &mut starts, &mut index_of, &mut worklist);
                    want(fallthru, &mut starts, &mut index_of, &mut worklist);
                    exit = BlockExit::Jcc {
                        cc,
                        target,
                        fallthru,
                    };
                    break;
                }
            }
        }
        blocks.push(RegionBlock {
            start: block_start,
            insns,
            exit,
        });
    }

    // Sort blocks by their assigned index (worklist order is LIFO).
    blocks.sort_by_key(|b| index_of[&b.start]);

    // A region with a single block and no in-region branch is just a basic block —
    // still emitted as a (degenerate) region for a uniform driver path.
    let region_bytes = blocks
        .iter()
        .map(|b| b.start + block_byte_len(code, b))
        .max()
        .unwrap_or(0)
        .min(slice_len);

    // ── Emission: one function with a br_table dispatch loop ──
    let mut body = Body::new_region();
    // Reserve the bookkeeping locals first so their indices are stable:
    //   $cur   — current block index (i64; wrapped to i32 for br_table)
    //   $insns — guest instructions retired so far (the `run` 2nd result)
    // The per-instruction emitters allocate further i64 temporaries after these.
    let cur_local = body.local();
    let insns_local = body.local();
    // A scratch local for `decode_one`'s `next_rip` parameter — only branch
    // terminators write it, and the region emits those itself (never via
    // `decode_one`), so it is never actually stored to; reserved for a valid index.
    let scratch_next_rip = body.local();
    body.retired_local = Some(insns_local);

    // Wasm locals are zero-initialised, so `$cur = 0` (enter at block 0) and
    // `$insns = 0` need no init code.

    let n = blocks.len();

    // Dispatch shape (the standard relooper br_table form), with `return` used for
    // every region exit so no multi-value block type is needed:
    //
    //   loop $disp                          ;; void
    //     block $B_{n-1} … block $B_0       ;; void each (innermost = $B_0)
    //       (i32)$cur ; br_table 0 1 … n-1 default
    //     end ; <body B_0>                  ;; br $disp (depth n-1) to re-dispatch,
    //     end ; <body B_1>                  ;;   or push (rip,insns); return to exit
    //     …
    //     end ; <body B_{n-1}>
    //   end loop
    //   unreachable                         ;; control never falls out of the loop
    //
    // From inside body B_k the open frames outward are $B_{k+1}…$B_{n-1} then the
    // loop, so `br $disp` is at depth `n-1-k`.
    body.byte(op::LOOP);
    body.byte(0x40); // void
    for _ in 0..n {
        body.byte(op::BLOCK);
        body.byte(0x40);
    }
    body.local_get(cur_local);
    body.binop(op::I32_WRAP_I64);
    body.byte(op::BR_TABLE);
    leb_u32(&mut body.code, n as u32); // table length (targets 0..n-1)
    for k in 0..n {
        leb_u32(&mut body.code, k as u32); // index k → block $B_k (depth k here)
    }
    leb_u32(&mut body.code, 0); // default (unreachable: $cur is always valid)

    // Emit each block: `end` (closes $B_k), then its body.
    let mut dec = Decoder::new(code);
    for (k, block) in blocks.iter().enumerate() {
        body.byte(op::END); // close block $B_k → its body follows
        emit_region_block(
            &mut body,
            &mut dec,
            block,
            &index_of,
            cur_local,
            insns_local,
            scratch_next_rip,
            k,
            n,
        );
    }
    body.byte(op::END); // close loop $disp
    body.byte(0x00); // `unreachable` — control never falls out of the dispatch loop

    let touches_mem = body.touches_mem;
    let total_insns: u32 = blocks.iter().map(|b| b.insns).sum();
    let wasm = encode_module(&body);
    Some(TranslatedRegion {
        wasm,
        bytes: region_bytes as u32,
        blocks: n as u32,
        insns: total_insns,
        touches_mem,
    })
}

/// The byte length the block at `b.start` spans (to its terminator end or its leave
/// edge), used to size the region's SMC extent. Re-scans the block's instructions.
fn block_byte_len(code: &[u8], b: &RegionBlock) -> usize {
    let mut pos = b.start;
    let slice_len = code.len().min(MAX_REGION_BYTES);
    let mut count = 0u32;
    while count < b.insns && pos < slice_len {
        match scan_one(code, pos) {
            Some(Scan::Linear { len })
            | Some(Scan::Jmp { len, .. })
            | Some(Scan::Jcc { len, .. }) => {
                pos += len;
                count += 1;
            }
            _ => break,
        }
    }
    pos - b.start
}

/// Emit the body of one region block `B_k` (of `n`): its instructions (via the
/// per-instruction emitters over the shared `dec`), counting each into `$insns`,
/// then the dispatch tail that either continues the loop at the next in-region block
/// index (honouring the instruction budget) or `return`s an exit rip to the driver.
/// `dec` is sought to the block's start before emitting.
#[allow(clippy::too_many_arguments)]
fn emit_region_block(
    body: &mut Body,
    dec: &mut Decoder,
    block: &RegionBlock,
    index_of: &alloc::collections::BTreeMap<usize, usize>,
    cur_local: u32,
    insns_local: u32,
    scratch_next_rip: u32,
    k: usize,
    n: usize,
) {
    // The `br $disp` (loop) depth from inside this block's body: the open frames
    // outward are $B_{k+1}…$B_{n-1} then the loop.
    let disp_depth = (n - 1 - k) as u32;

    // The terminator (JMP/Jcc) is NOT emitted by `decode_one` (the region emits its
    // own dispatch for it); every other instruction is.
    let has_branch_terminator = matches!(block.exit, BlockExit::Jmp { .. } | BlockExit::Jcc { .. });
    let body_insns = if has_branch_terminator {
        block.insns - 1
    } else {
        block.insns
    };

    dec.pos = block.start;
    let mut emitted: u32 = 0;
    while emitted < body_insns {
        body.cur_insn_off = dec.pos;
        match decode_one(dec, body, scratch_next_rip) {
            DecodeResult::Ok => {
                // $insns += 1 (one more instruction retired in the region).
                bump_insns(body, insns_local);
                emitted += 1;
            }
            // A non-terminator block should never produce these (the scanner
            // validated supportability and the terminator is handled separately);
            // be defensive and stop emitting the body if it does.
            DecodeResult::Terminator | DecodeResult::Stop => break,
        }
    }

    // Dispatch tail.
    match &block.exit {
        BlockExit::Fallthrough { next } => {
            let idx = index_of[next];
            emit_region_goto(body, idx, *next, cur_local, insns_local, disp_depth);
        }
        BlockExit::Leave { exit_off } => emit_region_return(body, *exit_off, insns_local),
        BlockExit::Jmp { target } => {
            bump_insns(body, insns_local); // count the JMP
            let idx = index_of[target];
            emit_region_goto(body, idx, *target, cur_local, insns_local, disp_depth);
        }
        BlockExit::Jcc {
            cc,
            target,
            fallthru,
        } => {
            bump_insns(body, insns_local); // count the Jcc
            emit_cond_i32(body, *cc); // i32 0/1
            body.byte(op::IF);
            body.byte(0x40); // void
                             // Inside the `if`/`else` arms there is one extra enclosing control frame,
                             // so a `br $disp` (loop) must reach one level deeper than at body level.
            emit_region_succ(
                body,
                *target,
                index_of,
                cur_local,
                insns_local,
                disp_depth + 1,
            );
            body.byte(op::ELSE);
            emit_region_succ(
                body,
                *fallthru,
                index_of,
                cur_local,
                insns_local,
                disp_depth + 1,
            );
            body.byte(op::END);
        }
    }
}

/// `$insns += 1`.
fn bump_insns(body: &mut Body, insns_local: u32) {
    body.local_get(insns_local);
    body.i64_const(1);
    body.binop(op::I64_ADD);
    body.local_set(insns_local);
}

/// Emit the dispatch tail for a single successor at region byte offset `off`: an
/// in-region block continues the loop (budget permitting), an out-of-region target
/// `return`s.
fn emit_region_succ(
    body: &mut Body,
    off: usize,
    index_of: &alloc::collections::BTreeMap<usize, usize>,
    cur_local: u32,
    insns_local: u32,
    disp_depth: u32,
) {
    match index_of.get(&off) {
        Some(&idx) => emit_region_goto(body, idx, off, cur_local, insns_local, disp_depth),
        None => emit_region_return(body, off, insns_local),
    }
}

/// Emit: if the instruction budget is exhausted, `return (entry+off, $insns)` to the
/// driver (which resumes at the in-region block's rip); otherwise set `$cur = idx`
/// and branch to the dispatch loop (`br $disp`). `off` is the target block's
/// region-entry-relative byte offset (its guest rip = `entry_rip + off`).
fn emit_region_goto(
    body: &mut Body,
    idx: usize,
    off: usize,
    cur_local: u32,
    insns_local: u32,
    disp_depth: u32,
) {
    // if ($insns >= budget) { return (entry+off, $insns) }   (budget <= insns)
    body.local_get(BUDGET_LOCAL);
    body.local_get(insns_local);
    body.binop(op::I64_LE_S); // budget <= insns  → exhausted
    body.byte(op::IF);
    body.byte(0x40);
    push_rip_at(body, off as u64);
    body.local_get(insns_local);
    body.byte(op::RETURN);
    body.byte(op::END);
    // not exhausted: $cur = idx ; br $disp (re-dispatch)
    body.i64_const(idx as i64);
    body.local_set(cur_local);
    body.byte(op::BR);
    leb_u32(&mut body.code, disp_depth);
}

/// Emit a region exit: `return (entry+off, $insns)` (guest `rip == entry_rip + off`).
fn emit_region_return(body: &mut Body, off: usize, insns_local: u32) {
    push_rip_at(body, off as u64);
    body.local_get(insns_local);
    body.byte(op::RETURN);
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
        // ── inc/dec via 0xFF /0 (inc) and /1 (dec); PUSH r/m via /6 ──
        0xff => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let digit = d.reg & 7;
            match digit {
                0 | 1 => {
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    emit_inc_dec(body, &d.rm, addr, digit == 1, size);
                    DecodeResult::Ok
                }
                6 => {
                    // PUSH r/m64 — always an 8-byte operand (operand-size
                    // independent), pushed after the EA is computed.
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    let v = read_rm(body, &d.rm, 8, addr);
                    emit_push(body, |b| b.local_get(v));
                    DecodeResult::Ok
                }
                _ => DecodeResult::Stop, // call/jmp (2/3/4/5) — control flow
            }
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
        // ── PUSH r64 (0x50+r) / POP r64 (0x58+r) ──
        0x50..=0x57 => {
            let reg = u32::from(op - 0x50) | (u32::from(rex & 1) << 3);
            // PUSH always uses the full 64-bit register value.
            let v = body.local();
            push_masked_reg(body, reg, 8);
            body.local_set(v);
            emit_push(body, |b| b.local_get(v));
            DecodeResult::Ok
        }
        0x58..=0x5f => {
            let reg = u32::from(op - 0x58) | (u32::from(rex & 1) << 3);
            let v = emit_pop(body);
            write_reg(body, reg, 8, v);
            DecodeResult::Ok
        }
        // ── POP r/m64 (0x8F /0) ──
        0x8f => {
            // Per the SDM the EA is computed after RSP is incremented by the pop.
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            if d.reg & 7 != 0 {
                return DecodeResult::Stop; // only /0 is POP r/m
            }
            match d.rm {
                RmLoc::Reg(_) => {
                    // Register destination: pop (commits RSP += 8) then write the
                    // register — no memory access after the RSP commit, so safe.
                    let v = emit_pop(body);
                    write_rm(body, &d.rm, 8, None, v);
                    DecodeResult::Ok
                }
                RmLoc::Mem(ref ea) => {
                    // Memory destination. The destination store can page-fault; to be
                    // fault-restart-safe the store must precede the `RSP += 8` commit
                    // (so a trap leaves RSP unchanged and the interpreter re-runs the
                    // pop cleanly). That reordering is only valid when the destination
                    // EA does NOT depend on RSP (the interpreter computes the EA from
                    // the *post*-increment RSP); an RSP-relative `pop [rsp±…]` is left
                    // to the interpreter (a rare form).
                    if ea.base == Some(RSP) || ea.index == Some(RSP) {
                        return DecodeResult::Stop;
                    }
                    // sp = RSP ; v = load(sp) ; store(EA, v) ; RSP = sp + 8.
                    let sp = body.local();
                    body.load_reg(RSP);
                    body.local_set(sp);
                    let v = body.local();
                    body.emit_load(sp, 8);
                    body.local_set(v);
                    let addr = emit_ea(body, ea, dec.pos);
                    body.emit_store(addr, 8, |b| b.local_get(v));
                    // store succeeded — commit RSP += 8
                    body.store_reg(RSP, |b| {
                        b.local_get(sp);
                        b.i64_const(8);
                        b.binop(op::I64_ADD);
                    });
                    DecodeResult::Ok
                }
            }
        }
        // ── LEA (0x8D) — effective address into reg, no memory access, no flags ──
        0x8d => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            // A register r/m form of LEA is illegal (#UD); stop the block.
            let RmLoc::Mem(ref ea) = d.rm else {
                return DecodeResult::Stop;
            };
            let addr = emit_ea(body, ea, dec.pos);
            emit_lea(body, d.reg, addr, size);
            DecodeResult::Ok
        }
        // ── TEST r/m, r (0x85) ──
        0x85 => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            let a = read_rm(body, &d.rm, size, addr);
            let b = body.local();
            push_masked_reg(body, d.reg, size);
            body.local_set(b);
            emit_test(body, a, b, size);
            DecodeResult::Ok
        }
        // ── TEST eAX, imm (0xA9) ──
        0xa9 => {
            let imm: u64 = {
                let Some(v) = dec.u32_le() else {
                    return DecodeResult::Stop;
                };
                if size == 8 {
                    (v as i32 as i64) as u64
                } else {
                    u64::from(v)
                }
            };
            let a = read_rm(body, &RmLoc::Reg(RAX), size, None);
            let b = body.local();
            body.i64_const(imm as i64);
            if size < 8 {
                body.i64_const(size_mask(size));
                body.binop(op::I64_AND);
            }
            body.local_set(b);
            emit_test(body, a, b, size);
            DecodeResult::Ok
        }
        // ── MOVSXD r64, r/m32 (0x63) ──
        0x63 => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            let src = read_rm_narrow(body, &d.rm, 4, addr, rex_present);
            emit_movx(body, d.reg, src, 4, true, size);
            DecodeResult::Ok
        }
        // ── IMUL r, r/m, imm32 (0x69) / imm8 (0x6B) ──
        0x69 | 0x6b => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let imm: i64 = if op == 0x6b {
                let Some(b) = dec.u8() else {
                    return DecodeResult::Stop;
                };
                b as i8 as i64
            } else {
                let Some(v) = dec.u32_le() else {
                    return DecodeResult::Stop;
                };
                v as i32 as i64
            };
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_imul3(body, d.reg, &d.rm, addr, imm, size);
            DecodeResult::Ok
        }
        // ── shifts SHL/SHR/SAR: 0xC1 imm8, 0xD1 by 1, 0xD3 by CL ──
        0xc1 | 0xd1 | 0xd3 => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let Some(sop) = ShiftOp::from_digit((d.reg & 7) as u8) else {
                return DecodeResult::Stop; // ROL/ROR/RCL/RCR deferred
            };
            // Fetch the count source BEFORE resolving a RIP-relative EA so the EA's
            // instruction-end matches the interpreter (the imm8 of 0xC1 follows the
            // ModRM/SIB/disp; 0xD1/0xD3 have no trailing immediate).
            let cnt = body.local();
            match op {
                0xc1 => {
                    let Some(imm) = dec.u8() else {
                        return DecodeResult::Stop;
                    };
                    body.i64_const(i64::from(imm));
                    body.local_set(cnt);
                }
                0xd1 => {
                    body.i64_const(1);
                    body.local_set(cnt);
                }
                _ => {
                    // 0xD3: count = CL = r[RCX] & 0xff.
                    body.load_reg(1); // RCX
                    body.i64_const(0xff);
                    body.binop(op::I64_AND);
                    body.local_set(cnt);
                }
            }
            let addr = maybe_ea(body, &d.rm, dec.pos);
            emit_shift(body, sop, &d.rm, addr, cnt, size);
            DecodeResult::Ok
        }
        // ── group3 (0xF7): TEST/NOT/NEG/MUL/IMUL (DIV/IDIV deferred) ──
        0xf7 => {
            let Some(d) = decode_modrm(dec, rex) else {
                return DecodeResult::Stop;
            };
            let digit = d.reg & 7;
            match digit {
                0 | 1 => {
                    // TEST r/m, imm32-sext. Immediate fetched before EA resolve.
                    let imm: u64 = {
                        let Some(v) = dec.u32_le() else {
                            return DecodeResult::Stop;
                        };
                        if size == 8 {
                            (v as i32 as i64) as u64
                        } else {
                            u64::from(v)
                        }
                    };
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    let a = read_rm(body, &d.rm, size, addr);
                    let b = body.local();
                    body.i64_const(imm as i64);
                    if size < 8 {
                        body.i64_const(size_mask(size));
                        body.binop(op::I64_AND);
                    }
                    body.local_set(b);
                    emit_test(body, a, b, size);
                    DecodeResult::Ok
                }
                2 => {
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    emit_not(body, &d.rm, addr, size);
                    DecodeResult::Ok
                }
                3 => {
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    emit_neg(body, &d.rm, addr, size);
                    DecodeResult::Ok
                }
                4 | 5 => {
                    // MUL (/4) / IMUL (/5): RDX:RAX = RAX * r/m.
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    emit_muldiv_mul(body, &d.rm, addr, digit == 5, size);
                    DecodeResult::Ok
                }
                // DIV (/6) / IDIV (/7) can raise #DE; the JIT has no exception
                // path, so they stop the block (the interpreter handles them).
                _ => DecodeResult::Stop,
            }
        }
        // 0x0F two-byte: Jcc rel32 (0x80..0x8F) terminator, plus SETcc, MOVZX/
        // MOVSX, and 2-operand IMUL; every other 0x0F opcode stops the block.
        0x0f => {
            let Some(op2) = dec.u8() else {
                return DecodeResult::Stop;
            };
            match op2 {
                0x80..=0x8f => {
                    let Some(rel) = dec.u32_le() else {
                        return DecodeResult::Stop;
                    };
                    let fallthru = dec.pos as u64;
                    let taken = fallthru.wrapping_add(rel as i32 as i64 as u64);
                    emit_jcc(body, next_rip_local, op2 - 0x80, taken, fallthru);
                    DecodeResult::Terminator
                }
                // SETcc r/m8 (0x90..0x9F) — write a 0/1 byte by condition.
                0x90..=0x9f => {
                    let Some(d) = decode_modrm(dec, rex) else {
                        return DecodeResult::Stop;
                    };
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    emit_setcc(body, op2 - 0x90, &d.rm, addr, rex_present);
                    DecodeResult::Ok
                }
                // MOVZX r, r/m8 (0xB6) / r/m16 (0xB7).
                0xb6 | 0xb7 => {
                    let ssz: u8 = if op2 == 0xb6 { 1 } else { 2 };
                    let Some(d) = decode_modrm(dec, rex) else {
                        return DecodeResult::Stop;
                    };
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    let src = read_rm_narrow(body, &d.rm, ssz, addr, rex_present);
                    emit_movx(body, d.reg, src, ssz, false, size);
                    DecodeResult::Ok
                }
                // MOVSX r, r/m8 (0xBE) / r/m16 (0xBF).
                0xbe | 0xbf => {
                    let ssz: u8 = if op2 == 0xbe { 1 } else { 2 };
                    let Some(d) = decode_modrm(dec, rex) else {
                        return DecodeResult::Stop;
                    };
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    let src = read_rm_narrow(body, &d.rm, ssz, addr, rex_present);
                    emit_movx(body, d.reg, src, ssz, true, size);
                    DecodeResult::Ok
                }
                // 2-operand IMUL r, r/m (0xAF).
                0xaf => {
                    let Some(d) = decode_modrm(dec, rex) else {
                        return DecodeResult::Stop;
                    };
                    let addr = maybe_ea(body, &d.rm, dec.pos);
                    emit_imul2(body, d.reg, &d.rm, addr, size);
                    DecodeResult::Ok
                }
                _ => DecodeResult::Stop,
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
    //   0: the exported `run` — a single block is `(i64) -> (i64,i64)` (entry_rip
    //      -> next_rip,insns); a region is `(i64,i64) -> (i64,i64)` (entry_rip,
    //      budget -> exit_rip, insns). Selected by `body.param_count`.
    //   1: (i64, i32) -> i64          (env.load(addr, size) -> value)
    //   2: (i64, i32, i64) -> ()      (env.store(addr, size, value))
    {
        let mut p = Vec::new();
        leb_u32(&mut p, 3); // 3 types
                            // type 0: (i64 [, i64]) -> (i64, i64)
        p.push(0x60);
        leb_u32(&mut p, body.param_count); // 1 (block) or 2 (region) params
        p.resize(p.len() + body.param_count as usize, 0x7e); // i64 ($entry_rip [, $budget])
        leb_u32(&mut p, 2); // 2 results
        p.push(0x7e); // i64 (next_rip / exit_rip)
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
