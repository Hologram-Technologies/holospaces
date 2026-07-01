//! The chained region-JIT's **browser executor** — the `js_sys::WebAssembly` twin of the native
//! `holospaces::emulator::jit_exec::exec_region_pooled` (which uses `wasmtime`, and so cannot build
//! for wasm32).
//!
//! The region codegen + the SHADOW→TRUST→COMMIT dispatch (`jit_run_region`) live in the portable
//! `holospaces` core; only *executing* a compiled region needs a wasm runtime. Here we instantiate
//! the region wasm through the page's OWN `WebAssembly` engine and run it on the user's CPU — no
//! server, no remote execution. We inject this through the core's executor SEAM
//! (`x64::set_region_executor`), so the core never links a wasm runtime.
//!
//! Same page-pool + lazy-paging + dirty-tracking algorithm as the native executor, marshalling to
//! the SAME memory ABI the codegen emits (`emulator::jit::abi`): regs at `0..128`, `rflags` at
//! `RFLAGS_MEM`, the software-TLB image at `TLB_BASE`, the page pool at `GUEST_BASE`; on a TLB miss
//! the region writes `MISS_MEM=1` + the faulting vaddr at `VADDR_MEM` and exits, we fetch that page
//! into the pool + TLB and retry from the region entry (idempotent). A non-miss exit returns
//! `(regs, rflags, dirty, exit_rip)` — exactly what the differential checks compare.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use holospaces::emulator::jit::abi::{
    EXIT_RIP_MEM, GUEST_BASE, MISS_MEM, RFLAGS_MEM, TLB_BASE, TLB_SIZE, VADDR_MEM,
};
use js_sys::{Object, Reflect, Uint8Array, WebAssembly};
use wasm_bindgen::{JsCast, JsValue};

/// Pages in the executor's guest-RAM page pool (~240 KiB) — must match the native executor and fit
/// the region wasm's declared 4-page (256 KiB) linear memory, so no `Memory.grow` is ever needed.
const POOL_PAGES: usize = 60;
const PAGE: usize = 0x1000;

/// A warm region instance kept resident per κ: compiling + instantiating the region wasm is paid
/// ONCE per κ, then every hot-loop execution reuses the `Instance`/`Memory`/`run` handle — only the
/// per-entry register/TLB/pool marshal is per-call (amortised over the region's many iterations).
struct WarmWeb {
    memory: WebAssembly::Memory,
    run: js_sys::Function,
}

thread_local! {
    static WARM: RefCell<HashMap<[u8; 32], WarmWeb>> = RefCell::new(HashMap::new());
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Install the browser region executor through the core's seam (idempotent; safe to call at the top
/// of every run chunk). It does NOT enable the region JIT — see the warning below.
///
/// **Region JIT is DORMANT by default (NOT armed).** A native measurement on a real Alpine boot
/// (`region_jit_real_alpine_speedup`, cc45) showed the chained region JIT is currently a **net LOSS**
/// on real workloads: ~0.25–0.47× (a *slowdown*), because real regions are short function bodies
/// where the per-commit marshalling cost exceeds the win (the measured 12–24× was a 2M-iteration
/// single loop, where marshalling amortizes). That run ALSO exposed a divergence the
/// SHADOW→TRUST→COMMIT gate missed (a committed region corrupted state → `uname: not found`). Until
/// both are fixed (per-commit cost down + the shadow hole closed), the browser leaves it OFF — the
/// executor is wired so it can be enabled for experiments via `set_region_jit_on(true)`, but never
/// by default. The interpreter remains the correct, faster path for real Alpine today.
pub fn arm() {
    ARMED.with(|a| {
        if !a.get() {
            holospaces::emulator::x64::set_region_executor(exec_region_pooled_web);
            // NB: deliberately NOT calling set_region_jit_on(true) — see the warning above.
            a.set(true);
        }
    });
}

/// Write `src` into the wasm linear memory at byte offset `off`.
fn write_at(view: &Uint8Array, off: u64, src: &[u8]) {
    view.subarray(off as u32, off as u32 + src.len() as u32).copy_from(src);
}
/// Read `dst.len()` bytes from the wasm linear memory at byte offset `off`.
fn read_at(view: &Uint8Array, off: u64, dst: &mut [u8]) {
    view.subarray(off as u32, off as u32 + dst.len() as u32).copy_to(dst);
}
fn read_u64(view: &Uint8Array, off: u64) -> u64 {
    let mut b = [0u8; 8];
    read_at(view, off, &mut b);
    u64::from_le_bytes(b)
}

/// Compile + instantiate the region wasm through the browser's own `WebAssembly` engine (synchronous
/// constructors — these modules are small), returning the warm handle. `None` on any JS-boundary
/// failure (the caller then interprets the region, never wrong).
fn instantiate(wasm: &[u8]) -> Option<WarmWeb> {
    let bytes = Uint8Array::from(wasm);
    let module = WebAssembly::Module::new(bytes.as_ref()).ok()?;
    let imports = Object::new();
    let instance = WebAssembly::Instance::new(&module, &imports).ok()?;
    let exports = instance.exports();
    let memory = Reflect::get(&exports, &JsValue::from_str("mem"))
        .ok()?
        .dyn_into::<WebAssembly::Memory>()
        .ok()?;
    let run = Reflect::get(&exports, &JsValue::from_str("run"))
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    Some(WarmWeb { memory, run })
}

/// The browser region executor — matches `holospaces::emulator::x64::RegionExecFn` so it can be
/// injected through `set_region_executor`. Runs the compiled region over a page pool, lazily filling
/// guest pages on a TLB miss and retrying from the entry, until the region exits without a miss.
/// Returns `(regs, rflags, dirty, exit_rip)` — bit-identical to the native executor (the core's
/// SHADOW→TRUST→COMMIT gate proves it before any region commits). `None` = a real `#PF`, the pool
/// fills, a slot collision, or a JS-boundary error → the caller interprets instead.
pub fn exec_region_pooled_web(
    key: [u8; 32],
    wasm: &[u8],
    entry_regs: [u64; 16],
    entry_rflags: u64,
    fetch_page: &dyn Fn(u64) -> Option<(usize, Vec<u8>)>,
) -> Option<([u64; 16], u64, Vec<(usize, Vec<u8>)>, u64)> {
    WARM.with(|wm| {
        let mut warms = wm.borrow_mut();
        if !warms.contains_key(&key) {
            let warm = instantiate(wasm)?;
            warms.insert(key, warm);
        }
        let warm = warms.get(&key).unwrap();
        let memory = &warm.memory;
        let run = &warm.run;

        // A FRESH TLB and a lazily-grown pool each call: the warm memory's stale pages are never
        // referenced (no TLB entry), and pages are re-fetched on miss — so reuse is correctness-safe.
        let mut tlb = vec![0u8; TLB_SIZE as usize * 16];
        let mut pool: Vec<u8> = Vec::new();
        let mut mapped: Vec<(u64, usize)> = Vec::new(); // slot -> (vpage, pa_frame)

        // Flatten the entry register file once (regs at 0..128, little-endian).
        let mut regbytes = [0u8; 128];
        for (i, v) in entry_regs.iter().enumerate() {
            regbytes[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }

        for _ in 0..=POOL_PAGES {
            // Marshal IN: regs, rflags, TLB, pool. Re-view the buffer each iteration (defensive —
            // the region never grows memory, but the ArrayBuffer handle is cheap to refresh).
            let view = Uint8Array::new(&memory.buffer());
            write_at(&view, 0, &regbytes);
            write_at(&view, RFLAGS_MEM, &entry_rflags.to_le_bytes());
            write_at(&view, TLB_BASE, &tlb);
            if !pool.is_empty() {
                write_at(&view, GUEST_BASE, &pool);
            }

            run.call0(&JsValue::NULL).ok()?;

            // Marshal OUT.
            let view = Uint8Array::new(&memory.buffer());
            let exit_rip = read_u64(&view, EXIT_RIP_MEM);
            let mut rb = [0u8; 128];
            read_at(&view, 0, &mut rb);
            let mut regs = [0u64; 16];
            for (i, r) in regs.iter_mut().enumerate() {
                *r = u64::from_le_bytes(rb[i * 8..i * 8 + 8].try_into().unwrap());
            }
            let rflags = read_u64(&view, RFLAGS_MEM);
            let miss = read_u64(&view, MISS_MEM);
            if !pool.is_empty() {
                read_at(&view, GUEST_BASE, &mut pool);
            }

            if miss == 0 {
                // A real / yield exit — return the region's effects + where to resume.
                let dirty = mapped
                    .iter()
                    .enumerate()
                    .map(|(slot, &(_vp, pa))| (pa, pool[slot * PAGE..slot * PAGE + PAGE].to_vec()))
                    .collect();
                return Some((regs, rflags, dirty, exit_rip));
            }

            // TLB miss — fetch the faulting page into the pool + TLB and retry from entry.
            let vaddr = read_u64(&view, VADDR_MEM);
            let vpage = vaddr >> 12;
            if mapped.iter().any(|&(vp, _)| vp == vpage) || mapped.len() >= POOL_PAGES {
                return None; // already mapped yet still missed / pool full
            }
            let (pa_frame, bytes) = fetch_page(vaddr)?; // None → real #PF → interpret
            if bytes.len() != PAGE {
                return None;
            }
            let slot = mapped.len();
            pool.extend_from_slice(&bytes);
            mapped.push((vpage, pa_frame));
            let s = (vpage & (TLB_SIZE - 1)) as usize;
            tlb[s * 16..s * 16 + 8].copy_from_slice(&vpage.to_le_bytes());
            tlb[s * 16 + 8..s * 16 + 16].copy_from_slice(&((slot * PAGE) as u64).to_le_bytes());
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use holospaces::emulator::jit::compile_region_from_code;
    use wasm_bindgen_test::wasm_bindgen_test;

    /// The browser executor runs a register-only loop region in node's OWN wasm engine and returns
    /// bit-exact registers — proving instantiate + marshal-in + `run` + marshal-out over the live
    /// `WebAssembly.Memory`. `add rax,rcx ; sub rbx,rdx ; jnz top` loops until rbx hits 0.
    #[wasm_bindgen_test]
    fn browser_executor_runs_a_register_loop_bit_exact() {
        let code = [0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x75, 0xf8];
        let wasm = compile_region_from_code(&code, 0, 1 << 20);
        let mut regs = [0u64; 16];
        regs[1] = 5; // rcx = 5  (added to rax each pass)
        regs[3] = 8; // rbx = 8  (loop count)
        regs[2] = 1; // rdx = 1  (subtracted from rbx each pass)
        let fetch = |_va: u64| None; // register-only region → never pages
        let (got, _rflags, dirty, _exit) = exec_region_pooled_web([7u8; 32], &wasm, regs, 0, &fetch)
            .expect("the register loop region runs in the browser wasm engine");
        assert_eq!(got[3], 0, "rbx counted down to 0 (8 subtractions)");
        assert_eq!(got[0], 40, "rax accumulated rcx eight times (8 * 5)");
        assert!(dirty.is_empty(), "a register-only region touches no guest pages");
    }

    /// The browser executor's lazy-paging path: a load region misses the empty TLB, the executor
    /// fetches the faulting page through the page-fetch closure, retries from entry, and the load
    /// completes — proving the pool marshalling (`GUEST_BASE`) and miss/retry loop in a real engine.
    #[wasm_bindgen_test]
    fn browser_executor_pages_in_a_load_on_miss() {
        let code = [0x48, 0x8b, 0x03]; // mov rax, [rbx]
        let wasm = compile_region_from_code(&code, 0, 1 << 20);
        let mut regs = [0u64; 16];
        regs[3] = 0x2000; // rbx → guest virtual page 2, offset 0
        let val: u64 = 0xDEAD_BEEF_0BAD_F00D;
        let fetch = move |va: u64| -> Option<(usize, Vec<u8>)> {
            let frame = (va as usize) & !0xfff; // identity translation for the test
            let mut page = vec![0u8; 0x1000];
            if frame == 0x2000 {
                page[0..8].copy_from_slice(&val.to_le_bytes());
            }
            Some((frame, page))
        };
        let (got, _rflags, dirty, _exit) = exec_region_pooled_web([9u8; 32], &wasm, regs, 0, &fetch)
            .expect("the load region pages in on miss and completes");
        assert_eq!(got[0], val, "rax loaded from the lazily paged-in guest page");
        assert_eq!(dirty.len(), 1, "exactly the one touched page is returned");
        assert_eq!(dirty[0].0, 0x2000, "the dirty page's physical frame is the fetched one");
    }

    /// The browser executor runs an IMMEDIATE-driven counted loop (the G6a decoder breadth) in node's
    /// wasm engine, bit-exact: `add rax,1 ; cmp rax,100 ; jne top` loops until rax == 100.
    #[wasm_bindgen_test]
    fn browser_executor_runs_an_immediate_counted_loop() {
        let code = [0x48, 0x83, 0xc0, 0x01, 0x48, 0x83, 0xf8, 0x64, 0x75, 0xf6];
        let wasm = compile_region_from_code(&code, 0, 1 << 20);
        let regs = [0u64; 16];
        let fetch = |_va: u64| None; // register/flag only — no paging
        let (got, _rflags, dirty, _exit) = exec_region_pooled_web([11u8; 32], &wasm, regs, 0, &fetch)
            .expect("the immediate counted loop runs in the browser wasm engine");
        assert_eq!(got[0], 100, "rax counted up to the immediate bound (cmp rax,100)");
        assert!(dirty.is_empty(), "a register/flag-only region touches no guest pages");
    }

    /// The browser executor runs PUSH/POP (the G6b stack-memory ops) with a lazily paged stack:
    /// `push rax ; pop rbx ; ret` round-trips rax→rbx through the stack and returns the dirty page.
    #[wasm_bindgen_test]
    fn browser_executor_runs_push_pop_with_a_paged_stack() {
        let code = [0x50, 0x5b, 0xc3]; // push rax ; pop rbx ; ret
        let wasm = compile_region_from_code(&code, 0, 1 << 20);
        let mut regs = [0u64; 16];
        regs[0] = 0xCAFE_F00D_1234_5678; // rax
        regs[4] = 0x2008; // rsp → push lands at [0x2000] (guest page 2)
        let fetch = |va: u64| -> Option<(usize, Vec<u8>)> {
            let frame = (va as usize) & !0xfff;
            Some((frame, vec![0u8; 0x1000])) // identity-mapped zeroed stack page
        };
        let (got, _rflags, dirty, _exit) = exec_region_pooled_web([13u8; 32], &wasm, regs, 0, &fetch)
            .expect("push/pop pages the stack in and completes in the browser wasm engine");
        assert_eq!(got[3], 0xCAFE_F00D_1234_5678, "pop rbx recovered the pushed rax");
        assert_eq!(got[4], 0x2008, "rsp restored after balanced push/pop");
        assert_eq!(dirty.len(), 1, "the one stack page is returned dirty");
        assert_eq!(dirty[0].0, 0x2000, "the dirty stack page's frame");
    }
}
