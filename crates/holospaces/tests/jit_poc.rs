//! JIT execution-substrate proof of concept (the first slab of the block JIT).
//!
//! Proves the native execution path the JIT will use: **Rust emits wasm bytecode at
//! runtime → `wasmtime` compiles/instantiates/runs it → the result is correct**, with
//! the guest register file living in the module's linear memory (`r[i]` at offset
//! `i*8`) — exactly the model a compiled x86 block uses. This de-risks the *execution
//! substrate* before any x86 decoder exists; the x86 decode→IR→codegen front-end is the
//! next slab. (Vehicle = `wasmtime` directly, NOT `hologram-runtime-wasmtime`, which is
//! a `.holo` container engine, not a bare-function runner.)

use wasmtime::{Engine, Instance, Module, Store};

/// Hand-emit the wasm bytecode for one JIT block: `r0 = r1 + r2` over a register file in
/// linear memory. This is the *real* codegen path (raw bytes), not WAT — the same kind of
/// module the SHA-512 codegen will emit, just minimal.
fn emit_add_block() -> Vec<u8> {
    vec![
        // magic + version
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
        // type section: one type `() -> ()`
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
        // function section: func 0 has type 0
        0x03, 0x02, 0x01, 0x00,
        // memory section: one memory, min 1 page
        0x05, 0x03, 0x01, 0x00, 0x01,
        // export section: "mem" -> memory 0, "run" -> func 0
        0x07, 0x0d, 0x02,
        0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00, // "mem", kind=memory(2), idx 0
        0x03, 0x72, 0x75, 0x6e, 0x00, 0x00, // "run", kind=func(0),   idx 0
        // code section: one body (18 bytes, 0 locals)
        0x0a, 0x14, 0x01, 0x12, 0x00,
        0x41, 0x00, //               i32.const 0          ; dst addr = &r0
        0x41, 0x08, 0x29, 0x03, 0x00, // i64.load  align=3 [&r1]
        0x41, 0x10, 0x29, 0x03, 0x00, // i64.load  align=3 [&r2]
        0x7c, //                     i64.add
        0x37, 0x03, 0x00, //         i64.store align=3 [&r0]
        0x0b, //                     end
    ]
}

#[test]
fn jit_substrate_runs_an_emitted_block_via_wasmtime() {
    let engine = Engine::default();
    let module = Module::new(&engine, emit_add_block()).expect("emitted wasm bytecode is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").expect("exported memory");
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .expect("run export");

    // The register file in linear memory: r[i] at byte offset i*8.
    let (r1, r2) = (0x1111_2222_3333_4444u64, 0x0000_0000_0001_0001u64);
    mem.write(&mut store, 8, &r1.to_le_bytes()).unwrap(); // r1
    mem.write(&mut store, 16, &r2.to_le_bytes()).unwrap(); // r2

    run.call(&mut store, ()).expect("the emitted block runs");

    let mut r0 = [0u8; 8];
    mem.read(&store, 0, &mut r0).unwrap();
    assert_eq!(
        u64::from_le_bytes(r0),
        r1.wrapping_add(r2),
        "the runtime-emitted wasm block computed r0 = r1 + r2 \
         — Rust→wasm→wasmtime→register-file execution works (the JIT substrate)"
    );
}
