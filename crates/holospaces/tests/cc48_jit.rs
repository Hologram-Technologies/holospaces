//! **CC-48 differential + speed test** — the correctness gate for the x86-64 →
//! WebAssembly dynamic binary translator ([`holospaces::emulator::x64_jit`]).
//!
//! For every supported opcode/flag edge (hand-written blocks) plus randomized
//! register/immediate fuzzing, this test:
//!   1. runs the block on the real interpreter (`holospaces::emulator::x64::Cpu`,
//!      qemu-validated by CC-44) from a known register state, capturing the final
//!      16 GPRs + the 5 arithmetic flags (CF,PF,ZF,SF,OF);
//!   2. translates the same block via `x64_jit::translate_block`, instantiates the
//!      emitted Wasm with wasmtime over a host memory holding the register file,
//!      runs `run`, and reads the register file back;
//!   3. asserts the 16 GPRs and the 5 flags are IDENTICAL.
//!
//! Then it prints an interpreter-vs-JIT MIPS/speedup comparison on a hot block.

use holospaces::emulator::x64::{self, Cpu};
use holospaces::emulator::x64_jit::{translate_block, translate_block_at, TranslatedBlock};
use wasmtime::{Caller, Engine, Func, Instance, Memory, MemoryType, Module, Store, TypedFunc};

// The five arithmetic flag bits the translator computes.
const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const OF: u64 = 1 << 11;
const ARITH_FLAGS: u64 = CF | PF | ZF | SF | OF;

const RFLAGS_OFF: usize = 128;

/// Flat RAM size, shared by the interpreter core and the JIT host `load`/`store`
/// buffer so identity (paging-off) addresses index the same bytes in both.
const RAM_BYTES: usize = 64 * 1024;

/// The 16-byte-per-mov setup program installs the initial register state; the
/// block therefore begins at this guest address (its entry `rip`). Each setup
/// `mov reg, imm64` is REX.W + 0xB8+r + imm64 = 10 bytes; 16 of them = 160.
const BLOCK_BASE: u64 = 16 * 10;

/// Run `code` on the interpreter from `init` register state and `init_ram` flat
/// RAM (paging off → identity addressing), executing exactly `insns` block
/// instructions. Returns the final 16 GPRs, the arithmetic flags, and the full
/// final RAM (so a memory-touching block can be compared byte-for-byte).
fn run_interp(
    code: &[u8],
    init: &[u64; 16],
    init_ram: &[u8],
    insns: u32,
) -> ([u64; 16], u64, u64, Vec<u8>) {
    // The interpreter exposes no public register setter, so we prepend a
    // `mov reg, imm64` (REX.W + 0xB8+r) for each of the 16 GPRs to install the
    // known initial state, then run those 16 setup instructions plus the block.
    let mut prog = Vec::new();
    for (r, &v) in init.iter().enumerate() {
        // REX.W (0x48 | (r>=8 ? B bit)) ; 0xB8 + (r & 7) ; imm64
        let rex = 0x48 | (if r >= 8 { 0x01 } else { 0x00 });
        prog.push(rex);
        prog.push(0xB8 + (r as u8 & 7));
        prog.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(prog.len() as u64, BLOCK_BASE, "setup program size drift");
    prog.extend_from_slice(code);

    let mut cpu = Cpu::new(RAM_BYTES);
    // Seed the flat RAM (the block's data region) first, then overlay the code —
    // `load_at` writes the program bytes and resets rip/rsp.
    cpu.vv_ram_write(0, init_ram);
    cpu.load_at(0, &prog);
    // 16 setup movs + the block's instruction count.
    let halt = cpu.run(16 + u64::from(insns));
    // We expect to simply run out of budget after executing every instruction.
    assert!(
        matches!(halt, x64::Halt::OutOfBudget),
        "interpreter stopped unexpectedly: {halt:?}"
    );

    let mut regs = [0u64; 16];
    for (i, slot) in regs.iter_mut().enumerate() {
        *slot = cpu.reg(i);
    }
    let ram = cpu.vv_ram_read(0, RAM_BYTES);
    (regs, cpu.rflags() & ARITH_FLAGS, cpu.rip(), ram)
}

/// Instantiate a translated block on wasmtime over a host memory seeded with the
/// 16-u64 register file (rflags at offset 128 starts at the interpreter default
/// `0x2`), with `env.load`/`env.store` backed by a flat RAM buffer that mirrors
/// the interpreter's RAM (`init_ram_full`, identity-addressed). Runs `run`, then
/// returns the final GPRs, flags, the executed-insn count, and the final RAM.
fn run_jit(
    tb: &TranslatedBlock,
    init: &[u64; 16],
    init_ram_full: &[u8],
    entry_rip: u64,
) -> ([u64; 16], u64, u64, u32, Vec<u8>) {
    let engine = Engine::default();
    let module = Module::new(&engine, &tb.wasm).expect("translated module must validate");

    // The Store data holds the guest flat RAM the host load/store imports act on.
    let mut store = Store::new(&engine, init_ram_full.to_vec());
    // One page (64 KiB) of host memory; the register file lives at offset 0.
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).expect("memory");

    // Seed the register file: 16 u64 regs, then rflags = 0x2 (the interpreter's
    // reset value, bit 1 reserved-1) at offset 128.
    let data = mem.data_mut(&mut store);
    for (i, &v) in init.iter().enumerate() {
        data[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    data[RFLAGS_OFF..RFLAGS_OFF + 8].copy_from_slice(&0x2u64.to_le_bytes());

    // Host memory imports — mirror the interpreter's `rd`/`wr` over flat RAM:
    // load(addr, size) zero-extends `size` little-endian bytes; store writes them.
    // Out-of-range accesses read 0 / are dropped (matching the interpreter's
    // bounds-checked `ram.get`/`ram.get_mut`).
    let load = Func::wrap(
        &mut store,
        |caller: Caller<'_, Vec<u8>>, addr: i64, size: i32| -> i64 {
            let ram = caller.data();
            let a = addr as u64 as usize;
            let mut v = 0u64;
            for i in 0..size as usize {
                v |= u64::from(ram.get(a + i).copied().unwrap_or(0)) << (8 * i);
            }
            v as i64
        },
    );
    let store_fn = Func::wrap(
        &mut store,
        |mut caller: Caller<'_, Vec<u8>>, addr: i64, size: i32, val: i64| {
            let a = addr as u64 as usize;
            let v = val as u64;
            let ram = caller.data_mut();
            for i in 0..size as usize {
                if let Some(b) = ram.get_mut(a + i) {
                    *b = (v >> (8 * i)) as u8;
                }
            }
        },
    );

    let instance = Instance::new(
        &mut store,
        &module,
        &[mem.into(), load.into(), store_fn.into()],
    )
    .expect("instantiate");
    // `run(entry_rip) -> (next_rip, insns)`.
    let run: TypedFunc<i64, (i64, i64)> = instance
        .get_typed_func(&mut store, "run")
        .expect("run export");
    let (next_rip, ran) = run.call(&mut store, entry_rip as i64).expect("run call");
    let next_rip = next_rip as u64;
    let ran = ran as u32;

    let data = mem.data(&store);
    let mut regs = [0u64; 16];
    for (i, slot) in regs.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&data[i * 8..i * 8 + 8]);
        *slot = u64::from_le_bytes(b);
    }
    let mut fb = [0u8; 8];
    fb.copy_from_slice(&data[RFLAGS_OFF..RFLAGS_OFF + 8]);
    let flags = u64::from_le_bytes(fb) & ARITH_FLAGS;
    let ram = store.data().clone();
    (regs, flags, next_rip, ran, ram)
}

/// Assert the interpreter and JIT agree, bit-for-bit, on a register-only block
/// from `init` (no memory operands, flat RAM is all-zero).
fn check(label: &str, code: &[u8], init: &[u64; 16]) {
    check_mem(label, code, init, &[]);
}

/// Build the interpreter's starting RAM: `init_ram` overlaid (at offset 0) with
/// the 16 setup `mov`s plus the block `code` (what `load_at` writes). The JIT's
/// host RAM buffer must start identical so a memory-touching block compares
/// byte-for-byte against the interpreter's final RAM.
fn starting_ram(code: &[u8], init: &[u64; 16], init_ram: &[u8]) -> Vec<u8> {
    let mut ram = vec![0u8; RAM_BYTES];
    let n = init_ram.len().min(RAM_BYTES);
    ram[..n].copy_from_slice(&init_ram[..n]);
    let mut prog = Vec::new();
    for (r, &v) in init.iter().enumerate() {
        let rex = 0x48 | (if r >= 8 { 0x01 } else { 0x00 });
        prog.push(rex);
        prog.push(0xB8 + (r as u8 & 7));
        prog.extend_from_slice(&v.to_le_bytes());
    }
    prog.extend_from_slice(code);
    let m = prog.len().min(RAM_BYTES);
    ram[..m].copy_from_slice(&prog[..m]);
    ram
}

/// Assert the interpreter and JIT agree, bit-for-bit, on a (possibly
/// memory-touching) block from `init` register state and `init_ram` flat RAM
/// (identity-addressed, paging off): final 16 GPRs, the 5 arithmetic flags, AND
/// the full final RAM must match. The block runs at guest `rip == BLOCK_BASE`
/// (the JIT is told so, for RIP-relative parity).
fn check_mem(label: &str, code: &[u8], init: &[u64; 16], init_ram: &[u8]) {
    let tb = match translate_block_at(code, BLOCK_BASE) {
        Some(tb) => tb,
        None => panic!("[{label}] translate_block returned None for a supported block"),
    };
    assert_eq!(
        tb.bytes as usize,
        code.len(),
        "[{label}] translator should consume the whole supported block"
    );

    let start_ram = starting_ram(code, init, init_ram);
    let (iregs, iflags, irip, iram) = run_interp(code, init, init_ram, tb.insns);
    let (jregs, jflags, jrip, ran, jram) = run_jit(&tb, init, &start_ram, BLOCK_BASE);

    assert_eq!(
        ran, tb.insns,
        "[{label}] run() returned the wrong insn count"
    );
    assert_eq!(
        irip, jrip,
        "[{label}] next_rip mismatch: interp={irip:#x} jit={jrip:#x}"
    );
    for i in 0..16 {
        assert_eq!(
            iregs[i], jregs[i],
            "[{label}] r{i} mismatch: interp={:#x} jit={:#x}",
            iregs[i], jregs[i]
        );
    }
    assert_eq!(
        iflags, jflags,
        "[{label}] flags mismatch: interp={:#x} jit={:#x} (CF={},{} PF={},{} ZF={},{} SF={},{} OF={},{})",
        iflags, jflags,
        iflags & CF != 0, jflags & CF != 0,
        iflags & PF != 0, jflags & PF != 0,
        iflags & ZF != 0, jflags & ZF != 0,
        iflags & SF != 0, jflags & SF != 0,
        iflags & OF != 0, jflags & OF != 0,
    );
    // Compare RAM byte-for-byte; on mismatch report the first differing offset.
    if iram != jram {
        let off = iram
            .iter()
            .zip(jram.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "[{label}] RAM mismatch at {off:#x}: interp={:#x} jit={:#x}",
            iram[off], jram[off]
        );
    }
}

// ── REX/ModRM assembly helpers ────────────────────────────────────────────────

/// REX prefix for a two-register-operand instruction (reg in ModRM.reg, rm in
/// ModRM.rm). `w` selects 64-bit operand size.
fn rex(w: bool, reg: u8, rm: u8) -> u8 {
    0x40 | (u8::from(w) << 3) | (((reg >> 3) & 1) << 2) | ((rm >> 3) & 1)
}

/// ModRM byte for register-direct (mod=3): reg in [5:3], rm in [2:0].
fn modrm_rr(reg: u8, rm: u8) -> u8 {
    0xC0 | ((reg & 7) << 3) | (rm & 7)
}

/// `op reg/rm` two-operand instruction: `[rex, opcode, modrm]`.
fn instr_rr(w: bool, opcode: u8, reg: u8, rm: u8) -> Vec<u8> {
    vec![rex(w, reg, rm), opcode, modrm_rr(reg, rm)]
}

/// group1 `0x83 /digit rm, imm8` : `[rex, 0x83, modrm(digit,rm), imm8]`.
fn instr_g1_imm8(w: bool, digit: u8, rm: u8, imm8: i8) -> Vec<u8> {
    vec![rex(w, digit, rm), 0x83, modrm_rr(digit, rm), imm8 as u8]
}

/// group1 `0x81 /digit rm, imm32` : `[rex, 0x81, modrm(digit,rm), imm32 le]`.
fn instr_g1_imm32(w: bool, digit: u8, rm: u8, imm32: i32) -> Vec<u8> {
    let mut v = vec![rex(w, digit, rm), 0x81, modrm_rr(digit, rm)];
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `mov reg, imm` (0xB8+r). 64-bit imm if `w`, else imm32.
fn instr_mov_imm(w: bool, reg: u8, imm: u64) -> Vec<u8> {
    let r = 0x40 | (u8::from(w) << 3) | ((reg >> 3) & 1);
    let mut v = vec![r, 0xB8 + (reg & 7)];
    if w {
        v.extend_from_slice(&imm.to_le_bytes());
    } else {
        v.extend_from_slice(&(imm as u32).to_le_bytes());
    }
    v
}

/// `mov rm, imm32-sext` (0xC7 /0).
fn instr_movc7(w: bool, rm: u8, imm32: i32) -> Vec<u8> {
    let mut v = vec![rex(w, 0, rm), 0xC7, modrm_rr(0, rm)];
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `inc`/`dec` rm (0xFF /0 or /1).
fn instr_incdec(w: bool, rm: u8, dec: bool) -> Vec<u8> {
    let digit = if dec { 1 } else { 0 };
    vec![rex(w, digit, rm), 0xFF, modrm_rr(digit, rm)]
}

// ── Relative branch assembly ──────────────────────────────────────────────────

/// `JMP rel8` (0xEB).
fn instr_jmp_rel8(rel: i8) -> Vec<u8> {
    vec![0xEB, rel as u8]
}

/// `JMP rel32` (0xE9).
fn instr_jmp_rel32(rel: i32) -> Vec<u8> {
    let mut v = vec![0xE9];
    v.extend_from_slice(&rel.to_le_bytes());
    v
}

/// `Jcc rel8` (0x70+cc).
fn instr_jcc_rel8(cc: u8, rel: i8) -> Vec<u8> {
    vec![0x70 + cc, rel as u8]
}

/// `Jcc rel32` (0x0F 0x80+cc).
fn instr_jcc_rel32(cc: u8, rel: i32) -> Vec<u8> {
    let mut v = vec![0x0F, 0x80 + cc];
    v.extend_from_slice(&rel.to_le_bytes());
    v
}

// ── Memory-operand ModRM assembly ─────────────────────────────────────────────

/// A memory effective-address spec for the test assembler.
#[derive(Clone, Copy)]
enum Mem {
    /// `[base + disp]` (base != rsp/rbp low-3 special cases handled via SIB).
    Base { base: u8, disp: i32 },
    /// `[base + index*scale + disp]` via a SIB byte.
    Sib {
        base: u8,
        index: u8,
        scale: u8,
        disp: i32,
    },
    /// `[index*scale + disp32]` — SIB no-base form (mod=0, base field = 5).
    NoBase { index: u8, scale: u8, disp: i32 },
    /// `[rip + disp32]` — RIP-relative.
    Rip { disp: i32 },
}

/// Choose the ModRM `mod` field for a displacement: 0 if it can be omitted
/// (only when the base's low 3 bits aren't 5/rbp), 1 for a disp8, else 2.
fn disp_mod(disp: i32, base_low5: bool) -> u8 {
    if disp == 0 && !base_low5 {
        0
    } else if (-128..=127).contains(&disp) {
        1
    } else {
        2
    }
}

fn push_disp(v: &mut Vec<u8>, md: u8, disp: i32) {
    match md {
        1 => v.push(disp as i8 as u8),
        2 => v.extend_from_slice(&disp.to_le_bytes()),
        _ => {}
    }
}

/// Encode a memory operand: returns `(rex_xb, bytes)` where `rex_xb` carries the
/// REX.X/REX.B extension bits for the index/base and `bytes` is the
/// ModRM(+SIB+disp) sequence, with the ModRM `reg` field left as `reg & 7`.
fn enc_mem(reg: u8, mem: Mem) -> (u8, Vec<u8>) {
    let regbits = (reg & 7) << 3;
    match mem {
        Mem::Base { base, disp } => {
            let blow = base & 7;
            // rsp (low3==4) forces a SIB; handle via the Sib/NoBase encoders.
            assert!(blow != 4, "use Mem::Sib for an rsp/r12 base");
            let md = disp_mod(disp, blow == 5);
            let mut v = vec![(md << 6) | regbits | blow];
            push_disp(&mut v, md, disp);
            let rex_b = (base >> 3) & 1;
            (rex_b, v)
        }
        Mem::Sib {
            base,
            index,
            scale,
            disp,
        } => {
            let blow = base & 7;
            // base low3==5 with mod==0 is the no-base form, so force a disp.
            let md = disp_mod(disp, blow == 5);
            let sib = (scale << 6) | ((index & 7) << 3) | blow;
            let mut v = vec![(md << 6) | regbits | 4, sib];
            push_disp(&mut v, md, disp);
            let rex_xb = (((index >> 3) & 1) << 1) | ((base >> 3) & 1);
            (rex_xb, v)
        }
        Mem::NoBase { index, scale, disp } => {
            // mod=0, rm=4 (SIB), SIB.base=5 → disp32, no base.
            let sib = (scale << 6) | ((index & 7) << 3) | 5;
            let mut v = vec![regbits | 4, sib];
            v.extend_from_slice(&disp.to_le_bytes());
            let rex_x = ((index >> 3) & 1) << 1;
            (rex_x, v)
        }
        Mem::Rip { disp } => {
            // mod=0, rm=5 → RIP-relative disp32.
            let mut v = vec![regbits | 5];
            v.extend_from_slice(&disp.to_le_bytes());
            (0, v)
        }
    }
}

/// REX byte for a memory-operand instruction with ModRM.reg = `reg`.
fn rex_mem(w: bool, reg: u8, rex_xb: u8) -> u8 {
    0x40 | (u8::from(w) << 3) | (((reg >> 3) & 1) << 2) | rex_xb
}

/// `op mem, reg` / `op reg, mem` two-operand instruction: `[rex, opcode, ModRM…]`.
fn instr_rm(w: bool, opcode: u8, reg: u8, mem: Mem) -> Vec<u8> {
    let (rex_xb, modrm) = enc_mem(reg, mem);
    let mut v = vec![rex_mem(w, reg, rex_xb), opcode];
    v.extend_from_slice(&modrm);
    v
}

/// group1 `0x83 /digit mem, imm8`.
fn instr_g1m_imm8(w: bool, digit: u8, mem: Mem, imm8: i8) -> Vec<u8> {
    let mut v = instr_rm(w, 0x83, digit, mem);
    v.push(imm8 as u8);
    v
}

/// group1 `0x81 /digit mem, imm32`.
fn instr_g1m_imm32(w: bool, digit: u8, mem: Mem, imm32: i32) -> Vec<u8> {
    let mut v = instr_rm(w, 0x81, digit, mem);
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `mov mem, imm32-sext` (0xC7 /0).
fn instr_movc7_m(w: bool, mem: Mem, imm32: i32) -> Vec<u8> {
    let mut v = instr_rm(w, 0xC7, 0, mem);
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `inc`/`dec` mem (0xFF /0 or /1).
fn instr_incdec_m(w: bool, mem: Mem, dec: bool) -> Vec<u8> {
    instr_rm(w, 0xFF, if dec { 1 } else { 0 }, mem)
}

// ── New-opcode assembly helpers (push/pop, lea, test, movzx/sx, shift, setcc, …) ─

/// `PUSH r64` (0x50+r). REX only needed for r8..r15 (the B bit).
fn instr_push_r(reg: u8) -> Vec<u8> {
    let mut v = Vec::new();
    if reg >= 8 {
        v.push(0x41); // REX.B
    }
    v.push(0x50 + (reg & 7));
    v
}

/// `POP r64` (0x58+r).
fn instr_pop_r(reg: u8) -> Vec<u8> {
    let mut v = Vec::new();
    if reg >= 8 {
        v.push(0x41); // REX.B
    }
    v.push(0x58 + (reg & 7));
    v
}

/// `PUSH r/m64` (0xFF /6).
fn instr_push_m(mem: Mem) -> Vec<u8> {
    // operand size is always 64-bit for PUSH; REX.W is irrelevant, but the
    // memory operand may need REX.X/REX.B — encode with w=false.
    instr_rm(false, 0xFF, 6, mem)
}

/// `POP r/m64` (0x8F /0).
fn instr_pop_m(mem: Mem) -> Vec<u8> {
    instr_rm(false, 0x8F, 0, mem)
}

/// `LEA reg, mem` (0x8D).
fn instr_lea(w: bool, reg: u8, mem: Mem) -> Vec<u8> {
    instr_rm(w, 0x8D, reg, mem)
}

/// `TEST r/m, r` (0x85) register-direct.
fn instr_test_rr(w: bool, reg: u8, rm: u8) -> Vec<u8> {
    instr_rr(w, 0x85, reg, rm)
}

/// `TEST r/m, r` (0x85) with a memory r/m.
fn instr_test_rm(w: bool, reg: u8, mem: Mem) -> Vec<u8> {
    instr_rm(w, 0x85, reg, mem)
}

/// `TEST eAX/rAX, imm32` (0xA9). REX.W selects 64-bit (imm32 sign-extended).
fn instr_test_ax_imm(w: bool, imm32: i32) -> Vec<u8> {
    let mut v = Vec::new();
    if w {
        v.push(0x48);
    }
    v.push(0xA9);
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `TEST r/m, imm32` (0xF7 /0). Register-direct.
fn instr_test_imm_rr(w: bool, rm: u8, imm32: i32) -> Vec<u8> {
    let mut v = vec![rex(w, 0, rm), 0xF7, modrm_rr(0, rm)];
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `TEST r/m, imm32` (0xF7 /0). Memory r/m.
fn instr_test_imm_m(w: bool, mem: Mem, imm32: i32) -> Vec<u8> {
    let mut v = instr_rm(w, 0xF7, 0, mem);
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `NOT r/m` (0xF7 /2) register-direct, and `NEG r/m` (0xF7 /3).
fn instr_f7_unary(w: bool, digit: u8, rm: u8) -> Vec<u8> {
    vec![rex(w, digit, rm), 0xF7, modrm_rr(digit, rm)]
}

/// `NOT`/`NEG` r/m memory (0xF7 /2 or /3).
fn instr_f7_unary_m(w: bool, digit: u8, mem: Mem) -> Vec<u8> {
    instr_rm(w, 0xF7, digit, mem)
}

/// Single-operand `MUL`/`IMUL` (0xF7 /4 or /5) register-direct.
fn instr_f7_muldiv(w: bool, digit: u8, rm: u8) -> Vec<u8> {
    vec![rex(w, digit, rm), 0xF7, modrm_rr(digit, rm)]
}

/// Single-operand `MUL`/`IMUL` (0xF7 /4 or /5) with a memory r/m.
fn instr_f7_muldiv_m(w: bool, digit: u8, mem: Mem) -> Vec<u8> {
    instr_rm(w, 0xF7, digit, mem)
}

/// A 0x0F two-byte instruction `op2 reg, rm` register-direct: `[rex, 0x0F, op2, modrm]`.
fn instr_0f_rr(w: bool, op2: u8, reg: u8, rm: u8) -> Vec<u8> {
    vec![rex(w, reg, rm), 0x0F, op2, modrm_rr(reg, rm)]
}

/// A 0x0F two-byte instruction `op2 reg, [mem]`: `[rex, 0x0F, op2, ModRM…]`.
fn instr_0f_rm(w: bool, op2: u8, reg: u8, mem: Mem) -> Vec<u8> {
    let (rex_xb, modrm) = enc_mem(reg, mem);
    let mut v = vec![rex_mem(w, reg, rex_xb), 0x0F, op2];
    v.extend_from_slice(&modrm);
    v
}

/// `MOVZX`/`MOVSX` reg, r/m (op2 = 0xB6/0xB7/0xBE/0xBF) register-direct source.
fn instr_movx_rr(w: bool, op2: u8, reg: u8, rm: u8) -> Vec<u8> {
    instr_0f_rr(w, op2, reg, rm)
}

/// `MOVZX`/`MOVSX` reg, [mem] memory source.
fn instr_movx_rm(w: bool, op2: u8, reg: u8, mem: Mem) -> Vec<u8> {
    instr_0f_rm(w, op2, reg, mem)
}

/// `MOVSXD r64, r/m32` (0x63) register-direct.
fn instr_movsxd_rr(reg: u8, rm: u8) -> Vec<u8> {
    vec![rex(true, reg, rm), 0x63, modrm_rr(reg, rm)]
}

/// `MOVSXD r64, [mem]` memory.
fn instr_movsxd_rm(reg: u8, mem: Mem) -> Vec<u8> {
    instr_rm(true, 0x63, reg, mem)
}

/// `IMUL reg, rm, imm32` (0x69) register-direct.
fn instr_imul_imm32_rr(w: bool, reg: u8, rm: u8, imm32: i32) -> Vec<u8> {
    let mut v = vec![rex(w, reg, rm), 0x69, modrm_rr(reg, rm)];
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `IMUL reg, [mem], imm32` (0x69).
fn instr_imul_imm32_rm(w: bool, reg: u8, mem: Mem, imm32: i32) -> Vec<u8> {
    let mut v = instr_rm(w, 0x69, reg, mem);
    v.extend_from_slice(&imm32.to_le_bytes());
    v
}

/// `IMUL reg, rm, imm8` (0x6B) register-direct.
fn instr_imul_imm8_rr(w: bool, reg: u8, rm: u8, imm8: i8) -> Vec<u8> {
    vec![rex(w, reg, rm), 0x6B, modrm_rr(reg, rm), imm8 as u8]
}

/// `IMUL reg, [mem], imm8` (0x6B).
fn instr_imul_imm8_rm(w: bool, reg: u8, mem: Mem, imm8: i8) -> Vec<u8> {
    let mut v = instr_rm(w, 0x6B, reg, mem);
    v.push(imm8 as u8);
    v
}

/// `IMUL reg, rm` 2-operand (0x0F 0xAF) register-direct.
fn instr_imul2_rr(w: bool, reg: u8, rm: u8) -> Vec<u8> {
    instr_0f_rr(w, 0xAF, reg, rm)
}

/// `IMUL reg, [mem]` 2-operand (0x0F 0xAF).
fn instr_imul2_rm(w: bool, reg: u8, mem: Mem) -> Vec<u8> {
    instr_0f_rm(w, 0xAF, reg, mem)
}

/// Shift `SHL/SHR/SAR` r/m by imm8 (0xC1 /digit) register-direct.
fn instr_shift_imm_rr(w: bool, digit: u8, rm: u8, imm8: u8) -> Vec<u8> {
    vec![rex(w, digit, rm), 0xC1, modrm_rr(digit, rm), imm8]
}

/// Shift r/m by imm8 (0xC1 /digit) memory.
fn instr_shift_imm_m(w: bool, digit: u8, mem: Mem, imm8: u8) -> Vec<u8> {
    let mut v = instr_rm(w, 0xC1, digit, mem);
    v.push(imm8);
    v
}

/// Shift r/m by 1 (0xD1 /digit) register-direct.
fn instr_shift_1_rr(w: bool, digit: u8, rm: u8) -> Vec<u8> {
    vec![rex(w, digit, rm), 0xD1, modrm_rr(digit, rm)]
}

/// Shift r/m by 1 (0xD1 /digit) memory.
fn instr_shift_1_m(w: bool, digit: u8, mem: Mem) -> Vec<u8> {
    instr_rm(w, 0xD1, digit, mem)
}

/// Shift r/m by CL (0xD3 /digit) register-direct.
fn instr_shift_cl_rr(w: bool, digit: u8, rm: u8) -> Vec<u8> {
    vec![rex(w, digit, rm), 0xD3, modrm_rr(digit, rm)]
}

/// Shift r/m by CL (0xD3 /digit) memory.
fn instr_shift_cl_m(w: bool, digit: u8, mem: Mem) -> Vec<u8> {
    instr_rm(w, 0xD3, digit, mem)
}

/// `SETcc r/m8` (0x0F 0x90+cc) register-direct (low byte of the register).
fn instr_setcc_rr(cc: u8, rm: u8) -> Vec<u8> {
    // SETcc has no operand-size meaning; emit a REX only when needed for the B bit
    // so the low-byte (non-AH) encoding is used for r0..r3 (matches the interp's
    // rex_present low-byte handling).
    let mut v = Vec::new();
    if rm >= 8 {
        v.push(0x41); // REX.B
    }
    v.push(0x0F);
    v.push(0x90 + cc);
    v.push(modrm_rr(0, rm));
    v
}

/// `SETcc [mem]` (0x0F 0x90+cc) — store a byte.
fn instr_setcc_m(cc: u8, mem: Mem) -> Vec<u8> {
    let (rex_xb, modrm) = enc_mem(0, mem);
    let mut v = Vec::new();
    let rb = rex_mem(false, 0, rex_xb);
    if rb != 0x40 {
        v.push(rb);
    }
    v.push(0x0F);
    v.push(0x90 + cc);
    v.extend_from_slice(&modrm);
    v
}

// ── Hand-written edge-case blocks ─────────────────────────────────────────────

#[test]
fn handwritten_edges() {
    // A varied initial register state (mix of small, large, sign-bit-set values).
    let base: [u64; 16] = [
        5,
        7,
        0xffff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
        0x1234_5678_9abc_def0,
        0,
        0x7fff_ffff,
        0x8000_0000,
        1,
        0xdead_beef,
        0xffff_ffff,
        0x100,
        0x12,
        0x7fff_ffff_ffff_ffff,
        0xfedc_ba98_7654_3210,
        0xabcd,
    ];

    // ── mov forms ──
    check("mov r/m,r 64", &instr_rr(true, 0x89, 1, 0), &base); // mov rax, rcx
    check("mov r,r/m 64", &instr_rr(true, 0x8B, 2, 3), &base); // mov rdx, rbx
    check("mov r/m,r 32 (zext)", &instr_rr(false, 0x89, 2, 4), &base);
    check("mov r,r/m 32 (zext)", &instr_rr(false, 0x8B, 5, 6), &base);
    check(
        "mov r,imm64",
        &instr_mov_imm(true, 9, 0xdead_beef_cafe_babe),
        &base,
    );
    check(
        "mov r,imm32 (zext)",
        &instr_mov_imm(false, 2, 0xffff_ffff),
        &base,
    );
    check("mov r8,imm64", &instr_mov_imm(true, 8, 0x1), &base);
    check("mov r,imm32 via C7", &instr_movc7(true, 3, -1), &base); // sext to 64
    check(
        "mov r,imm32 via C7 32",
        &instr_movc7(false, 4, 0x7fff_ffff),
        &base,
    );

    // ── ALU reg forms — every op, both directions, both sizes ──
    for &(name, opcode, dir_to_rm) in &[
        ("add", 0x01u8, true),
        ("or", 0x09, true),
        ("and", 0x21, true),
        ("sub", 0x29, true),
        ("xor", 0x31, true),
        ("cmp", 0x39, true),
        ("add r", 0x03, false),
        ("or r", 0x0B, false),
        ("and r", 0x23, false),
        ("sub r", 0x2B, false),
        ("xor r", 0x33, false),
        ("cmp r", 0x3B, false),
    ] {
        // exercise a few register pairs including extended regs.
        for &(reg, rm) in &[(0u8, 1u8), (2, 3), (8, 9), (4, 12), (10, 5)] {
            check(
                &format!("{name} 64 r{reg},r{rm} (to_rm={dir_to_rm})"),
                &instr_rr(true, opcode, reg, rm),
                &base,
            );
            check(
                &format!("{name} 32 r{reg},r{rm} (to_rm={dir_to_rm})"),
                &instr_rr(false, opcode, reg, rm),
                &base,
            );
        }
    }

    // ── group1 imm8 / imm32, digits add/or/and/sub/xor/cmp ──
    for &digit in &[0u8, 1, 4, 5, 6, 7] {
        for &rm in &[0u8, 2, 8, 13] {
            for &imm in &[1i8, -1, 127, -128, 0] {
                check(
                    &format!("g1 imm8 d{digit} r{rm} #{imm} (64)"),
                    &instr_g1_imm8(true, digit, rm, imm),
                    &base,
                );
                check(
                    &format!("g1 imm8 d{digit} r{rm} #{imm} (32)"),
                    &instr_g1_imm8(false, digit, rm, imm),
                    &base,
                );
            }
            for &imm in &[1i32, -1, 0x7fff_ffff, i32::MIN, 0x1234] {
                check(
                    &format!("g1 imm32 d{digit} r{rm} #{imm} (64)"),
                    &instr_g1_imm32(true, digit, rm, imm),
                    &base,
                );
                check(
                    &format!("g1 imm32 d{digit} r{rm} #{imm} (32)"),
                    &instr_g1_imm32(false, digit, rm, imm),
                    &base,
                );
            }
        }
    }

    // ── inc / dec, both sizes, CF preservation ──
    for &rm in &[0u8, 2, 4, 8, 15] {
        check(
            &format!("inc64 r{rm}"),
            &instr_incdec(true, rm, false),
            &base,
        );
        check(
            &format!("dec64 r{rm}"),
            &instr_incdec(true, rm, true),
            &base,
        );
        check(
            &format!("inc32 r{rm}"),
            &instr_incdec(false, rm, false),
            &base,
        );
        check(
            &format!("dec32 r{rm}"),
            &instr_incdec(false, rm, true),
            &base,
        );
    }

    // ── overflow / carry edges ──
    // 0x7fff... + 1 → OF set (signed overflow), 0xffff... + 1 → CF set + ZF.
    check(
        "inc at INT_MAX (OF)",
        &instr_incdec(true, 13, false), // r13 = 0x7fff_ffff_ffff_ffff
        &base,
    );
    check(
        "inc at -1 (wrap, CF preserved, ZF)",
        &instr_incdec(true, 2, false), // r2 = 0xffff... → 0, ZF set, CF untouched
        &base,
    );

    // ── multi-instruction linear block ──
    let mut block = Vec::new();
    block.extend(instr_mov_imm(true, 0, 100)); // mov rax, 100
    block.extend(instr_mov_imm(true, 1, 7)); // mov rcx, 7
    block.extend(instr_rr(true, 0x01, 1, 0)); // add rax, rcx  (rax+=rcx)
    block.extend(instr_g1_imm8(true, 5, 0, 3)); // sub rax, 3
    block.extend(instr_incdec(true, 0, false)); // inc rax
    block.extend(instr_rr(true, 0x31, 2, 2)); // xor rdx, rdx
    block.extend(instr_rr(true, 0x89, 3, 0)); // mov rbx, rax
    check("linear multi-insn block", &block, &base);
}

// ── Hand-written memory-operand blocks ────────────────────────────────────────

/// The data region the memory-operand tests address (disjoint from the code at
/// `[0, BLOCK_BASE + block)` and the stack at the top of RAM).
const DATA: u64 = 0x1000;

/// A flat RAM image with a varied, sign-bit-mixing 64-bit pattern across the data
/// region so loads/stores at any size exercise real bytes.
fn seeded_ram() -> Vec<u8> {
    let mut ram = vec![0u8; RAM_BYTES];
    let pat: [u64; 8] = [
        0x0011_2233_4455_6677,
        0x8899_aabb_ccdd_eeff,
        0xffff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
        0x7fff_ffff_7fff_ffff,
        0x0000_0001_0000_0001,
        0xdead_beef_cafe_babe,
        0x1234_5678_9abc_def0,
    ];
    for (i, w) in pat.iter().enumerate() {
        let off = DATA as usize + i * 8;
        ram[off..off + 8].copy_from_slice(&w.to_le_bytes());
    }
    ram
}

/// An `init` register state where several registers point into the data region
/// (so register-based effective addresses land there).
fn mem_init() -> [u64; 16] {
    let mut init = [0u64; 16];
    init[0] = 5; // rax — a value to store
    init[1] = 0x0f0f_0f0f_0f0f_0f0f; // rcx — a value to AND/etc.
    init[2] = 7;
    init[3] = DATA; // rbx → data base
    init[4] = 2; // rsp-slot reused only as an index value here
    init[5] = DATA + 0x40; // rbp → data base + 0x40
    init[6] = DATA + 0x10; // rsi → data base + 0x10
    init[7] = DATA + 0x20; // rdi → data base + 0x20
    init[8] = 0xabcd_ef01; // r8
    init[9] = 4; // r9 — an index value
    init[10] = DATA + 0x30; // r10 → data base + 0x30
    init[11] = 0x99;
    init[12] = DATA + 0x8; // r12 → data base + 8 (r12 base forces a SIB)
    init[13] = 1;
    init[14] = DATA + 0x18; // r14
    init[15] = 0xfeed; // r15
    init
}

#[test]
fn handwritten_mem_edges() {
    let ram = seeded_ram();
    let init = mem_init();

    // ── addressing-mode coverage via `mov reg, [mem]` (0x8B) ──
    // [base] (mod=0), every non-special base register.
    for &b in &[0u8, 1, 3, 5, 6, 7, 10, 12, 14] {
        // r3/r5/r6/r7/r10/r12/r14 point into DATA; r0/r1 do not but reads are
        // bounds-safe (out-of-range → 0 in both engines).
        let mem = if (b & 7) == 4 || (b & 7) == 5 {
            Mem::Sib {
                base: b,
                index: 4,
                scale: 0,
                disp: 0,
            }
        } else {
            Mem::Base { base: b, disp: 0 }
        };
        check_mem(
            &format!("mov r2,[r{b}] (mod0)"),
            &instr_rm(true, 0x8B, 2, mem),
            &init,
            &ram,
        );
    }

    // disp8 / disp32 forms.
    check_mem(
        "mov r0,[rbx+8] disp8",
        &instr_rm(true, 0x8B, 0, Mem::Base { base: 3, disp: 8 }),
        &init,
        &ram,
    );
    check_mem(
        "mov r0,[rbx+0x30] disp32",
        &instr_rm(
            true,
            0x8B,
            0,
            Mem::Base {
                base: 3,
                disp: 0x30,
            },
        ),
        &init,
        &ram,
    );
    check_mem(
        "mov r0,[rbx-8] negative disp8",
        &instr_rm(
            true,
            0x8B,
            0,
            Mem::Base {
                base: 5, // rbp → DATA+0x40
                disp: -8,
            },
        ),
        &init,
        &ram,
    );

    // SIB base+index*scale, all scales.
    for &scale in &[0u8, 1, 2, 3] {
        check_mem(
            &format!("mov r0,[rbx+r9*{}+8]", 1 << scale),
            &instr_rm(
                true,
                0x8B,
                0,
                Mem::Sib {
                    base: 3,
                    index: 9, // r9 = 4
                    scale,
                    disp: 8,
                },
            ),
            &init,
            &ram,
        );
    }
    // SIB with an extended base (r12 forces SIB) and extended index.
    check_mem(
        "mov r0,[r12+r9*2]",
        &instr_rm(
            true,
            0x8B,
            0,
            Mem::Sib {
                base: 12,
                index: 9,
                scale: 1,
                disp: 0,
            },
        ),
        &init,
        &ram,
    );
    // SIB no-base: [index*scale + disp32].
    check_mem(
        "mov r0,[r9*4 + DATA]",
        &instr_rm(
            true,
            0x8B,
            0,
            Mem::NoBase {
                index: 9, // 4
                scale: 2, // *4 = 16
                disp: DATA as i32,
            },
        ),
        &init,
        &ram,
    );
    // SIB index==none (index field 4): [base + disp].
    check_mem(
        "mov r0,[rbx + (no index) + 0x18]",
        &instr_rm(
            true,
            0x8B,
            0,
            Mem::Sib {
                base: 3,
                index: 4, // no index
                scale: 0,
                disp: 0x18,
            },
        ),
        &init,
        &ram,
    );

    // RIP-relative: pick disp so EA = DATA. The block is one instruction;
    // instr_rm(0x8B reg,Rip) is REX+opcode+ModRM(1)+disp32(4) = 7 bytes, so the
    // instruction end is BLOCK_BASE + 7.
    {
        let len = instr_rm(true, 0x8B, 0, Mem::Rip { disp: 0 }).len() as i64;
        let disp = (DATA as i64) - (BLOCK_BASE as i64 + len);
        check_mem(
            "mov r0,[rip+disp]",
            &instr_rm(true, 0x8B, 0, Mem::Rip { disp: disp as i32 }),
            &init,
            &ram,
        );
    }

    // ── 32-bit operand loads (zero-extend the destination register) ──
    check_mem(
        "mov32 r0,[rbx] (zext)",
        &instr_rm(false, 0x8B, 0, Mem::Base { base: 3, disp: 0 }),
        &init,
        &ram,
    );

    // ── memory DESTINATION: mov [mem], reg (0x89) ──
    check_mem(
        "mov [rbx],rax (0x89)",
        &instr_rm(true, 0x89, 0, Mem::Base { base: 3, disp: 0 }),
        &init,
        &ram,
    );
    check_mem(
        "mov32 [rbx+0x10],rcx (0x89, 4-byte store)",
        &instr_rm(
            false,
            0x89,
            1,
            Mem::Base {
                base: 3,
                disp: 0x10,
            },
        ),
        &init,
        &ram,
    );
    // mov [mem], imm32 (0xC7 /0).
    check_mem(
        "mov qword [rbx+0x18], -1 (C7)",
        &instr_movc7_m(
            true,
            Mem::Base {
                base: 3,
                disp: 0x18,
            },
            -1,
        ),
        &init,
        &ram,
    );
    check_mem(
        "mov dword [rbx+0x20], 0x7fffffff (C7)",
        &instr_movc7_m(
            false,
            Mem::Base {
                base: 3,
                disp: 0x20,
            },
            0x7fff_ffff,
        ),
        &init,
        &ram,
    );

    // ── ALU with a memory operand, both directions, every op + flag edge ──
    for &(name, op_rm, op_r) in &[
        ("add", 0x01u8, 0x03u8),
        ("or", 0x09, 0x0B),
        ("and", 0x21, 0x23),
        ("sub", 0x29, 0x2B),
        ("xor", 0x31, 0x33),
        ("cmp", 0x39, 0x3B),
    ] {
        // op [mem], reg  (memory destination — load, compute, store; cmp no store)
        for &w in &[true, false] {
            check_mem(
                &format!("{name} [rbx+8],rcx w={w}"),
                &instr_rm(w, op_rm, 1, Mem::Base { base: 3, disp: 8 }),
                &init,
                &ram,
            );
            // op reg, [mem]  (memory source)
            check_mem(
                &format!("{name} rax,[rbx+0x10] w={w}"),
                &instr_rm(
                    w,
                    op_r,
                    0,
                    Mem::Base {
                        base: 3,
                        disp: 0x10,
                    },
                ),
                &init,
                &ram,
            );
        }
    }

    // ── group1 imm to memory, both widths, every digit ──
    for &digit in &[0u8, 1, 4, 5, 6, 7] {
        check_mem(
            &format!("g1 imm8 d{digit} [rbx+0x28]"),
            &instr_g1m_imm8(
                true,
                digit,
                Mem::Base {
                    base: 3,
                    disp: 0x28,
                },
                -1,
            ),
            &init,
            &ram,
        );
        check_mem(
            &format!("g1 imm32 d{digit} [rbx+0x28] (32-bit)"),
            &instr_g1m_imm32(
                false,
                digit,
                Mem::Base {
                    base: 3,
                    disp: 0x28,
                },
                0x1234_5678,
            ),
            &init,
            &ram,
        );
    }

    // ── inc / dec memory (CF preserved), both widths ──
    check_mem(
        "inc qword [rbx]",
        &instr_incdec_m(true, Mem::Base { base: 3, disp: 0 }, false),
        &init,
        &ram,
    );
    check_mem(
        "dec dword [rbx+8]",
        &instr_incdec_m(false, Mem::Base { base: 3, disp: 8 }, true),
        &init,
        &ram,
    );

    // ── a multi-instruction block mixing register and memory operands ──
    let mut block = Vec::new();
    block.extend(instr_rm(true, 0x8B, 0, Mem::Base { base: 3, disp: 0 })); // mov rax,[rbx]
    block.extend(instr_g1_imm8(true, 0, 0, 5)); // add rax, 5
    block.extend(instr_rm(true, 0x89, 0, Mem::Base { base: 3, disp: 8 })); // mov [rbx+8],rax
    block.extend(instr_rm(
        true,
        0x01,
        1,
        Mem::Base {
            base: 3,
            disp: 0x10,
        },
    )); // add [rbx+0x10],rcx
    block.extend(instr_incdec_m(
        true,
        Mem::Base {
            base: 3,
            disp: 0x18,
        },
        false,
    )); // inc qword [rbx+0x18]
    block.extend(instr_rm(
        true,
        0x33,
        2,
        Mem::Base {
            base: 3,
            disp: 0x20,
        },
    )); // xor rdx,[rbx+0x20]
    check_mem("mixed reg+mem block", &block, &init, &ram);
}

// ── New-opcode edge cases (push/pop, lea, test, movzx/sx, shift, setcc, neg/not,
//    imul, mul) ─────────────────────────────────────────────────────────────────

/// A register state with several registers pointing into the data region AND a
/// valid in-RAM stack pointer (rsp = `STACK_TOP`) for the PUSH/POP round-trips.
const STACK_TOP: u64 = 0x2000;

fn newop_init() -> [u64; 16] {
    let mut init = mem_init();
    init[4] = STACK_TOP; // rsp → a valid in-RAM stack
    init
}

#[test]
fn handwritten_newop_edges() {
    let ram = seeded_ram();
    let init = newop_init();

    // ── PUSH r64 / POP r64 round-trips (touch the stack memory) ──
    for &r in &[0u8, 1, 3, 8, 10, 15] {
        check_mem(&format!("push r{r}"), &instr_push_r(r), &init, &ram);
        check_mem(&format!("pop r{r}"), &instr_pop_r(r), &init, &ram);
    }
    // push then pop into a different register (a real round-trip through memory).
    {
        let mut blk = Vec::new();
        blk.extend(instr_push_r(3)); // push rbx
        blk.extend(instr_pop_r(0)); // pop rax  → rax == old rbx, rsp restored
        check_mem("push rbx; pop rax", &blk, &init, &ram);
    }
    // push two, pop two (LIFO order), exercising consecutive stack slots.
    {
        let mut blk = Vec::new();
        blk.extend(instr_push_r(0));
        blk.extend(instr_push_r(1));
        blk.extend(instr_pop_r(2)); // rdx = old rcx
        blk.extend(instr_pop_r(5)); // rbp = old rax
        check_mem("push/push/pop/pop", &blk, &init, &ram);
    }

    // ── PUSH r/m (0xFF /6) and POP r/m (0x8F /0) ──
    check_mem(
        "push qword [rbx]",
        &instr_push_m(Mem::Base { base: 3, disp: 0 }),
        &init,
        &ram,
    );
    check_mem(
        "pop qword [rbx+0x10]",
        &instr_pop_m(Mem::Base {
            base: 3,
            disp: 0x10,
        }),
        &init,
        &ram,
    );
    // push reg then pop into memory.
    {
        let mut blk = Vec::new();
        blk.extend(instr_push_r(8)); // push r8
        blk.extend(instr_pop_m(Mem::Base {
            base: 3,
            disp: 0x18,
        })); // pop [rbx+0x18]
        check_mem("push r8; pop [rbx+0x18]", &blk, &init, &ram);
    }

    // ── LEA: register dest, every addressing mode incl. SIB + RIP-relative ──
    check_mem(
        "lea r0,[rbx+0x10]",
        &instr_lea(
            true,
            0,
            Mem::Base {
                base: 3,
                disp: 0x10,
            },
        ),
        &init,
        &ram,
    );
    check_mem(
        "lea r0,[rbx+r9*4+8]",
        &instr_lea(
            true,
            0,
            Mem::Sib {
                base: 3,
                index: 9,
                scale: 2,
                disp: 8,
            },
        ),
        &init,
        &ram,
    );
    check_mem(
        "lea32 r0,[rbx+0x10] (zext)",
        &instr_lea(
            false,
            0,
            Mem::Base {
                base: 3,
                disp: 0x10,
            },
        ),
        &init,
        &ram,
    );
    check_mem(
        "lea r5,[r9*8 + DATA] (no base)",
        &instr_lea(
            true,
            5,
            Mem::NoBase {
                index: 9,
                scale: 3,
                disp: DATA as i32,
            },
        ),
        &init,
        &ram,
    );
    {
        let len = instr_lea(true, 0, Mem::Rip { disp: 0 }).len() as i64;
        let disp = (DATA as i64) - (BLOCK_BASE as i64 + len);
        check_mem(
            "lea r0,[rip+disp]",
            &instr_lea(true, 0, Mem::Rip { disp: disp as i32 }),
            &init,
            &ram,
        );
    }

    // ── TEST: r/m,r (0x85); rAX,imm (0xA9); r/m,imm (0xF7 /0) ──
    for &(reg, rm) in &[(0u8, 1u8), (2, 3), (8, 10)] {
        check_mem(
            &format!("test64 r{reg},r{rm}"),
            &instr_test_rr(true, reg, rm),
            &init,
            &ram,
        );
        check_mem(
            &format!("test32 r{reg},r{rm}"),
            &instr_test_rr(false, reg, rm),
            &init,
            &ram,
        );
    }
    check_mem(
        "test [rbx+8],rcx",
        &instr_test_rm(true, 1, Mem::Base { base: 3, disp: 8 }),
        &init,
        &ram,
    );
    check_mem("test rax,imm 64", &instr_test_ax_imm(true, -1), &init, &ram);
    check_mem(
        "test eax,imm 32",
        &instr_test_ax_imm(false, 0x7fff_ffff),
        &init,
        &ram,
    );
    check_mem(
        "test r3,imm (F7/0)",
        &instr_test_imm_rr(true, 3, 0x0f0f),
        &init,
        &ram,
    );
    check_mem(
        "test [rbx+0x10],imm (F7/0)",
        &instr_test_imm_m(
            true,
            Mem::Base {
                base: 3,
                disp: 0x10,
            },
            -1,
        ),
        &init,
        &ram,
    );

    // ── MOVZX / MOVSX, register source AND memory source, byte + word ──
    // Put discriminating values into a register so the narrow read is exercised.
    let mut mvinit = init;
    mvinit[1] = 0xffff_ffff_ffff_ff80; // low byte 0x80 (neg), low word 0xff80
    mvinit[2] = 0x0000_0000_0000_007f; // low byte 0x7f (pos)
    for &(name, op2, ssz) in &[
        ("movzx8", 0xB6u8, 1u8),
        ("movzx16", 0xB7, 2),
        ("movsx8", 0xBE, 1),
        ("movsx16", 0xBF, 2),
    ] {
        let _ = ssz;
        check_mem(
            &format!("{name} r0,r1 (64)"),
            &instr_movx_rr(true, op2, 0, 1),
            &mvinit,
            &ram,
        );
        check_mem(
            &format!("{name} r0,r2 (32 dest, zext upper)"),
            &instr_movx_rr(false, op2, 0, 2),
            &mvinit,
            &ram,
        );
        check_mem(
            &format!("{name} r0,[rbx] (mem src)"),
            &instr_movx_rm(true, op2, 0, Mem::Base { base: 3, disp: 0 }),
            &mvinit,
            &ram,
        );
    }
    // MOVSXD 32→64, register and memory.
    check_mem("movsxd r0,r1 (neg)", &instr_movsxd_rr(0, 1), &mvinit, &ram);
    check_mem(
        "movsxd r0,[rbx+8]",
        &instr_movsxd_rm(0, Mem::Base { base: 3, disp: 8 }),
        &mvinit,
        &ram,
    );

    // ── shifts SHL(4)/SHR(5)/SAR(7): imm8 (incl. 0), by 1, by CL (incl. CL=0) ──
    // Seed rax with a sign-bit-set value so SAR vs SHR differ; rcx (CL) varies.
    for &digit in &[4u8, 5, 7] {
        for &imm in &[0u8, 1, 3, 31, 63] {
            check_mem(
                &format!("shift64 d{digit} r0 by imm {imm}"),
                &instr_shift_imm_rr(true, digit, 0, imm),
                &init,
                &ram,
            );
            check_mem(
                &format!("shift32 d{digit} r0 by imm {imm}"),
                &instr_shift_imm_rr(false, digit, 0, imm),
                &init,
                &ram,
            );
        }
        // shift by 1 (0xD1).
        check_mem(
            &format!("shift64 d{digit} r3 by 1"),
            &instr_shift_1_rr(true, digit, 3),
            &init,
            &ram,
        );
        check_mem(
            &format!("shift d{digit} [rbx+8] by 1 (mem)"),
            &instr_shift_1_m(true, digit, Mem::Base { base: 3, disp: 8 }),
            &init,
            &ram,
        );
        // shift by CL (0xD3) — try CL set to several values via rcx.
        for &cl in &[0u64, 1, 5, 32, 63, 0x100 /* masks to 0 */] {
            let mut ci = init;
            ci[1] = cl; // rcx low byte = CL
            check_mem(
                &format!("shift64 d{digit} r0 by CL={cl}"),
                &instr_shift_cl_rr(true, digit, 0),
                &ci,
                &ram,
            );
            check_mem(
                &format!("shift32 d{digit} r0 by CL={cl}"),
                &instr_shift_cl_rr(false, digit, 0),
                &ci,
                &ram,
            );
        }
        // shift imm8 to a memory operand.
        check_mem(
            &format!("shift d{digit} [rbx] by imm 3 (mem)"),
            &instr_shift_imm_m(true, digit, Mem::Base { base: 3, disp: 0 }, 3),
            &init,
            &ram,
        );
        check_mem(
            &format!("shift d{digit} [rbx] by CL (mem)"),
            &instr_shift_cl_m(true, digit, Mem::Base { base: 3, disp: 0 }),
            &init,
            &ram,
        );
    }

    // ── SETcc, several conditions, register low-byte + memory ──
    // Run a flag-setting op first, then SETcc, to exercise real conditions.
    for cc in 0u8..16 {
        let mut blk = instr_rr(true, 0x29, 1, 0); // sub rax, rcx → set flags
        blk.extend(instr_setcc_rr(cc, 8)); // setcc r8b
        check_mem(&format!("setcc cc={cc} r8b"), &blk, &init, &ram);

        let mut blk = instr_rr(true, 0x01, 8, 13); // add r8, r13 (INT_MAX+small) → OF/SF
        blk.extend(instr_setcc_rr(cc, 0)); // setcc al
        check_mem(&format!("setcc cc={cc} al (after add)"), &blk, &init, &ram);

        let mut blk = instr_rr(true, 0x21, 1, 0); // and rax, rcx
        blk.extend(instr_setcc_m(
            cc,
            Mem::Base {
                base: 3,
                disp: 0x28,
            },
        )); // setcc [rbx+0x28]
        check_mem(&format!("setcc cc={cc} [mem]"), &blk, &init, &ram);
    }

    // ── NEG / NOT, register + memory, edge values (0, INT_MIN) ──
    {
        let mut ni = init;
        ni[0] = 0; // NEG 0 → r=0, CF=0, ZF=1
        ni[1] = 0x8000_0000_0000_0000; // INT_MIN → NEG = INT_MIN, OF=1
        ni[2] = 0x8000_0000; // 32-bit INT_MIN
        check_mem("neg r0 (0)", &instr_f7_unary(true, 3, 0), &ni, &ram);
        check_mem(
            "neg r1 (INT_MIN 64)",
            &instr_f7_unary(true, 3, 1),
            &ni,
            &ram,
        );
        check_mem(
            "neg32 r2 (INT_MIN 32)",
            &instr_f7_unary(false, 3, 2),
            &ni,
            &ram,
        );
        check_mem("not r3", &instr_f7_unary(true, 2, 3), &ni, &ram);
        check_mem("not32 r4", &instr_f7_unary(false, 2, 4), &ni, &ram);
        check_mem(
            "neg qword [rbx]",
            &instr_f7_unary_m(true, 3, Mem::Base { base: 3, disp: 0 }),
            &ni,
            &ram,
        );
        check_mem(
            "not dword [rbx+8]",
            &instr_f7_unary_m(false, 2, Mem::Base { base: 3, disp: 8 }),
            &ni,
            &ram,
        );
    }

    // ── IMUL 2-operand (0xAF) and 3-operand (0x69/0x6B), reg + mem ──
    check_mem("imul2 r0,r1 (64)", &instr_imul2_rr(true, 0, 1), &init, &ram);
    check_mem(
        "imul2 r0,r1 (32)",
        &instr_imul2_rr(false, 0, 1),
        &init,
        &ram,
    );
    check_mem(
        "imul2 r0,[rbx]",
        &instr_imul2_rm(true, 0, Mem::Base { base: 3, disp: 0 }),
        &init,
        &ram,
    );
    check_mem(
        "imul3 r0,r1,imm32",
        &instr_imul_imm32_rr(true, 0, 1, 0x1234),
        &init,
        &ram,
    );
    check_mem(
        "imul3 r0,r1,imm8 (neg)",
        &instr_imul_imm8_rr(true, 0, 1, -5),
        &init,
        &ram,
    );
    check_mem(
        "imul3 r0,[rbx],imm32 (32)",
        &instr_imul_imm32_rm(false, 0, Mem::Base { base: 3, disp: 0 }, -1),
        &init,
        &ram,
    );
    check_mem(
        "imul3 r0,[rbx+8],imm8",
        &instr_imul_imm8_rm(true, 0, Mem::Base { base: 3, disp: 8 }, 7),
        &init,
        &ram,
    );

    // ── single-operand MUL / IMUL (0xF7 /4,/5): RDX:RAX, overflow set + clear ──
    {
        // overflow CLEAR: small * small fits in the low half.
        let mut si = init;
        si[0] = 3; // rax
        si[3] = 5; // rbx — the multiplier r/m
        check_mem(
            "mul r3 (no overflow)",
            &instr_f7_muldiv(true, 4, 3),
            &si,
            &ram,
        );
        check_mem(
            "imul r3 (no overflow)",
            &instr_f7_muldiv(true, 5, 3),
            &si,
            &ram,
        );

        // overflow SET: large unsigned product spills into RDX.
        let mut oi = init;
        oi[0] = 0xffff_ffff_ffff_ffff; // rax
        oi[3] = 2; // rbx
        check_mem("mul r3 (overflow)", &instr_f7_muldiv(true, 4, 3), &oi, &ram);
        check_mem(
            "imul r3 (neg→overflow)",
            &instr_f7_muldiv(true, 5, 3),
            &oi,
            &ram,
        );

        // 32-bit MUL / IMUL.
        let mut wi = init;
        wi[0] = 0x1_0000; // rax (32-bit op uses low 32)
        wi[3] = 0x1_0000; // → product 0x1_0000_0000 spills to EDX
        check_mem(
            "mul32 r3 (overflow)",
            &instr_f7_muldiv(false, 4, 3),
            &wi,
            &ram,
        );
        check_mem("imul32 r3", &instr_f7_muldiv(false, 5, 3), &wi, &ram);

        // negative IMUL 64-bit (exercises the signed 128-bit correction).
        let mut neg = init;
        neg[0] = (-3i64) as u64;
        neg[3] = 7;
        check_mem("imul r3 (-3*7)", &instr_f7_muldiv(true, 5, 3), &neg, &ram);
        neg[0] = (-3i64) as u64;
        neg[3] = (-7i64) as u64;
        check_mem("imul r3 (-3*-7)", &instr_f7_muldiv(true, 5, 3), &neg, &ram);

        // MUL / IMUL with a memory r/m.
        check_mem(
            "mul qword [rbx]",
            &instr_f7_muldiv_m(true, 4, Mem::Base { base: 3, disp: 0 }),
            &init,
            &ram,
        );
        check_mem(
            "imul qword [rbx+8]",
            &instr_f7_muldiv_m(true, 5, Mem::Base { base: 3, disp: 8 }),
            &init,
            &ram,
        );
    }

    // ── a longer linear block mixing the new opcodes (block-length win) ──
    {
        let mut blk = Vec::new();
        blk.extend(instr_lea(
            true,
            0,
            Mem::Base {
                base: 3,
                disp: 0x10,
            },
        )); // lea rax,[rbx+0x10]
        blk.extend(instr_movx_rr(true, 0xB6, 1, 0)); // movzx rcx, al
        blk.extend(instr_shift_imm_rr(true, 4, 1, 3)); // shl rcx, 3
        blk.extend(instr_imul2_rr(true, 1, 3)); // imul rcx, rbx
        blk.extend(instr_test_rr(true, 1, 1)); // test rcx, rcx
        blk.extend(instr_f7_unary(true, 3, 2)); // neg rdx
        blk.extend(instr_push_r(1)); // push rcx
        blk.extend(instr_pop_r(5)); // pop rbp
        check_mem("linear new-opcode block", &blk, &init, &ram);
    }
}

// ── Branch terminators ────────────────────────────────────────────────────────

#[test]
fn handwritten_branch_edges() {
    let base: [u64; 16] = [
        5,
        7,
        0xffff_ffff_ffff_ffff,
        0x8000_0000_0000_0000,
        0x1234_5678_9abc_def0,
        0,
        0x7fff_ffff,
        0x8000_0000,
        1,
        0xdead_beef,
        0xffff_ffff,
        0x100,
        0x12,
        0x7fff_ffff_ffff_ffff,
        0xfedc_ba98_7654_3210,
        0xabcd,
    ];

    // ── unconditional JMP, forward + backward, rel8 + rel32 ──
    check("jmp rel8 +0", &instr_jmp_rel8(0), &base);
    check("jmp rel8 fwd", &instr_jmp_rel8(40), &base);
    check("jmp rel8 back", &instr_jmp_rel8(-20), &base);
    check("jmp rel8 min", &instr_jmp_rel8(i8::MIN), &base);
    check("jmp rel8 max", &instr_jmp_rel8(i8::MAX), &base);
    check("jmp rel32 +0", &instr_jmp_rel32(0), &base);
    check("jmp rel32 fwd", &instr_jmp_rel32(0x1234), &base);
    check("jmp rel32 back", &instr_jmp_rel32(-0x1000), &base);
    check(
        "jmp rel32 big-back",
        &instr_jmp_rel32(-(BLOCK_BASE as i32)),
        &base,
    );

    // A JMP after some arithmetic (the arithmetic sets flags; JMP ignores them).
    {
        let mut blk = Vec::new();
        blk.extend(instr_rr(true, 0x01, 0, 1)); // add rax, rcx
        blk.extend(instr_jmp_rel8(0x10));
        check("arith then jmp rel8", &blk, &base);
    }

    // ── conditional Jcc: every condition, several flag states, rel8 + rel32 ──
    // Flag-producing setups (a `cmp`-like or `add` over a register pair) drive a
    // spread of CF/ZF/SF/OF/PF; the interpreter and JIT must agree on taken/not
    // for ALL 16 conditions regardless of which way each setup resolves them.
    // Each entry is `op reg, reg` chosen to land a distinct flag combination.
    let setups: &[(&str, Vec<u8>)] = &[
        // sub rax,rax → ZF=1, CF=0, SF=0, OF=0, PF=1 (result 0)
        ("zero", instr_rr(true, 0x29, 0, 0)),
        // sub rax,rcx with rax=5,rcx=7 → negative, CF=1 (borrow), SF=1, OF=0
        ("borrow-neg", instr_rr(true, 0x29, 1, 0)),
        // add r2(-1)+r8(1) → 0, CF=1, ZF=1
        ("carry-zero", instr_rr(true, 0x01, 8, 2)),
        // add r13(INT_MAX)+r8(1) → OF=1, SF=1
        ("signed-of", instr_rr(true, 0x01, 8, 13)),
        // and rax,rcx (5 & 7 = 5) → CF=0,OF=0, SF=0, ZF=0, PF(parity of 5=101→2 ones→even? no, PF set on even)
        ("logic", instr_rr(true, 0x21, 1, 0)),
        // cmp r6(0x7fffffff),r7(0x80000000) 32-bit → exercises 32-bit flags
        ("cmp32", instr_rr(false, 0x39, 7, 6)),
    ];

    for cc in 0u8..16 {
        for (sname, setup) in setups {
            // rel8 form
            let mut blk = setup.clone();
            blk.extend(instr_jcc_rel8(cc, 0x12));
            check(&format!("jcc cc={cc} rel8 setup={sname}"), &blk, &base);

            // rel8 backward
            let mut blk = setup.clone();
            blk.extend(instr_jcc_rel8(cc, -0x10));
            check(&format!("jcc cc={cc} rel8-back setup={sname}"), &blk, &base);

            // rel32 form
            let mut blk = setup.clone();
            blk.extend(instr_jcc_rel32(cc, 0x2000));
            check(&format!("jcc cc={cc} rel32 setup={sname}"), &blk, &base);

            // rel32 backward
            let mut blk = setup.clone();
            blk.extend(instr_jcc_rel32(cc, -0x800));
            check(
                &format!("jcc cc={cc} rel32-back setup={sname}"),
                &blk,
                &base,
            );
        }
    }

    // ── Jcc as the very first/only instruction (flags = reset state 0x2) ──
    for cc in 0u8..16 {
        check(
            &format!("bare jcc cc={cc} rel8"),
            &instr_jcc_rel8(cc, 8),
            &base,
        );
        check(
            &format!("bare jcc cc={cc} rel32"),
            &instr_jcc_rel32(cc, 0x40),
            &base,
        );
    }

    // ── a block that ends by fall-through (no branch) still returns right RIP ──
    {
        let mut blk = Vec::new();
        blk.extend(instr_mov_imm(true, 0, 42));
        blk.extend(instr_rr(true, 0x01, 0, 1)); // add rax, rcx
        check("fallthrough no-branch rip", &blk, &base);
    }

    // ── multi-insn arithmetic block terminated by the matching Jcc ──
    {
        // mov rax,10; sub rax,10 (→ZF); je +0x20
        let mut blk = Vec::new();
        blk.extend(instr_mov_imm(true, 0, 10));
        blk.extend(instr_g1_imm8(true, 5, 0, 10)); // sub rax, 10 → ZF=1
        blk.extend(instr_jcc_rel8(0x4, 0x20)); // JE (cc=4) taken
        check("sub-to-zero then JE (taken)", &blk, &base);

        // mov rax,10; sub rax,3 (→ZF=0); je +0x20 (not taken → fallthrough)
        let mut blk = Vec::new();
        blk.extend(instr_mov_imm(true, 0, 10));
        blk.extend(instr_g1_imm8(true, 5, 0, 3)); // sub rax, 3 → ZF=0
        blk.extend(instr_jcc_rel8(0x4, 0x20)); // JE not taken
        check("sub-nonzero then JE (not taken)", &blk, &base);
    }
}

// ── Randomized fuzzing ────────────────────────────────────────────────────────

/// A tiny xorshift PRNG (deterministic, no external crate).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn pick<'a, T>(&mut self, s: &'a [T]) -> &'a T {
        &s[(self.next() as usize) % s.len()]
    }
}

/// Emit one random supported instruction into `out`.
fn random_insn(rng: &mut Rng, out: &mut Vec<u8>) {
    let alu_ops_rm = [0x01u8, 0x09, 0x21, 0x29, 0x31, 0x39];
    let alu_ops_r = [0x03u8, 0x0B, 0x23, 0x2B, 0x33, 0x3B];
    let kind = rng.next() % 18;
    let w = rng.next() & 1 == 0;
    let a = (rng.next() % 16) as u8;
    let b = (rng.next() % 16) as u8;
    match kind {
        0 => out.extend(instr_rr(w, *rng.pick(&alu_ops_rm), a, b)),
        1 => out.extend(instr_rr(w, *rng.pick(&alu_ops_r), a, b)),
        2 => {
            let digit = *rng.pick(&[0u8, 1, 4, 5, 6, 7]);
            out.extend(instr_g1_imm8(w, digit, a, rng.next() as i8));
        }
        3 => {
            let digit = *rng.pick(&[0u8, 1, 4, 5, 6, 7]);
            out.extend(instr_g1_imm32(w, digit, a, rng.next() as i32));
        }
        4 => out.extend(instr_mov_imm(w, a, rng.next())),
        5 => out.extend(instr_movc7(w, a, rng.next() as i32)),
        6 => out.extend(instr_incdec(w, a, rng.next() & 1 == 0)),
        7 => out.extend(instr_rr(w, 0x89, a, b)), // mov
        // ── new opcodes (register-direct forms) ──
        8 => out.extend(instr_test_rr(w, a, b)),
        9 => out.extend(instr_test_imm_rr(w, a, rng.next() as i32)),
        10 => out.extend(instr_f7_unary(w, *rng.pick(&[2u8, 3]), a)), // NOT/NEG
        11 => out.extend(instr_imul2_rr(w, a, b)),
        12 => out.extend(instr_imul_imm32_rr(w, a, b, rng.next() as i32)),
        13 => out.extend(instr_imul_imm8_rr(w, a, b, rng.next() as i8)),
        14 => {
            // shift SHL/SHR/SAR; mix imm8 / by-1 / by-CL forms.
            let digit = *rng.pick(&[4u8, 5, 7]);
            match rng.next() % 3 {
                0 => out.extend(instr_shift_imm_rr(w, digit, a, (rng.next() % 70) as u8)),
                1 => out.extend(instr_shift_1_rr(w, digit, a)),
                _ => out.extend(instr_shift_cl_rr(w, digit, a)),
            }
        }
        15 => {
            // MOVZX/MOVSX/MOVSXD into a register from a register source.
            match rng.next() % 5 {
                0 => out.extend(instr_movx_rr(w, 0xB6, a, b)),
                1 => out.extend(instr_movx_rr(w, 0xB7, a, b)),
                2 => out.extend(instr_movx_rr(w, 0xBE, a, b)),
                3 => out.extend(instr_movx_rr(w, 0xBF, a, b)),
                _ => out.extend(instr_movsxd_rr(a, b)),
            }
        }
        16 => out.extend(instr_setcc_rr((rng.next() % 16) as u8, a)),
        // single-operand MUL/IMUL (writes RDX:RAX).
        _ => out.extend(instr_f7_muldiv(w, *rng.pick(&[4u8, 5]), a)),
    }
}

#[test]
fn randomized_fuzz() {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    for case in 0..2000 {
        // Random initial register state.
        let mut init = [0u64; 16];
        for slot in init.iter_mut() {
            // bias toward edge values too.
            *slot = match rng.next() % 5 {
                0 => 0,
                1 => u64::MAX,
                2 => 1u64 << 63,
                3 => 0x7fff_ffff_ffff_ffff,
                _ => rng.next(),
            };
        }
        // A block of 1..=8 random instructions.
        let n = 1 + (rng.next() % 8);
        let mut code = Vec::new();
        for _ in 0..n {
            random_insn(&mut rng, &mut code);
        }
        // Half the time, terminate the block with a random relative branch so the
        // branch terminator + condition evaluation is fuzzed against the interp.
        if rng.next() & 1 == 0 {
            match rng.next() % 4 {
                0 => code.extend(instr_jmp_rel8(rng.next() as i8)),
                1 => code.extend(instr_jmp_rel32(rng.next() as i32)),
                2 => code.extend(instr_jcc_rel8((rng.next() % 16) as u8, rng.next() as i8)),
                _ => code.extend(instr_jcc_rel32((rng.next() % 16) as u8, rng.next() as i32)),
            }
        }
        check(&format!("fuzz case {case}"), &code, &init);
    }
}

/// Emit one random supported instruction that may carry a MEMORY operand, into
/// `out`. To keep the interpreter and JIT in lock-step over a flat RAM, every
/// effective address is constrained to the data region `[DATA, DATA+0x300)`:
///   * memory bases are the reserved pointer registers r13/r14/r15 (which point
///     into the data region and are never written by a fuzz instruction);
///   * indices are the reserved small-value registers r11/r12 (or none);
///   * displacements are bounded to `[0, 0x80)`.
///
/// All register operands (reg field and register r/m) are drawn from r0..=r10,
/// so the reserved registers stay constant for the whole block.
/// The general-purpose register operands the memory fuzzer may write: r0..=r10
/// except r4 (RSP), which is reserved as a valid in-RAM stack pointer so the
/// PUSH/POP forms touch real stack memory.
const MEM_FUZZ_REGS: [u8; 10] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10];

fn random_mem_insn(rng: &mut Rng, out: &mut Vec<u8>) {
    let alu_ops_rm = [0x01u8, 0x09, 0x21, 0x29, 0x31, 0x39];
    let alu_ops_r = [0x03u8, 0x0B, 0x23, 0x2B, 0x33, 0x3B];
    let w = rng.next() & 1 == 0;
    let reg = *rng.pick(&MEM_FUZZ_REGS); // r0..=r10 except RSP

    // A random data-region memory operand.
    let mk_mem = |rng: &mut Rng| -> Mem {
        let base = *rng.pick(&[13u8, 14, 15]);
        let disp = (rng.next() % 0x80) as i32;
        match rng.next() % 4 {
            0 => Mem::Base { base, disp },
            1 => Mem::Sib {
                base,
                index: 4, // no index
                scale: (rng.next() % 4) as u8,
                disp,
            },
            2 => Mem::Sib {
                base,
                index: *rng.pick(&[11u8, 12]),
                scale: (rng.next() % 4) as u8,
                disp,
            },
            _ => Mem::NoBase {
                index: *rng.pick(&[11u8, 12]),
                scale: (rng.next() % 4) as u8,
                disp: DATA as i32 + disp,
            },
        }
    };
    let mem = mk_mem(rng);

    match rng.next() % 18 {
        0 => out.extend(instr_rm(w, *rng.pick(&alu_ops_rm), reg, mem)), // op [mem], reg
        1 => out.extend(instr_rm(w, *rng.pick(&alu_ops_r), reg, mem)),  // op reg, [mem]
        2 => {
            let digit = *rng.pick(&[0u8, 1, 4, 5, 6, 7]);
            out.extend(instr_g1m_imm8(w, digit, mem, rng.next() as i8));
        }
        3 => {
            let digit = *rng.pick(&[0u8, 1, 4, 5, 6, 7]);
            out.extend(instr_g1m_imm32(w, digit, mem, rng.next() as i32));
        }
        4 => out.extend(instr_rm(w, 0x89, reg, mem)), // mov [mem], reg
        5 => out.extend(instr_rm(w, 0x8B, reg, mem)), // mov reg, [mem]
        6 => out.extend(instr_movc7_m(w, mem, rng.next() as i32)),
        7 => out.extend(instr_incdec_m(w, mem, rng.next() & 1 == 0)),
        // ── new opcodes with memory operands ──
        8 => out.extend(instr_test_rm(w, reg, mem)), // TEST [mem], reg
        9 => out.extend(instr_test_imm_m(w, mem, rng.next() as i32)), // TEST [mem], imm
        10 => out.extend(instr_f7_unary_m(w, *rng.pick(&[2u8, 3]), mem)), // NOT/NEG [mem]
        11 => out.extend(instr_f7_muldiv_m(w, *rng.pick(&[4u8, 5]), mem)), // MUL/IMUL [mem]
        12 => out.extend(instr_imul2_rm(w, reg, mem)), // IMUL reg, [mem]
        13 => out.extend(instr_imul_imm8_rm(w, reg, mem, rng.next() as i8)),
        14 => {
            // shift [mem] by imm8 / by 1 / by CL.
            let digit = *rng.pick(&[4u8, 5, 7]);
            match rng.next() % 3 {
                0 => out.extend(instr_shift_imm_m(w, digit, mem, (rng.next() % 70) as u8)),
                1 => out.extend(instr_shift_1_m(w, digit, mem)),
                _ => out.extend(instr_shift_cl_m(w, digit, mem)),
            }
        }
        15 => {
            // MOVZX/MOVSX/MOVSXD reg, [mem].
            match rng.next() % 5 {
                0 => out.extend(instr_movx_rm(w, 0xB6, reg, mem)),
                1 => out.extend(instr_movx_rm(w, 0xB7, reg, mem)),
                2 => out.extend(instr_movx_rm(w, 0xBE, reg, mem)),
                3 => out.extend(instr_movx_rm(w, 0xBF, reg, mem)),
                _ => out.extend(instr_movsxd_rm(reg, mem)),
            }
        }
        16 => out.extend(instr_setcc_m((rng.next() % 16) as u8, mem)),
        // PUSH/POP — reg and r/m forms (RSP is reserved valid; see MEM_FUZZ_REGS).
        _ => match rng.next() % 4 {
            0 => out.extend(instr_push_r(reg)),
            1 => out.extend(instr_pop_r(reg)),
            2 => out.extend(instr_push_m(mem)),
            _ => out.extend(instr_pop_m(mem)),
        },
    }
}

#[test]
fn randomized_fuzz_mem() {
    let mut rng = Rng(0xfeed_face_dead_c0de);
    for case in 0..2000 {
        // Seed the data region with a random pattern; reserved pointer/index
        // registers are fixed, the rest (r0..=r10) are random edge values.
        let mut ram = vec![0u8; RAM_BYTES];
        for off in (DATA as usize..DATA as usize + 0x300).step_by(8) {
            ram[off..off + 8].copy_from_slice(&rng.next().to_le_bytes());
        }
        let mut init = [0u64; 16];
        for slot in init.iter_mut().take(11) {
            *slot = match rng.next() % 5 {
                0 => 0,
                1 => u64::MAX,
                2 => 1u64 << 63,
                3 => 0x7fff_ffff_ffff_ffff,
                _ => rng.next(),
            };
        }
        // r4 (RSP) is reserved as a valid in-RAM stack pointer (a region disjoint
        // from the code and the data region) so the PUSH/POP fuzz forms touch real
        // stack memory; it is never used as a random reg operand (MEM_FUZZ_REGS).
        init[4] = 0x3000;
        init[11] = 1; // index regs: small values so index*scale stays bounded
        init[12] = 2;
        init[13] = DATA; // pointer bases into the data region
        init[14] = DATA + 0x100;
        init[15] = DATA + 0x200;

        // A block of 1..=6 random memory-or-register instructions.
        let n = 1 + (rng.next() % 6);
        let mut code = Vec::new();
        for _ in 0..n {
            if rng.next() & 1 == 0 {
                random_mem_insn(&mut rng, &mut code);
            } else {
                // a register-only instruction touching only r0..=r10 except RSP
                let a = *rng.pick(&MEM_FUZZ_REGS);
                let b = *rng.pick(&MEM_FUZZ_REGS);
                match rng.next() % 4 {
                    0 => code.extend(instr_rr(
                        rng.next() & 1 == 0,
                        *rng.pick(&[0x01u8, 0x21, 0x29, 0x31, 0x39]),
                        a,
                        b,
                    )),
                    1 => code.extend(instr_rr(rng.next() & 1 == 0, 0x89, a, b)),
                    2 => code.extend(instr_incdec(rng.next() & 1 == 0, a, rng.next() & 1 == 0)),
                    _ => code.extend(instr_g1_imm8(
                        rng.next() & 1 == 0,
                        *rng.pick(&[0u8, 1, 4, 5, 6, 7]),
                        a,
                        rng.next() as i8,
                    )),
                }
            }
        }
        check_mem(&format!("mem fuzz case {case}"), &code, &init, &ram);
    }
}

// ── Speed comparison ──────────────────────────────────────────────────────────

#[test]
fn speed_comparison() {
    // A hot register-only block: a body of ALU/mov ops, no control flow, repeated
    // so each translated `run` call executes a meaningful amount of work (the
    // steady-state execution path, not host-call overhead).
    let mut body = Vec::new();
    body.extend(instr_rr(true, 0x01, 0, 1)); // add rax, rcx
    body.extend(instr_rr(true, 0x21, 0, 2)); // and rax, rdx
    body.extend(instr_g1_imm8(true, 0, 0, 3)); // add rax, 3
    body.extend(instr_rr(true, 0x31, 1, 1)); // xor rcx, rcx
    body.extend(instr_incdec(true, 0, false)); // inc rax
    body.extend(instr_rr(true, 0x89, 3, 0)); // mov rbx, rax
    let mut block = Vec::new();
    for _ in 0..256 {
        block.extend_from_slice(&body);
    }

    let tb = translate_block(&block).expect("translate hot block");
    let init = [3u64, 0xff, 0x0f0f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

    // Correctness first (so the speed numbers describe a correct translation).
    check("hot block", &block, &init);

    // ~10M guest instructions through each engine.
    let block_insns = u64::from(tb.insns);
    let calls: u64 = 10_000_000 / block_insns;

    // ── Interpreter: run the whole block in one `run` call, `calls` times. ──
    // Warm setup program: 16 movs to set state, then the block.
    let mut prog = Vec::new();
    for (r, &v) in init.iter().enumerate() {
        prog.extend(instr_mov_imm(true, r as u8, v));
    }
    prog.extend_from_slice(&block);

    let mut cpu = Cpu::new(1 << 20);

    let interp_start = std::time::Instant::now();
    let mut total_interp_insns = 0u64;
    for _ in 0..calls {
        cpu.load_at(0, &prog); // reset rip/rsp to the program start
        let steps = 16 + block_insns;
        let _ = cpu.run(steps);
        total_interp_insns += block_insns;
    }
    let interp_dt = interp_start.elapsed();
    let interp_mips = (total_interp_insns as f64) / interp_dt.as_secs_f64() / 1e6;

    // ── JIT: instantiate once, call run() ITERS times. ──
    let engine = Engine::default();
    let module = Module::new(&engine, &tb.wasm).expect("module");
    // The module now always imports env.load/env.store; this hot block is
    // register-only so they are never called, but must be supplied.
    let mut store: Store<Vec<u8>> = Store::new(&engine, Vec::new());
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).expect("mem");
    {
        let data = mem.data_mut(&mut store);
        for (i, &v) in init.iter().enumerate() {
            data[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        data[RFLAGS_OFF..RFLAGS_OFF + 8].copy_from_slice(&0x2u64.to_le_bytes());
    }
    let load = Func::wrap(
        &mut store,
        |_c: Caller<'_, Vec<u8>>, _a: i64, _s: i32| -> i64 { 0 },
    );
    let store_fn = Func::wrap(
        &mut store,
        |_c: Caller<'_, Vec<u8>>, _a: i64, _s: i32, _v: i64| {},
    );
    let instance = Instance::new(
        &mut store,
        &module,
        &[mem.into(), load.into(), store_fn.into()],
    )
    .expect("inst");
    let run: TypedFunc<i64, (i64, i64)> = instance.get_typed_func(&mut store, "run").expect("run");

    let jit_start = std::time::Instant::now();
    let mut total_jit_insns = 0u64;
    for _ in 0..calls {
        // entry_rip is irrelevant for this register-only fall-through block.
        let (_next_rip, r) = run.call(&mut store, 0i64).expect("run");
        total_jit_insns += r as u64;
    }
    let jit_dt = jit_start.elapsed();
    let jit_mips = (total_jit_insns as f64) / jit_dt.as_secs_f64() / 1e6;

    let speedup = jit_mips / interp_mips;
    println!("\n=== CC-48 x86-64 → Wasm DBT speed comparison ===");
    println!(
        "interpreter : {:>10.1} MIPS  ({} insns in {:?})",
        interp_mips, total_interp_insns, interp_dt
    );
    println!(
        "JIT (wasmtime): {:>8.1} MIPS  ({} insns in {:?})",
        jit_mips, total_jit_insns, jit_dt
    );
    println!("speedup     : {speedup:.2}x");
    println!("================================================\n");

    assert!(jit_mips > 0.0 && interp_mips > 0.0);
}

#[test]
fn measure_block_length() {
    // A representative linear instruction mix (the shapes Node/V8 + kernel hot
    // paths emit), assembled back to back; we walk it translating block-by-block
    // and report the average translated-block length BEFORE vs the kinds of stops.
    let mut code = Vec::new();
    // function prologue-ish + arithmetic + new opcodes, no control flow inside.
    code.extend(instr_push_r(5)); // push rbp
    code.extend(instr_rr(true, 0x89, 5, 4)); // mov rbp, rsp
    code.extend(instr_rm(true, 0x8B, 0, Mem::Base { base: 3, disp: 0 })); // mov rax,[rbx]
    code.extend(instr_movx_rr(true, 0xB6, 1, 0)); // movzx rcx, al
    code.extend(instr_shift_imm_rr(true, 4, 1, 4)); // shl rcx, 4
    code.extend(instr_imul2_rr(true, 1, 3)); // imul rcx, rbx
    code.extend(instr_lea(
        true,
        2,
        Mem::Base {
            base: 1,
            disp: 0x10,
        },
    )); // lea rdx,[rcx+0x10]
    code.extend(instr_test_rr(true, 2, 2)); // test rdx, rdx
    code.extend(instr_setcc_rr(4, 8)); // sete r8b
    code.extend(instr_g1_imm8(true, 0, 0, 1)); // add rax, 1
    code.extend(instr_f7_unary(true, 3, 6)); // neg rsi
    code.extend(instr_movsxd_rr(7, 0)); // movsxd rdi, eax
    code.extend(instr_pop_r(5)); // pop rbp
    code.extend(instr_jmp_rel8(0x10)); // jmp (terminator)

    let mut pos = 0usize;
    let mut blocks = 0u32;
    let mut total_insns = 0u32;
    while pos < code.len() {
        match translate_block(&code[pos..]) {
            Some(tb) => {
                blocks += 1;
                total_insns += tb.insns;
                pos += tb.bytes as usize;
            }
            None => {
                pos += 1; // an unsupported lead byte; interpreter would step it
            }
        }
    }
    let avg = total_insns as f64 / blocks as f64;
    println!("\n=== CC-48 translated-block length on the representative sample ===");
    println!("blocks={blocks} total_insns={total_insns} avg_block_len={avg:.1}");
    println!("==================================================================\n");
    assert!(
        avg > 2.0,
        "new opcodes should grow blocks well past ~2 insns"
    );
}
