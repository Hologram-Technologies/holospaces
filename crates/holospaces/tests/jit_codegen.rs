//! JIT codegen + differential (the second slab: IR → wasm, proven bit-identical).
//!
//! A typed micro-op IR over a 16×u64 register file, an interpreter oracle, and a
//! register-allocated IR→wasm-bytecode codegen run via `wasmtime`. A seeded-random
//! differential check (interpret vs codegen→wasmtime) proves the codegen is bit-exact —
//! the discipline the real block JIT lives by. This covers the ALU/shift/rotate ops the
//! SHA-512 round function needs; the x86 *decoder* (bytes → this IR) is the next slab.

use wasmtime::{Engine, Instance, Module, Store};

const NREG: usize = 16;

#[derive(Clone, Copy)]
enum Bin {
    Add,
    Sub,
    Xor,
    And,
    Or,
}
#[derive(Clone, Copy)]
enum Sh {
    ShrU,
    Shl,
    Rotr,
}
#[derive(Clone, Copy)]
enum Op {
    Movi { d: u8, imm: u64 },
    Bin { op: Bin, d: u8, a: u8, b: u8 },
    Shift { op: Sh, d: u8, a: u8, sh: u8 },
}

/// The reference oracle — the meaning of the IR, in plain Rust.
fn interpret(block: &[Op], r: &mut [u64; NREG]) {
    for op in block {
        match *op {
            Op::Movi { d, imm } => r[d as usize] = imm,
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
    section(5, vec![0x01, 0x00, 0x01], &mut m); // memory min 1
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

/// Run a compiled block over a register file via wasmtime; return the resulting regs.
fn run_wasm(bytes: &[u8], regs: [u64; NREG]) -> [u64; NREG] {
    let engine = Engine::default();
    let module = Module::new(&engine, bytes).expect("emitted wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let run = instance.get_typed_func::<(), ()>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    run.call(&mut store, ()).expect("run");
    let mut out = [0u64; NREG];
    for (i, o) in out.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *o = u64::from_le_bytes(b);
    }
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
        interpret(&block, &mut want);
        let got = run_wasm(&compile(&block), regs);
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
    interpret(&block, &mut want);
    assert_eq!(run_wasm(&compile(&block), regs), want, "SHA-512-shaped block diverged");
}
