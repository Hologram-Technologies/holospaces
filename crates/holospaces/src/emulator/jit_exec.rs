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

use super::jit::{eff_addr, op_mem_addr, Op, GUEST_BASE, RFLAGS_MEM, TLB_BASE, TLB_SIZE};

/// Pages in the executor's guest-RAM page pool (~256 KiB). A block touches only a handful;
/// the pool holds its working set, indexed by `host_off` (a slot offset, NOT a physical
/// address — so the wasm memory stays tiny regardless of guest RAM size).
const POOL_PAGES: usize = 60;
const PAGE: usize = 0x1000;

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

/// Execute a compiled block over a small **page pool**, lazily filling guest pages on a
/// TLB-miss bail and restarting until the block completes — the scalable boot executor
/// (the wasm memory is constant-size regardless of guest RAM). On a bail at op `k`, the
/// faulting vaddr is recomputed from `ops[k]`'s address mode + the post-`k` registers,
/// `translate`d to a physical page, copied into a free pool slot, and the run retries from
/// the block entry (idempotent: the block is deterministic from `entry_regs`).
///
/// Runs "dry": `fetch_page(vaddr) -> Some((pa_frame, 4 KiB bytes))` supplies a guest page on
/// a miss (`None` = a real `#PF`), and the block's effects are RETURNED, not committed —
/// `Some((regs, rflags, dirty))` where `dirty` is each mapped page as `(pa_frame, bytes)`.
/// The caller commits (writes `dirty` into guest RAM) only for a *trusted* block; the
/// differential compares `dirty` to what `step()` produced without committing. `None` when
/// the block cannot complete — a real fault, the pool fills, or a slot collision — in which
/// case the caller interprets the block. (One callback, not `&ram`+`translate`, so the caller
/// borrows its `Cpu` exactly once.)
pub(crate) fn exec_block_pooled(
    wasm: &[u8],
    ops: &[Op],
    entry_regs: [u64; 16],
    entry_rflags: u64,
    fetch_page: impl Fn(u64) -> Option<(usize, Vec<u8>)>,
) -> Option<([u64; 16], u64, Vec<(usize, Vec<u8>)>)> {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).ok()?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).ok()?;
    let mem = instance.get_memory(&mut store, "mem").unwrap();
    let need = (GUEST_BASE as usize + POOL_PAGES * PAGE).div_ceil(0x10000);
    let have = mem.size(&store) as usize;
    if need > have {
        mem.grow(&mut store, (need - have) as u64).ok()?;
    }
    let run = instance.get_typed_func::<(), i32>(&mut store, "run").ok()?;

    let mut tlb = vec![0u8; TLB_SIZE as usize * 16];
    let mut pool = vec![0u8; POOL_PAGES * PAGE];
    let mut mapped: Vec<(u64, usize)> = Vec::new(); // slot -> (vpage, pa_frame)

    for _ in 0..=POOL_PAGES {
        for (i, v) in entry_regs.iter().enumerate() {
            mem.write(&mut store, i * 8, &v.to_le_bytes()).ok()?;
        }
        mem.write(&mut store, RFLAGS_MEM as usize, &entry_rflags.to_le_bytes()).ok()?;
        mem.write(&mut store, TLB_BASE as usize, &tlb).ok()?;
        mem.write(&mut store, GUEST_BASE as usize, &pool).ok()?;
        let bail = run.call(&mut store, ()).ok()?;
        let mut regs = [0u64; 16];
        for (i, r) in regs.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            mem.read(&store, i * 8, &mut b).ok()?;
            *r = u64::from_le_bytes(b);
        }
        let mut fb = [0u8; 8];
        mem.read(&store, RFLAGS_MEM as usize, &mut fb).ok()?;
        let rflags = u64::from_le_bytes(fb);
        mem.read(&store, GUEST_BASE as usize, &mut pool).ok()?;

        if (bail as usize) >= ops.len() {
            // completed — return every mapped page as a dirty candidate (clean pages equal RAM)
            let dirty = mapped
                .iter()
                .enumerate()
                .map(|(slot, &(_vpage, pa_frame))| {
                    (pa_frame, pool[slot * PAGE..slot * PAGE + PAGE].to_vec())
                })
                .collect();
            return Some((regs, rflags, dirty));
        }

        // bail at a memory op — recompute its vaddr, fetch the page, retry
        let (base, idx, scale, disp) = op_mem_addr(&ops[bail as usize])?;
        let vaddr = eff_addr(&regs, base, idx, scale, disp) as u64;
        let vpage = vaddr >> 12;
        if mapped.iter().any(|&(vp, _)| vp == vpage) || mapped.len() >= POOL_PAGES {
            return None; // already mapped yet still missed (slot collision) / pool full
        }
        let (pa_frame, bytes) = fetch_page(vaddr)?; // None → real #PF → interpret
        if bytes.len() != PAGE {
            return None;
        }
        let slot = mapped.len();
        pool[slot * PAGE..slot * PAGE + PAGE].copy_from_slice(&bytes);
        mapped.push((vpage, pa_frame));
        let s = (vpage & (TLB_SIZE - 1)) as usize;
        tlb[s * 16..s * 16 + 8].copy_from_slice(&vpage.to_le_bytes());
        tlb[s * 16 + 8..s * 16 + 16].copy_from_slice(&((slot * PAGE) as u64).to_le_bytes());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::jit::{compile_tlb_flags, NO_REG};

    #[test]
    fn pooled_executor_fills_pages_on_bail_and_completes() {
        // rax = [rbx] ; [rbp] = rax — source in page 2, dest in page 5 (two TLB misses).
        let block = [
            Op::Load { d: 0, base: 3, idx: NO_REG, scale: 0, disp: 0 },
            Op::Store { base: 5, idx: NO_REG, scale: 0, disp: 0, s: 0 },
        ];
        let wasm = compile_tlb_flags(&block);
        let mut ram = vec![0u8; 0x10000];
        let v: u64 = 0xCAFE_F00D_1234_5678;
        ram[0x2000..0x2008].copy_from_slice(&v.to_le_bytes());
        let mut regs = [0u64; 16];
        regs[3] = 0x2000; // rbx → page 2
        regs[5] = 0x5000; // rbp → page 5
        // start with an empty pool: the block bails twice, the executor fetches both pages.
        let fetch = |va: u64| {
            let f = (va as usize) & !0xfff; // identity translation for the test
            (f + 0x1000 <= ram.len()).then(|| (f, ram[f..f + 0x1000].to_vec()))
        };
        let out = exec_block_pooled(&wasm, &block, regs, 0x2, fetch);
        let (out_regs, _rf, dirty) = out.expect("the block completes after the pool fills both pages");
        assert_eq!(out_regs[0], v, "rax loaded from the lazily-filled source page");
        // dry mode: commit the returned dirty pages, then the store is visible in guest RAM.
        for (pa, bytes) in &dirty {
            ram[*pa..*pa + bytes.len()].copy_from_slice(bytes);
        }
        assert_eq!(
            u64::from_le_bytes(ram[0x5000..0x5008].try_into().unwrap()),
            v,
            "the store landed in the pool and committed to guest RAM"
        );
    }
}
