//! **CC-48 JIT execution driver** — the `std`-only fast-execution path that runs
//! the [`x64_jit`](super::x64_jit) translator's **regions** (traces) on a real Wasm
//! engine (`wasmtime`) from inside the [`x64`](super::x64) interpreter's run loop.
//!
//! The interpreter ([`Cpu::run`](super::x64::Cpu::run)) decodes and dispatches
//! every guest instruction on every execution; for a hot run of register/memory
//! integer ops — including a whole loop — that per-instruction dispatch dominates.
//! This driver removes it: it translates a hot **region** *once*
//! ([`super::x64_jit::translate_region_at`]) — several basic blocks reachable by
//! direct branches, with an internal `br_table` dispatch loop — into a Wasm function
//! the host runs natively, caches the compiled module by the region's *physical*
//! entry address, and on every subsequent visit runs many guest instructions in a
//! single Wasm call instead of N interpreter dispatches. Anything the translator
//! does not cover (a syscall, an MMIO touch, a page fault, an unsupported opcode)
//! falls back to the interpreter for exactly one instruction, so the JIT is a pure
//! accelerator over the qemu-validated core — never a second, divergent core.
//!
//! ## Timer/interrupt ordering (load-bearing)
//!
//! `run` advances the platform timers (`sys_tick`) and then delivers a pending
//! interrupt (`take_pending_interrupt`) *before* each instruction's `step`. The
//! driver mirrors this exactly for an interpreted instruction — `jit_sys_tick_n(1)`
//! then `take_pending` then `step` — because an interpreted `RDTSC`/`RDRAND` reads
//! the TSC, and the kernel's `RDRAND` (with `random.trust_cpu`) mixes the TSC into
//! its output; ticking *after* the instruction (the natural place for a batched
//! region) would feed it a TSC one step behind the interpreter and every
//! random/canary/ASLR value would diverge, crashing userspace. A region retires
//! many instructions per call and contains no `RDTSC`/`RDRAND` (those stop a block),
//! so it is ticked by its retired count *after* the call — observationally
//! identical. A region's budget is also capped at the instructions-until-next-timer
//! ([`Cpu::jit_steps_to_next_timer`]) so a long region exits at the timer deadline
//! and the IRQ is delivered promptly, matching the interpreter's cadence.
//!
//! ## Correctness model
//!
//! Every architectural effect still flows through the interpreter's authority:
//!
//!   * **Memory** — the emitted block's `env.load`/`env.store` imports call
//!     [`Cpu::jit_mem`](super::x64::Cpu::jit_mem), which translates and accesses
//!     RAM *identically* to the interpreter's `rd`/`wr` (paging, `CPL`,
//!     `dcache_touch`). A page fault or MMIO access makes `jit_mem` return `None`;
//!     the host import then **traps**, aborting the block. Because the block stamps
//!     the current guest `rip` before every memory access and writes each
//!     instruction's register/flag result as it goes, the shared register file at
//!     the trap holds the state *as of the last completed instruction*; the driver
//!     flushes that into the `Cpu`, sets `rip` to the aborting instruction, clears
//!     any latched fault, and re-interprets *that one instruction* through
//!     [`Cpu::step`](super::x64::Cpu::step) — re-deriving the #PF / running the MMIO
//!     op with full side effects, exactly as `step` would. (RAM writes a block
//!     commits before it aborts are to already-mapped pages and are not replayed,
//!     so a multi-memory-op block is safely restartable.)
//!   * **Time / interrupts** — the driver mirrors `run`'s per-iteration
//!     `sys_tick` / `halted` / `take_pending_interrupt` bookkeeping so the guest's
//!     timer cadence and interrupt delivery match the interpreter.
//!   * **Self-modifying code** — the kernel patches its own `.text`
//!     (alternatives / `text_poke`). The interpreter's `wr` flags any store onto a
//!     page with a cached block ([`Cpu`]'s `jit_code_pages` / `jit_dirty`); the
//!     driver then drops the stale entries so the next visit re-translates.
//!
//! ## Performance shape
//!
//! Several choices trim per-block overhead:
//!
//!   * **JIT tiering** — a block is interpreted until it has run [`HOT_THRESHOLD`]
//!     times, then compiled. Cranelift's per-module compile cost (hundreds of µs)
//!     would never amortise on the *cold*, one-shot blocks that make up most of a
//!     boot, so only the genuinely hot blocks (loops, `memcpy`/`memset`, the
//!     scheduler/fault paths) graduate to native code.
//!   * **A shared register file** — every block instance imports the *same* Wasm
//!     [`Memory`] for the 16 GPRs + `RFLAGS`, and the live state is kept there
//!     across a run of consecutive translated blocks (synced back to the `Cpu` only
//!     at a JIT↔interpreter boundary). This also keeps the store's per-instance
//!     footprint tiny so the cache can hold thousands of hot blocks.
//!   * **No swap for register-only blocks** (`touches_mem == false` → no host
//!     import → no `Cpu` lent into the store), an **inline last-block cache** for a
//!     hot loop re-entering the same block, a **fast `u64` hasher** for the cache,
//!     and **code bytes fetched only on a translation miss**.
//!
//! `run_jit` lives on [`Cpu`](super::x64::Cpu) (it needs the core's private state);
//! this module owns [`X64JitExec`] — the engine, the shared register file, the block
//! cache, and the host imports.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use wasmtime::{
    Caller, Config, Engine, Func, Instance, Memory, MemoryType, Module, Store, TypedFunc,
};

use super::x64::{Cpu, Halt};
use super::x64_jit::translate_region_at;

/// A minimal, fast hasher for the block caches, whose keys are physical addresses
/// (`u64`). The cache is looked up on *every* block dispatch (millions of times per
/// boot), so the default `SipHash` is pure overhead — a single multiplicative
/// finaliser (the SplitMix64/`fxhash` mixing constant) gives a well-distributed
/// hash for address keys at a fraction of the cost. Only `write_u64` is ever used.
#[derive(Default)]
struct U64Hasher(u64);

impl Hasher for U64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // Not used for u64 keys; provided for completeness.
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// A `HashMap` keyed by physical address with the fast [`U64Hasher`].
type PhysMap<V> = HashMap<u64, V, BuildHasherDefault<U64Hasher>>;

/// Byte offset of the `RFLAGS` slot in a block's register file (after the 16
/// little-endian `u64` GPRs).
const RFLAGS_OFF: usize = 128;
/// Byte offset of the fault-restart RIP slot a block stamps before each memory
/// access (must match `x64_jit`'s `FAULT_RIP_OFF`).
const FAULT_RIP_OFF: usize = 136;
/// Byte offset of the retired-instruction slot a region stamps before each memory
/// access (must match `x64_jit`'s `RETIRED_OFF`) — how many instructions the region
/// had retired before the aborting one, so a trap credits the timer correctly.
const RETIRED_OFF: usize = 144;

/// The instruction budget passed to a region `run` call — the cadence at which the
/// region returns to the driver so timer/interrupt bookkeeping is pumped. Large
/// enough that a hot loop runs thousands of guest instructions in one Wasm call
/// (the whole point), small enough that the periodic timer and interrupt delivery
/// stay fine-grained (the driver advances the timer by the real retired count via
/// [`Cpu::jit_sys_tick_n`] and pumps a pending interrupt each return).
const REGION_BUDGET: u64 = 4096;

/// How many times an entry must be interpreted before the driver compiles it (the
/// JIT tiering threshold). High enough that a one-shot or rarely-taken block never
/// pays Cranelift's per-module compile cost, low enough that a hot loop graduates
/// to native execution almost immediately.
const HOT_THRESHOLD: u32 = 64;

/// The hotness-counter value parked on an entry the translator could not handle
/// (its first instruction is unsupported), so the driver stops re-counting it and
/// just interprets.
const HOTNESS_REJECTED: u32 = u32::MAX;

/// The maximum number of compiled blocks held before the store is rebuilt (a
/// `wasmtime` store caps instances at 10,000 and never frees them until dropped).
/// Kept below that cap with headroom.
const CACHE_CAP: usize = 8192;

/// A compiled, instantiated translated **region** (trace), cached by its physical
/// entry address. The register file is the executor's shared [`Memory`]; a region
/// holds only its `run` entry point and bookkeeping. Its `run` runs many guest
/// instructions (a hot loop entirely) in one Wasm call, taking an instruction
/// budget and returning `(exit_rip, insns_retired)`.
struct CachedRegion {
    /// The instantiated module. Held to keep the instance (and thus its imported
    /// `run` function) alive in the store for the cache entry's lifetime.
    #[allow(dead_code)]
    instance: Instance,
    run: TypedFunc<(i64, i64), (i64, i64)>,
    /// Whether the region emits any guest-memory access — if not, it never calls a
    /// host import, so the driver can run it without lending the `Cpu`.
    touches_mem: bool,
    /// The physical page (`phys >> 12`) the region's code lives on — the SMC
    /// invalidation key (a region never crosses a page). A region is dropped when its
    /// page leaves the `Cpu`'s `jit_code_pages` set (a store overwrote source bytes).
    page: u64,
}

/// The outcome of attempting a translated region at the current `rip`.
enum BlockRun {
    /// No usable translation (the entry instruction is unsupported) — the driver
    /// interprets one instruction.
    Interpret,
    /// The region ran to a region exit or its budget: continue at `next_rip`, having
    /// retired `insns` guest instructions (across the blocks it ran in one call).
    Done { next_rip: u64, insns: u64 },
    /// A host `load`/`store` import trapped (a page fault or MMIO access). The
    /// architectural register/flag state of the instruction *before* the aborting
    /// one has been synced into the `Cpu`; `fault_rip` is the aborting instruction's
    /// guest rip, and `retired` is the number of instructions the region had fully
    /// retired before it (to credit the timer). The driver resumes the interpreter
    /// there (clearing any latched fault) so it re-runs that one instruction.
    Trapped { fault_rip: u64, retired: u64 },
}

/// The CC-48 JIT engine + block cache. Construct one per boot and drive it with
/// [`Cpu::run_jit`](super::x64::Cpu::run_jit).
///
/// For a memory-touching block the driving core is swapped into the Wasm store for
/// the duration of the call (`core::mem::swap`, a cheap struct move: the core's
/// RAM/TLB live behind `Vec`/`Box` pointers), so the block's `env.load`/`env.store`
/// imports reach the interpreter's [`jit_mem`](Cpu::jit_mem) primitive with **no
/// `unsafe`** — the store's `Caller::data_mut()` *is* the real core. A tiny
/// placeholder core occupies the slot otherwise.
pub struct X64JitExec {
    engine: Engine,
    store: Store<Cpu>,
    /// The shared register file (16 GPRs + `RFLAGS` + the fault-restart rip) that
    /// *every* block instance imports as `env.mem`. Sharing one memory (rather than
    /// one per block) keeps the store's per-instance footprint tiny, so the cache
    /// can hold thousands of hot blocks. The driver copies the architectural
    /// registers into / out of it around each block call (a block run is serial, so
    /// no two blocks ever use it at once).
    regfile: Memory,
    /// Compiled regions keyed by physical entry address.
    cache: PhysMap<CachedRegion>,
    /// Per-entry execution counter for blocks not yet compiled — the JIT *tiering*
    /// gate. Compiling a block on `wasmtime`/Cranelift costs hundreds of
    /// microseconds, so a block that runs only a handful of times never amortises
    /// it. The driver interprets a cold entry (bumping this counter) and only
    /// compiles once it has been seen [`HOT_THRESHOLD`] times — focusing the
    /// (expensive) translation budget on the few genuinely hot blocks that retire
    /// the bulk of the guest's instructions (hot loops, `memcpy`/`memset`, the
    /// page-fault and scheduler paths). Entries graduate out of this map into
    /// `cache`; entries the translator rejects are parked at a sentinel so they are
    /// not re-counted.
    hotness: PhysMap<u32>,
    /// The tiering threshold: how many times an entry is interpreted before it is
    /// compiled (defaults to [`HOT_THRESHOLD`]; lowerable via
    /// [`X64JitExec::with_hot_threshold`] for tests that want immediate compilation).
    hot_threshold: u32,
    /// Whether the *live* register state currently lives in [`Self::regfile`]
    /// (`true`, after a JIT block left it there) or in the architectural [`Cpu`]
    /// (`false`). Lazy synchronisation: consecutive JIT blocks keep the registers in
    /// Wasm and never copy them back to the `Cpu` between blocks; the driver flushes
    /// them out only at a boundary that reads the `Cpu` registers (an interpreted
    /// instruction, or an interrupt delivery). A hot all-JIT stretch thus pays *no*
    /// per-block register copy-out.
    regs_in_wasm: bool,
    /// Instances created in the current store since the last rebuild. A `wasmtime`
    /// store never frees an instance until it is dropped — so an SMC flush that
    /// drops a cache entry leaves its instance behind. This counts *every* compile
    /// (re-compiles included) to bound the store's true instance count, triggering a
    /// rebuild before the engine's hard cap. (`cache.len()` undercounts because of
    /// flushed-but-still-resident instances.)
    instances_live: usize,
    /// The physical address of the last block run — an inline cache so a hot loop
    /// re-entering the same block skips the hash-map lookup.
    last_phys: u64,
    /// Guest instructions retired through translated regions (coverage probe).
    pub jit_insns: u64,
    /// Guest instructions retired through the interpreter fallback (coverage probe).
    pub interp_insns: u64,
    /// Translated regions executed (cache hits + first runs) — the number of Wasm
    /// `run` calls the driver made.
    pub blocks_run: u64,
    /// Distinct regions compiled (translation-cache misses).
    pub blocks_translated: u64,
    /// Regions that aborted via a host trap (a guest page fault / MMIO touch).
    pub blocks_trapped: u64,
    /// Sum of the static basic-block count over every compiled region (so the boot
    /// can report the average region size = `region_blocks_total / blocks_translated`).
    pub region_blocks_total: u64,
}

impl Default for X64JitExec {
    fn default() -> Self {
        Self::new()
    }
}

impl X64JitExec {
    /// A fresh executor with an empty block cache. The store starts with a tiny
    /// placeholder core (zero-RAM) that the driver swaps the real core in and out of
    /// around a memory-touching block call.
    #[must_use]
    pub fn new() -> Self {
        // The driver makes many short calls and aborts a block on every guest page
        // fault / MMIO touch via a host trap. Drop the diagnostic backtrace + native
        // unwind tables (the trap still unwinds correctly; the driver never inspects
        // the backtrace) to shave per-trap cost on the demand-paging abort path.
        let mut config = Config::new();
        config.wasm_backtrace_max_frames(None);
        config.native_unwind_info(false);
        let engine = Engine::new(&config).expect("wasmtime engine config");
        let mut store = Store::new(&engine, Cpu::jit_placeholder());
        let regfile = Memory::new(&mut store, MemoryType::new(1, None)).expect("regfile memory");
        X64JitExec {
            engine,
            store,
            regfile,
            cache: PhysMap::default(),
            hotness: PhysMap::default(),
            hot_threshold: HOT_THRESHOLD,
            regs_in_wasm: false,
            instances_live: 0,
            last_phys: u64::MAX,
            jit_insns: 0,
            interp_insns: 0,
            blocks_run: 0,
            blocks_translated: 0,
            blocks_trapped: 0,
            region_blocks_total: 0,
        }
    }

    /// Rebuild the Wasm store from scratch — a fresh store, a fresh shared register
    /// file, and an empty cache/hotness. Called when the cache reaches
    /// [`CACHE_CAP`]: a `wasmtime` store caps the number of instances it will hold
    /// (and they are not freed until the store is dropped), so a boot that compiles
    /// more distinct hot blocks than that cap over its lifetime would otherwise hit
    /// the limit and stop JITing. Recreating the store frees every instance at once;
    /// the (bounded) live hot set simply re-tiers and re-compiles. Rare, so its cost
    /// is amortised away.
    fn rebuild_store(&mut self) {
        let mut store = Store::new(&self.engine, Cpu::jit_placeholder());
        let regfile = Memory::new(&mut store, MemoryType::new(1, None)).expect("regfile memory");
        self.store = store;
        self.regfile = regfile;
        self.cache.clear();
        self.hotness.clear();
        self.instances_live = 0;
        self.regs_in_wasm = false;
        self.last_phys = u64::MAX;
    }

    /// Set the JIT tiering threshold (how many interpreted runs before an entry is
    /// compiled). A threshold of `1` compiles every block on first sight — useful
    /// for tests that want to exercise the native path deterministically. Returns
    /// `self` for builder-style construction.
    #[must_use]
    pub fn with_hot_threshold(mut self, threshold: u32) -> Self {
        self.hot_threshold = threshold.max(1);
        self
    }

    /// Make the architectural [`Cpu`] registers authoritative: if the live state is
    /// in the Wasm register file, copy it back into `cpu` and clear the flag. A
    /// no-op when the registers are already in `cpu`. Called before the interpreter
    /// runs, before an interrupt is delivered, and before a store rebuild — anything
    /// that reads `cpu` registers or discards the register file.
    fn sync_from_wasm(&mut self, cpu: &mut Cpu) {
        if !self.regs_in_wasm {
            return;
        }
        let data = self.regfile.data(&self.store);
        for i in 0..16 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[i * 8..i * 8 + 8]);
            cpu.set_reg(i, u64::from_le_bytes(b));
        }
        let mut fb = [0u8; 8];
        fb.copy_from_slice(&data[RFLAGS_OFF..RFLAGS_OFF + 8]);
        cpu.set_rflags(u64::from_le_bytes(fb));
        self.regs_in_wasm = false;
    }

    /// Make the Wasm register file authoritative: if the live state is in `cpu`,
    /// copy it in and set the flag. A no-op when the registers are already in Wasm
    /// (a prior JIT block left them there). Called before running a translated block.
    fn sync_to_wasm(&mut self, cpu: &Cpu) {
        if self.regs_in_wasm {
            return;
        }
        let data = self.regfile.data_mut(&mut self.store);
        for i in 0..16 {
            data[i * 8..i * 8 + 8].copy_from_slice(&cpu.reg(i).to_le_bytes());
        }
        data[RFLAGS_OFF..RFLAGS_OFF + 8].copy_from_slice(&cpu.rflags().to_le_bytes());
        self.regs_in_wasm = true;
    }

    /// Drop every cached block whose source page is no longer marked as a live JIT
    /// code page on `cpu` — the self-modifying-code flush. Called by the driver when
    /// the interpreter has flagged a write onto a cached code page.
    fn flush_invalidated(&mut self, cpu: &Cpu) {
        let live = cpu.jit_code_pages_ref();
        self.cache.retain(|_, b| live.contains(&b.page));
        // Also drop hotness counters on the invalidated pages so a re-written entry
        // re-tiers from cold (its old bytes' count is meaningless), and clear the
        // rejection sentinel so a patched entry can be re-evaluated.
        self.hotness.retain(|&phys, _| live.contains(&(phys >> 12)));
        self.last_phys = u64::MAX; // invalidate the inline cache
    }

    /// Compile and instantiate a translated **region** for `code` (entry
    /// `entry_rip`), returning whether a region was cached. The instance imports the
    /// executor's shared register file as `env.mem`; the `env.load`/`env.store`
    /// imports trap on a `None` from [`Cpu::jit_mem`] (a page fault / MMIO), aborting
    /// the region (resumed at the faulting instruction by the driver).
    fn translate_and_cache(&mut self, phys: u64, code: &[u8], entry_rip: u64) -> bool {
        let Some(tr) = translate_region_at(code, entry_rip) else {
            return false;
        };
        let Ok(module) = Module::new(&self.engine, &tr.wasm) else {
            return false;
        };
        let regfile: wasmtime::Extern = self.regfile.into();

        let load = Func::wrap(
            &mut self.store,
            |mut caller: Caller<'_, Cpu>, addr: i64, size: i32| -> wasmtime::Result<i64> {
                let cpu = caller.data_mut();
                match cpu.jit_mem(addr as u64, size as u8, false, 0) {
                    Some(v) => Ok(v as i64),
                    None => wasmtime::bail!("jit load abort (fault/mmio)"),
                }
            },
        );
        let store_fn = Func::wrap(
            &mut self.store,
            |mut caller: Caller<'_, Cpu>, addr: i64, size: i32, val: i64| -> wasmtime::Result<()> {
                let cpu = caller.data_mut();
                match cpu.jit_mem(addr as u64, size as u8, true, val as u64) {
                    Some(_) => Ok(()),
                    None => wasmtime::bail!("jit store abort (fault/mmio)"),
                }
            },
        );

        let Ok(instance) = Instance::new(
            &mut self.store,
            &module,
            &[regfile, load.into(), store_fn.into()],
        ) else {
            return false;
        };
        let Ok(run) = instance.get_typed_func::<(i64, i64), (i64, i64)>(&mut self.store, "run")
        else {
            return false;
        };

        self.blocks_translated += 1;
        self.region_blocks_total += u64::from(tr.blocks);
        self.instances_live += 1;
        self.cache.insert(
            phys,
            CachedRegion {
                instance,
                run,
                touches_mem: tr.touches_mem,
                page: phys >> 12,
            },
        );
        true
    }

    /// Run (translate-on-miss, then execute) the **region** at physical entry `phys`
    /// for the current `cpu` state, with instruction `budget`. The 16 GPRs + rflags
    /// are copied into the shared register file, `run(entry_rip, budget)` is called
    /// (running many guest instructions across the region's blocks in one Wasm call),
    /// and the registers are left in the Wasm file (lazy sync). Returns the
    /// [`BlockRun`] outcome. Guest code bytes are fetched (from `cpu`, page-sized for
    /// region discovery) **only on a cache miss**. `entry_rip` is the guest virtual
    /// `rip`.
    fn run_block(&mut self, cpu: &mut Cpu, phys: u64, entry_rip: u64, budget: u64) -> BlockRun {
        // Tiering gate: only compiled (cached) entries run natively. The inline
        // last-region cache shortcuts a hot loop re-entering the same region.
        if phys != self.last_phys && !self.cache.contains_key(&phys) {
            // Cold entry — count it; compile only once it crosses the threshold.
            let count = self.hotness.entry(phys).or_insert(0);
            if *count == HOTNESS_REJECTED {
                return BlockRun::Interpret; // translator already rejected this entry
            }
            *count += 1;
            if *count < self.hot_threshold {
                return BlockRun::Interpret; // not hot yet — interpret one instruction
            }
            // Hot enough. Keep the store's instance count bounded: rebuild it if
            // full. The rebuild discards the register file, so flush the live
            // registers to `cpu` first (lazy sync may have left them in Wasm).
            if self.instances_live >= CACHE_CAP {
                self.sync_from_wasm(cpu);
                self.rebuild_store();
            }
            // Compile and cache (or park as rejected). Fetch a page's worth so the
            // region can discover direct-branch-reachable blocks across the page.
            let code = cpu.jit_fetch_code(phys, 4096);
            if !self.translate_and_cache(phys, &code, entry_rip) {
                self.hotness.insert(phys, HOTNESS_REJECTED);
                return BlockRun::Interpret;
            }
            self.hotness.remove(&phys);
            // The phys page now carries a cached region — mark it so the interpreter
            // write path detects SMC against it.
            cpu.jit_mark_code_page(phys >> 12);
        }
        self.last_phys = phys;

        let (run, touches_mem) = {
            let b = self.cache.get(&phys).expect("just inserted/contained");
            (b.run.clone(), b.touches_mem)
        };
        let mem = self.regfile;

        self.sync_to_wasm(cpu);

        let result = if touches_mem {
            // Memory-touching region: lend the real core to the host imports for the
            // call (a cheap struct move — RAM/TLB are heap-backed).
            core::mem::swap(cpu, self.store.data_mut());
            let r = run.call(&mut self.store, (entry_rip as i64, budget as i64));
            core::mem::swap(cpu, self.store.data_mut());
            r
        } else {
            // Register-only region: it calls no host import, so it cannot trap and
            // needs no `Cpu` lent into the store — the cheapest path.
            run.call(&mut self.store, (entry_rip as i64, budget as i64))
        };

        match result {
            Ok((next_rip, ran)) => {
                // The region left the updated registers in the shared Wasm file; keep
                // them there (lazy sync) — a following JIT region reuses them with no
                // copy, and the driver flushes them to `cpu` only at a boundary that
                // needs them. `regs_in_wasm` is already `true` (set by `sync_to_wasm`).
                debug_assert!(self.regs_in_wasm);
                self.blocks_run += 1;
                BlockRun::Done {
                    next_rip: next_rip as u64,
                    insns: ran as u64,
                }
            }
            // A host import trapped: the region aborted on a guest page fault / MMIO
            // access. The shared register file holds the architectural state *as of
            // the last completed instruction* (every prior instruction in the region
            // committed its register/flag/RAM effects; the aborting instruction
            // committed none — its memory access is its first or last effect). Sync
            // that state into `cpu` and resume the interpreter at the aborting
            // instruction's rip (stamped into `FAULT_RIP_OFF` by the region before the
            // access), crediting the timer with the instructions the region retired
            // before the abort (stamped into `RETIRED_OFF`). The interpreter then
            // re-runs *only* that one instruction with full side effects.
            Err(_) => {
                let data = mem.data(&self.store);
                let mut rb = [0u8; 8];
                rb.copy_from_slice(&data[FAULT_RIP_OFF..FAULT_RIP_OFF + 8]);
                let fault_rip = u64::from_le_bytes(rb);
                let mut nb = [0u8; 8];
                nb.copy_from_slice(&data[RETIRED_OFF..RETIRED_OFF + 8]);
                let retired = u64::from_le_bytes(nb);
                // Flush the pre-fault register/flag state (held in the Wasm file) into
                // `cpu`, making it authoritative for the interpreter.
                self.sync_from_wasm(cpu);
                self.blocks_trapped += 1;
                BlockRun::Trapped { fault_rip, retired }
            }
        }
    }
}

impl Cpu {
    /// **The CC-48 JIT-accelerated run loop** — drives up to `max_steps` guest
    /// instructions, executing hot blocks natively through `exec` and falling back
    /// to the interpreter for everything else. A drop-in faster alternative to
    /// [`Cpu::run`](Cpu::run) with byte-identical architectural behaviour: it
    /// mirrors `run`'s per-iteration `sys_tick` / `halted` / interrupt / `#PF`
    /// bookkeeping, and a block writes the same architectural register/`rflags`
    /// state the equivalent interpreted instructions would. `std`-only.
    ///
    /// Each iteration: pump time + a pending interrupt (exactly as `run`), then
    /// translate the rip's physical address to a cached block. A cached block runs
    /// in one Wasm call and advances the budget by its retired `insns`; a trap or an
    /// untranslatable head interprets exactly one instruction the same way `run`'s
    /// body does, including the post-`step` page-fault vector.
    #[cfg(feature = "std")]
    #[must_use]
    pub fn run_jit(&mut self, exec: &mut X64JitExec, max_steps: u64) -> Halt {
        let mut steps = 0u64;
        while steps < max_steps {
            // Per-iteration device/timer bookkeeping. Unlike `run` (which `sys_tick`s
            // once per instruction), a region retires *many* instructions per
            // iteration, so the timer is advanced by the iteration's *actual* retired
            // count (`jit_sys_tick_n`) at the END of the iteration — keeping the
            // periodic timer/jiffies on the real instruction clock. The pending
            // interrupt latched by that advance is delivered at the next iteration's
            // top (a latency bounded by `REGION_BUDGET`, far under any timer period).
            self.jit_pump_net(steps);
            if self.jit_halted() {
                exec.sync_from_wasm(self); // make `cpu` registers authoritative on exit
                return Halt::Halted;
            }
            // Interrupt delivery pushes a frame using `rsp`/`rflags`, so flush the
            // live registers to `cpu` first — but only when one is actually pending
            // (the common case is none, so the registers stay in Wasm).
            if self.jit_interrupt_pending() {
                exec.sync_from_wasm(self);
            }
            self.jit_take_pending_interrupt();

            // If a prior interpreter store hit a cached code page, flush the stale
            // region(s) before this iteration translates/runs anything.
            if self.jit_take_dirty() {
                exec.flush_invalidated(self);
            }

            // Translate the current rip to a physical address. A faulting fetch (the
            // code page is not present) falls to the interpreter, which takes the
            // #PF on the instruction fetch itself.
            let rip = self.rip();
            let interpret_one;
            if let Some(phys) = self.jit_fetch_phys(rip) {
                // Cap the region's budget by (a) the remaining step allowance, so the
                // driver honours `max_steps`, and (b) the instructions until the next
                // armed timer fires, so a long region exits exactly at the timer
                // deadline and the driver delivers the IRQ promptly — keeping the
                // interrupt cadence close to the interpreter's (a region cannot deliver
                // an interrupt mid-run, so stranding one for a whole region breaks the
                // kernel's timing-sensitive paths).
                let budget = REGION_BUDGET
                    .min(max_steps - steps)
                    .min(self.jit_steps_to_next_timer())
                    .max(1);
                match exec.run_block(self, phys, rip, budget) {
                    BlockRun::Done { next_rip, insns } => {
                        self.jit_set_rip(next_rip);
                        self.jit_add_insns(insns);
                        exec.jit_insns += insns;
                        steps += insns;
                        // Advance the timer by the region's retired count. The region
                        // contains no `RDTSC`/`RDRAND` (those stop a block → always
                        // interpreted), so it observes no TSC and advancing it here
                        // (after the run) is indistinguishable from per-instruction.
                        self.jit_sys_tick_n(insns);
                        interpret_one = false;
                    }
                    // A load/store aborted mid-region: the register state before the
                    // aborting instruction is already in `cpu`; credit the timer with
                    // the instructions the region retired before the abort, then
                    // resume the interpreter at the faulting instruction.
                    BlockRun::Trapped { fault_rip, retired } => {
                        self.jit_add_insns(retired);
                        exec.jit_insns += retired;
                        steps += retired;
                        self.jit_sys_tick_n(retired);
                        self.jit_set_rip(fault_rip);
                        self.jit_clear_fault();
                        interpret_one = true;
                    }
                    // The entry instruction is untranslatable: clear any latched fault
                    // (the rip-fetch translate above may have filled the TLB only) and
                    // interpret one instruction exactly as `run` does.
                    BlockRun::Interpret => {
                        self.jit_clear_fault();
                        interpret_one = true;
                    }
                }
            } else {
                self.jit_clear_fault();
                interpret_one = true;
            }

            if interpret_one {
                // The interpreter reads/writes the architectural `cpu` registers, so
                // flush the live state out of the Wasm file first (a no-op if it is
                // already in `cpu` — e.g. just after a trap).
                exec.sync_from_wasm(self);
                // Interpret EXACTLY as `run`'s loop body does, in the same order:
                //   sys_tick  →  take_pending_interrupt  →  step
                // (1) Advance the timer for THIS instruction *before* executing it.
                //     Load-bearing: `RDTSC`/`RDRAND` are interpreted here and observe
                //     the TSC — ticking after the step (a region's pattern) would feed
                //     them a TSC one step behind the interpreter, and `RDRAND` mixes the
                //     TSC into its output, so every random/canary/ASLR value would
                //     diverge and crash userspace.
                self.jit_sys_tick_n(1);
                // (2) Deliver any interrupt latched by *this* instruction's tick before
                //     executing it — exactly as `run` does. (The top-of-loop
                //     `take_pending` delivers at a region's entry; for a single
                //     interpreted instruction the architecturally-correct delivery point
                //     is here, right after its tick, so the interrupt cadence matches
                //     `run` instruction-for-instruction.)
                if self.jit_interrupt_pending() {
                    exec.sync_from_wasm(self);
                }
                self.jit_take_pending_interrupt();
                // (3) Interpret one instruction (`step()` → on Err return the halt; the
                //     post-`step` #PF vector is inside `step`).
                match self.jit_interpret_one() {
                    Ok(()) => {}
                    Err(h) => return h,
                }
                exec.interp_insns += 1;
                steps += 1;
            }
        }
        // Leave the architectural `cpu` registers authoritative for the caller (the
        // budget was exhausted with the live state possibly still in the Wasm file).
        exec.sync_from_wasm(self);
        Halt::OutOfBudget
    }
}
