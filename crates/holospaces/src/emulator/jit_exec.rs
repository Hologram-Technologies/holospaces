//! The native block-JIT executor (the `jit` feature): run a `compile_tlb_flags` block over
//! the live `Cpu` state via `wasmtime`.
//!
//! The JIT *codegen* (`super::jit`) is pure `no_std` byte-emission; only *executing* the
//! emitted wasm needs a runtime. `wasmtime` is a bare-function runner here (NOT the `.holo`
//! container engine). The browser peer runs the same blocks through `js_sys::WebAssembly`.
//!
//! Memory marshalling mirrors the proven `jit_executor_matches_step_through_paging` blueprint:
//! regs at `0..128`, `rflags` at `RFLAGS_MEM`, the software TLB image at `TLB_BASE`, guest RAM
//! at `GUEST_BASE` indexed by **physical address** (`host = GUEST_BASE + PA`). The caller fills
//! the TLB from the interpreter's own translation (`translate_acc` / `self.tlb` frames).
#![allow(dead_code)] // exec_block is exercised by the feature-gated test; run() wiring is Step 2

use wasmtime::{Engine, Instance, Module, Store};

use super::jit::{GUEST_BASE, RFLAGS_MEM, TLB_BASE};

/// Execute one compiled block over the given architectural state. `regs`, `rflags`, and `ram`
/// are updated in place; `tlb` is the software-TLB image (read-only this run). Returns the
/// bail index — the instruction the block stopped at on a TLB miss, or the block length if it
/// ran to completion (so the caller resumes the interpreter at `block_start + offsets[bail]`).
///
/// NB: this instantiates the module per call; the steady-state path should cache the compiled
/// `Module` (keyed by the block's κ, alongside the wasm in `BlockCache`) — a perf refinement,
/// not a correctness one.
pub(crate) fn exec_block(
    wasm: &[u8],
    regs: &mut [u64; 16],
    rflags: &mut u64,
    ram: &mut [u8],
    tlb: &[u8],
) -> i32 {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).expect("emitted wasm is valid");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let mem = instance.get_memory(&mut store, "mem").unwrap();

    // Grow the linear memory so the guest-RAM region [GUEST_BASE, +ram.len()) fits.
    let need_pages = (GUEST_BASE as usize + ram.len()).div_ceil(0x10000);
    let have_pages = mem.size(&store) as usize;
    if need_pages > have_pages {
        mem.grow(&mut store, (need_pages - have_pages) as u64).expect("grow guest RAM region");
    }

    let run = instance.get_typed_func::<(), i32>(&mut store, "run").unwrap();
    for (i, v) in regs.iter().enumerate() {
        mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
    }
    mem.write(&mut store, RFLAGS_MEM as usize, &(*rflags).to_le_bytes()).unwrap();
    mem.write(&mut store, TLB_BASE as usize, tlb).unwrap();
    mem.write(&mut store, GUEST_BASE as usize, ram).unwrap();

    let bail = run.call(&mut store, ()).expect("run");

    for (i, r) in regs.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        mem.read(&store, i * 8, &mut b).unwrap();
        *r = u64::from_le_bytes(b);
    }
    let mut fb = [0u8; 8];
    mem.read(&store, RFLAGS_MEM as usize, &mut fb).unwrap();
    *rflags = u64::from_le_bytes(fb);
    mem.read(&store, GUEST_BASE as usize, ram).unwrap();
    bail
}
