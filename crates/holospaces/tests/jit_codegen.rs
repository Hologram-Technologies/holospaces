//! JIT codegen + differential (the second slab: IR → wasm, proven bit-identical).
//!
//! A typed micro-op IR over a 16×u64 register file, an interpreter oracle, and a
//! register-allocated IR→wasm-bytecode codegen run via `wasmtime`. A seeded-random
//! differential check (interpret vs codegen→wasmtime) proves the codegen is bit-exact —
//! the discipline the real block JIT lives by. This covers the ALU/shift/rotate ops the
//! SHA-512 round function needs; the x86 *decoder* (bytes → this IR) is the next slab.

use wasmtime::{Engine, Instance, Module, Store};

const NREG: usize = 16;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Bin {
    Add,
    Sub,
    Xor,
    And,
    Or,
}
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sh {
    ShrU,
    Shl,
    Rotr,
}
#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Movi { d: u8, imm: u64 },
    Movr { d: u8, s: u8 },
    Bin { op: Bin, d: u8, a: u8, b: u8 },
    Shift { op: Sh, d: u8, a: u8, sh: u8 },
    /// `d = [r[base] + disp]` (the SHA-512 `W[]` schedule on the stack).
    Load { d: u8, base: u8, disp: i32 },
    /// `[r[base] + disp] = s`.
    Store { base: u8, disp: i32, s: u8 },
}

/// Guest RAM lives in the same wasm memory as the register file, after a page of
/// headroom: regs at `r*8`, guest byte `A` at wasm offset `GUEST_BASE + A`.
const GUEST_BASE: u64 = 0x1000;
const GUEST_LEN: usize = 0x3000; // 3 pages of test guest RAM

/// The reference oracle — the meaning of the IR, in plain Rust.
fn interpret(block: &[Op], r: &mut [u64; NREG], ram: &mut [u8]) {
    for op in block {
        match *op {
            Op::Movi { d, imm } => r[d as usize] = imm,
            Op::Movr { d, s } => r[d as usize] = r[s as usize],
            Op::Load { d, base, disp } => {
                let a = r[base as usize].wrapping_add(disp as i64 as u64) as usize;
                r[d as usize] = u64::from_le_bytes(ram[a..a + 8].try_into().unwrap());
            }
            Op::Store { base, disp, s } => {
                let a = r[base as usize].wrapping_add(disp as i64 as u64) as usize;
                ram[a..a + 8].copy_from_slice(&r[s as usize].to_le_bytes());
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

/// Register-allocated codegen: load all 16 regs from memory into i64 locals at entry,
/// compute on locals, store back at exit (per-op traffic is `local.get/set`, not memory).
fn compile(block: &[Op]) -> Vec<u8> {
    let mut code = Vec::new();
    // locals: one group of 16 × i64
    uleb(1, &mut code);
    uleb(NREG as u64, &mut code);
    code.push(0x7e); // i64
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
    // entry: regs → locals
    for r in 0..NREG {
        load_mem(r, &mut code);
        setl(r as u8, &mut code);
    }
    // body
    for op in block {
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
                sleb(i64::from(sh), &mut code); // i64.const shift
                code.push(match op {
                    Sh::ShrU => 0x88,
                    Sh::Shl => 0x86,
                    Sh::Rotr => 0x8a,
                });
                setl(d, &mut code);
            }
            // memory ops: wasm offset = (r[base] + disp) + GUEST_BASE, wrapped to i32
            Op::Load { d, base, disp } => {
                getl(base, &mut code);
                code.push(0x42);
                sleb(i64::from(disp), &mut code); // + disp
                code.push(0x7c); // i64.add
                code.push(0x42);
                sleb(GUEST_BASE as i64, &mut code); // + GUEST_BASE
                code.push(0x7c); // i64.add
                code.push(0xa7); // i32.wrap_i64
                code.extend([0x29, 0x03, 0x00]); // i64.load align=3
                setl(d, &mut code);
            }
            Op::Store { base, disp, s } => {
                getl(base, &mut code);
                code.push(0x42);
                sleb(i64::from(disp), &mut code);
                code.push(0x7c);
                code.push(0x42);
                sleb(GUEST_BASE as i64, &mut code);
                code.push(0x7c);
                code.push(0xa7); // i32.wrap_i64 → addr on stack
                getl(s, &mut code); // value to store
                code.extend([0x37, 0x03, 0x00]); // i64.store align=3
            }
        }
    }
    // exit: locals → regs
    for r in 0..NREG {
        code.push(0x41);
        sleb((r * 8) as i64, &mut code); // i32.const addr
        getl(r as u8, &mut code);
        code.extend([0x37, 0x03, 0x00]); // i64.store
    }
    code.push(0x0b); // end

    // assemble the module
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    section(1, vec![0x01, 0x60, 0x00, 0x00], &mut m); // type () -> ()
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

// A tiny seeded PRNG (deterministic, no Math.random/Date) for the fuzz corpus.
struct Rng(u64);
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

/// Minimal x86-64 decoder for the reg-reg ALU/mov subset (the SHA-512 compression
/// core): optional REX, opcode, ModRM (mod=3). Maps to the IR; **bails** (stops) on any
/// memory form or unknown opcode — exactly the JIT's "bail to the interpreter" discipline.
/// The full transform also needs the stack/table loads (Load/Store IR) — the next slab.
fn decode_x86(mut b: &[u8]) -> Vec<Op> {
    let mut out = Vec::new();
    while b.len() >= 2 {
        let mut rex = 0u8;
        if b[0] & 0xf0 == 0x40 {
            rex = b[0];
            b = &b[1..];
        }
        if b.len() < 2 {
            break;
        }
        let (opcode, modrm) = (b[0], b[1]);
        if modrm >> 6 != 3 {
            break; // memory form — outside this subset
        }
        let ext = |bit: u8| if rex & bit != 0 { 8u8 } else { 0 };
        let reg = ((modrm >> 3) & 7) | ext(0x04); // REX.R
        let rm = (modrm & 7) | ext(0x01); // REX.B
        let bin = |op| Op::Bin { op, d: rm, a: rm, b: reg }; // `op r/m, r` → r/m op= r
        let op = match opcode {
            0x01 => bin(Bin::Add),
            0x09 => bin(Bin::Or),
            0x21 => bin(Bin::And),
            0x29 => bin(Bin::Sub),
            0x31 => bin(Bin::Xor),
            0x89 => Op::Movr { d: rm, s: reg }, // mov r/m, r
            _ => break,
        };
        out.push(op);
        b = &b[2..];
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
    // base pointers held in regs 12..16 — never written by the block, so they stay valid.
    const BASE: [u8; 4] = [12, 13, 14, 15];
    for _ in 0..300 {
        let n = 1 + (rng.next() % 30);
        let mut block = Vec::new();
        for _ in 0..n {
            let d = (rng.next() % 12) as u8; // dst regs 0..12 only (keep base ptrs intact)
            let base = BASE[(rng.next() % 4) as usize];
            let disp = (rng.next() % 0x600) as i32; // in-range offset
            block.push(match rng.next() % 6 {
                0 => Op::Movi { d, imm: rng.next() },
                1 => Op::Bin { op: Bin::Add, d, a: rng.reg(), b: rng.reg() },
                2 => Op::Bin { op: Bin::Xor, d, a: rng.reg(), b: rng.reg() },
                3 => Op::Shift { op: Sh::Rotr, d, a: rng.reg(), sh: (rng.next() % 64) as u8 },
                4 => Op::Load { d, base, disp },
                _ => Op::Store { base, disp, s: rng.reg() },
            });
        }
        // base pointers spaced so [base+disp, +8) stays inside the test RAM.
        let mut regs = [0u64; NREG];
        for r in regs[..12].iter_mut() {
            *r = rng.next();
        }
        regs[12] = 0x0000;
        regs[13] = 0x0800;
        regs[14] = 0x1000;
        regs[15] = 0x1800;
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
