//! **The x86-64 (AMD64 / Intel 64) core** — the system emulator's third ISA
//! target (`CC-43`), alongside the [RISC-V](super) `RV64GC` core and the
//! [`aarch64`](crate::emulator::aarch64) `ARMv8-A` core.
//!
//! x86-64 is the ubiquitous registry architecture: most container images publish
//! a `linux/amd64` variant, so this is the core that lets the browser peer boot
//! the largest share of real devcontainers. Like the other cores it is a CPU over
//! the **shared device bus** (`devbus` (`super::devbus`)) — the κ-disk (`virtio`), the
//! console, the userspace NAT — so the disk, networking, and workspace are not
//! re-implemented per ISA (Law L4). Deterministic: identical image + input yield
//! identical console output and final state (Law L1/L5), so a κ snapshot is
//! reproducible across peers.
//!
//! **Architectural authority.** This core is built to the published x86-64
//! specification — the *Intel® 64 and IA-32 Architectures Software Developer's
//! Manual* (instruction semantics from Vol 2; long-mode paging, the local APIC +
//! APIC timer, the IDT and interrupt/exception delivery, the 8259-via-LINT0
//! virtual-wire mode, and `IA32_TSC` from Vol 3A). The SDM is imported and pinned
//! under `vv/artifacts/intel-sdm/` and, per the V&V governance, paired with an
//! external validator: `qemu-system-x86_64` (an independent SDM implementation) is
//! the differential oracle `CC-44` witnesses the boot against — behaviour is
//! defined by the spec and checked against the reference, never self-referentially.
//!
//! This module is the **long-mode integer core + platform** it boots on: the
//! 64-bit register file and `RFLAGS`, a flat 64-bit address space over guest RAM,
//! the legacy `16550` serial console (port `0x3f8`, the boot console), and the
//! decode/execute of the x86-64 integer instruction set (REX-prefixed,
//! ModRM/SIB-addressed). Conformance is built up instruction-by-instruction (the
//! analogue of the RISC-V core's riscv-tests, `CC-9`); the full real-mode→long-mode
//! Linux boot path composes on this core.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use hologram_substrate_core::{address_bytes, verify_kappa, KappaLabel71, KappaStore};

/// Why the core stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Halt {
    /// The step budget was exhausted (the caller drives more).
    OutOfBudget,
    /// A `hlt` with interrupts disabled — the guest powered off / wedged.
    Halted,
    /// An unsupported or malformed instruction was hit (the core does not yet
    /// implement it) — carries the faulting `rip`.
    Undefined(u64),
}

/// `RFLAGS` bits the integer core maintains.
mod flag {
    pub const CF: u64 = 1 << 0;
    pub const PF: u64 = 1 << 2;
    pub const ZF: u64 = 1 << 6;
    pub const SF: u64 = 1 << 7;
    pub const OF: u64 = 1 << 11;
}

/// The legacy `16550` UART (the PC serial console at I/O port `0x3f8`) — the
/// boot console (`console=ttyS0`). Output is buffered for [`Cpu::console`]; input
/// is the terminal channel ([`Cpu::feed_console`], `CC-11`).
struct Uart {
    output: Vec<u8>,
    input: Vec<u8>,
    in_cursor: usize,
    /// Line Control Register — bit 7 (`DLAB`) selects the divisor latch over the
    /// data/IER registers; the kernel sets the word length here too.
    lcr: u8,
    /// Interrupt Enable Register (the kernel enables/disables UART IRQs).
    ier: u8,
    /// Modem Control Register — bit 4 (`LOOP`) puts the UART in the loopback the
    /// 8250 autodetection drives; the FIFO/scratch probes also run.
    mcr: u8,
    /// Scratch register (`0x3ff`) — the 8250 detection writes then reads it back
    /// to confirm a real UART is present.
    scratch: u8,
    /// FIFO Control Register shadow (the detection reads it back via IIR).
    fcr: u8,
    /// The divisor latch (the configured baud rate; written when `DLAB` is set).
    divisor: u16,
    /// Transmit-holding-register-empty interrupt pending (16550 THRE). Set on a
    /// THR-empty transition (a `THR` write, or enabling `ETBEI` while empty),
    /// CLEARED when the IIR is read — a one-shot per transition, not a level held
    /// while `ETBEI` is set. (A level THRE re-fires the serial ISR forever.)
    thre_pending: bool,
}

/// The shared platform the core drives: the console and the substrate devices —
/// the κ-disk, the shared workspace filesystem, and the userspace network — the
/// *same* [`VirtioBlk`](super::VirtioBlk) / [`Virtio9p`](super::Virtio9p) /
/// [`VirtioNet`](super::VirtioNet) the RISC-V and AArch64 machines boot, serviced
/// by the one shared `devbus` (Law L4: devices are shared, not per-ISA).
/// dev-only (`cc44-trace`): set while the CPU is executing inside `__text_poke`,
/// so the CR3-switch / aliased-write trace fires only for the poke sequence.
#[cfg(feature = "cc44-trace")]
static TP_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Dev instrumentation (std builds only — host tests + the wasm browser peer; the
// bare-metal no_std build excludes it entirely). A bounded ring of the most recent
// guest CPU-exception deliveries (vectors < 32), pushed in `deliver_interrupt`.
// Fires only on exceptions (rare vs. instructions/IRQs), so the cost is negligible.
// Used to post-mortem the x64 wasm demand-paging divergence (the execve user-stack
// `#PF` that kills init in the browser but not natively). Drained, newline-joined,
// via `X64Workspace::exc_trace`.
#[cfg(feature = "std")]
thread_local! {
    static EXC_TRACE: core::cell::RefCell<Vec<String>> = const { core::cell::RefCell::new(Vec::new()) };
}
#[cfg(feature = "std")]
fn exc_trace_push(line: String) {
    EXC_TRACE.with(|t| {
        let mut v = t.borrow_mut();
        v.push(line);
        let n = v.len();
        if n > 1024 {
            v.drain(0..n - 1024);
        }
    });
}
/// Drain the dev exception-trace ring (most recent guest CPU exceptions).
#[cfg(feature = "std")]
#[must_use]
pub fn drain_exc_trace() -> Vec<String> {
    EXC_TRACE.with(|t| core::mem::take(&mut *t.borrow_mut()))
}

// A ring of the most recent executed instruction starts (rip, cpl) — for
// post-mortem of a bad jump (e.g. control reaching a poison/filler page): the
// tail shows the exact branch path in. Trace builds only (a push per instruction).
#[cfg(feature = "cc44-trace")]
thread_local! {
    // (rip, rsp, rbp) — user instructions only. rbp is callee-saved, so a callee
    // that returns with rbp changed is a bug (the fork()-path corruption hunt).
    static RIP_RING: core::cell::RefCell<std::collections::VecDeque<(u64, u64, u64)>> =
        core::cell::RefCell::new(std::collections::VecDeque::new());
}
#[cfg(feature = "cc44-trace")]
fn rip_ring_push(rip: u64, rsp: u64, rbp: u64) {
    RIP_RING.with(|t| {
        let mut v = t.borrow_mut();
        v.push_back((rip, rsp, rbp));
        if v.len() > 8192 {
            v.pop_front();
        }
    });
}

// ── x86-64 self-consistency capture (dev-only, `cc44-trace`) ─────────────────────
//
// A supported debugging hook for the x86-64 core. When armed via `Cpu::cc44_trace`,
// `step()` appends one line per executed instruction to a thread-local buffer:
//
//     rip  op[16 hex bytes]  rflags  g0..g15  mem(8 low GPRs × 4 u64 @ [reg])  xmm0..15
//
// `Cpu::cc44_trace_drain` returns the buffer. The companion analyzers in
// `scratchpad/` (selfck/valck/pushck/sseck/sseld/storeck .py — capstone-based)
// then recompute each instruction's SDM-correct flags/values/EA from its OWN
// captured inputs and flag the first divergence — a base/qemu-independent
// self-consistency check that proved the core correct across ~17,900 instruction
// effects (see memory holo-x64-printf-escape-crash). Use this when CC-62 (or a real
// workload) goes red, to localize a miscompute to a single instruction.
#[cfg(feature = "cc44-trace")]
thread_local! {
    static SELFCK_BUF: core::cell::RefCell<String> = const { core::cell::RefCell::new(String::new()) };
    static SELFCK_ON: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}
#[cfg(feature = "cc44-trace")]
fn selfck_on() -> bool {
    SELFCK_ON.with(core::cell::Cell::get)
}
#[cfg(feature = "cc44-trace")]
fn selfck_push(line: String) {
    SELFCK_BUF.with(|b| b.borrow_mut().push_str(&line));
}
#[cfg(feature = "cc44-trace")]
impl Cpu {
    /// Arm/disarm the per-instruction self-consistency capture (userspace, cpl==3).
    /// Arming clears the buffer. See the module-level note for the line format and
    /// the `scratchpad/` analyzer workflow.
    #[doc(hidden)]
    pub fn cc44_trace(on: bool) {
        SELFCK_ON.with(|c| c.set(on));
        if on {
            SELFCK_BUF.with(|b| b.borrow_mut().clear());
        }
    }
    /// Take the captured trace (one line per executed userspace instruction).
    #[doc(hidden)]
    #[must_use]
    pub fn cc44_trace_drain() -> String {
        SELFCK_BUF.with(|b| core::mem::take(&mut *b.borrow_mut()))
    }
}

// JIT Rung 0 — a sampling hot-code profiler (std/dev only). `run()` samples `rip`'s
// 4 KiB code page every 1024 instructions into a histogram; a few hot pages dominating
// the samples = the JIT thesis (compile those once per planet, the rest is cold). Cheap:
// one mask+compare per instruction, a map update 1/1024.
#[cfg(feature = "std")]
thread_local! {
    static HOTPROF: core::cell::RefCell<std::collections::HashMap<u64, u64>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
}
#[cfg(feature = "std")]
fn hotprof_sample(rip: u64) {
    HOTPROF.with(|h| *h.borrow_mut().entry(rip & !0xfff).or_insert(0) += 1);
}
// Diagnostic: count instructions retired at CPL==3 (userspace) since the last reset, so a
// test can prove whether a *resumed* machine ever schedules a userspace task at all (the
// resume-shell-executes investigation). One compare+add per step; off in non-std builds.
#[cfg(feature = "std")]
thread_local! {
    static USER_INSNS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static USER_PAGES: core::cell::RefCell<std::collections::HashMap<u64, u64>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
}
/// Drain the userspace (CPL==3) code-page histogram, hottest-first — shows WHAT userspace ran.
#[cfg(feature = "std")]
#[must_use]
pub fn drain_user_pages() -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> =
        USER_PAGES.with(|h| core::mem::take(&mut *h.borrow_mut()).into_iter().collect());
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

// Diagnostic: a histogram of the PRIMARY opcode byte (post-prefix) of every executed instruction,
// and (when that byte is 0x0F) the two-byte secondary, so the JIT decoder can be broadened by
// FREQUENCY — cover the hot 20% of opcodes that are 80% of execution, not blindly. A fixed 256-slot
// array increment per instruction (~1 ns, negligible vs Cpu::step's ~45 ns).
#[cfg(feature = "std")]
thread_local! {
    static OPHIST: core::cell::RefCell<[u64; 256]> = const { core::cell::RefCell::new([0u64; 256]) };
    static OPHIST_0F: core::cell::RefCell<[u64; 256]> = const { core::cell::RefCell::new([0u64; 256]) };
}
// The opcode histogram is a DEV profiling aid, but its per-instruction thread-local `RefCell`
// increment was UNCONDITIONALLY in the release hot path (a real drag on the ~8 Minsn/s interpreter,
// like the boot-diag counter above). Gate it on a static atomic that defaults OFF: production pays
// only a cached relaxed load + a predicted-not-taken branch; `reset_ophist` arms it for profiling.
#[cfg(feature = "std")]
static OPHIST_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Reset **and arm** the opcode-frequency histograms (turns per-instruction recording ON — the
/// production hot path leaves it off). Pair with [`drain_ophist`].
#[cfg(feature = "std")]
pub fn reset_ophist() {
    OPHIST.with(|h| *h.borrow_mut() = [0u64; 256]);
    OPHIST_0F.with(|h| *h.borrow_mut() = [0u64; 256]);
    OPHIST_ON.store(true, core::sync::atomic::Ordering::Relaxed);
}
/// Drain the opcode-frequency histograms as `(primary, secondary_for_0f, sorted (op,count))`.
/// `secondary` is the `0F xx` two-byte distribution; `primary[0x0F]` is the total two-byte count.
#[cfg(feature = "std")]
#[must_use]
pub fn drain_ophist() -> (Vec<(u8, u64)>, Vec<(u8, u64)>) {
    let pull = |t: &core::cell::RefCell<[u64; 256]>| {
        let a = core::mem::replace(&mut *t.borrow_mut(), [0u64; 256]);
        let mut v: Vec<(u8, u64)> =
            a.iter().enumerate().filter(|(_, &c)| c > 0).map(|(i, &c)| (i as u8, c)).collect();
        v.sort_by(|x, y| y.1.cmp(&x.1));
        v
    };
    OPHIST_ON.store(false, core::sync::atomic::Ordering::Relaxed);
    (OPHIST.with(pull), OPHIST_0F.with(pull))
}
/// Userspace (CPL==3) instructions retired since [`reset_user_insns`].
#[cfg(feature = "std")]
#[must_use]
pub fn user_insns_seen() -> u64 {
    USER_INSNS.with(core::cell::Cell::get)
}
/// Reset the userspace-instruction diagnostic counter.
#[cfg(feature = "std")]
pub fn reset_user_insns() {
    USER_INSNS.with(|c| c.set(0));
}
/// Drain the hot-code profile as `(code_page, sample_count)` sorted hottest-first.
#[cfg(feature = "std")]
#[must_use]
pub fn drain_hotprof() -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> =
        HOTPROF.with(|h| core::mem::take(&mut *h.borrow_mut()).into_iter().collect());
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

// JIT Rung 2 — block discovery (std/dev only, OFF by default so the shipping run loop
// pays nothing). When armed (`set_blockprof`), `run()` detects a control transfer cheaply
// — the next `rip` is not the sequential fall-through (within x86's 15 B max insn length)
// — and samples the *target* (= a basic-block entry = a JIT compilation unit) 1/64, plus
// tracks total block entries + instructions so the test can report the hot blocks to
// compile and the average block length.
#[cfg(feature = "std")]
thread_local! {
    static BLOCKPROF: core::cell::RefCell<std::collections::HashMap<u64, u64>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
    static BLOCKPROF_ON: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static BLOCK_ENTRIES: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}
/// Arm/disarm JIT Rung-2 block profiling (off in production; the test turns it on).
#[cfg(feature = "std")]
pub fn set_blockprof(on: bool) {
    BLOCKPROF_ON.with(|c| c.set(on));
}
#[cfg(feature = "std")]
#[inline]
fn blockprof_entry(target: u64) {
    let n = BLOCK_ENTRIES.with(|c| {
        let v = c.get() + 1;
        c.set(v);
        v
    });
    if n & 0x3f == 0 {
        BLOCKPROF.with(|h| *h.borrow_mut().entry(target).or_insert(0) += 1);
    }
}
/// Drain the Rung-2 block profile: `(block_entries, sorted (block_start, sample_count))`.
#[cfg(feature = "std")]
#[must_use]
pub fn drain_blockprof() -> (u64, Vec<(u64, u64)>) {
    let entries = BLOCK_ENTRIES.with(|c| c.replace(0));
    let mut v: Vec<(u64, u64)> =
        BLOCKPROF.with(|h| core::mem::take(&mut *h.borrow_mut()).into_iter().collect());
    v.sort_by(|a, b| b.1.cmp(&a.1));
    (entries, v)
}

// A runtime toggle for the interpreter fast paths (string-op bulk copies, retpoline
// skip). On by default; the micro-benchmark turns it off to measure each fast path's
// speedup cleanly (a synthetic, ASLR-independent workload — unlike a whole boot).
#[cfg(feature = "std")]
thread_local! {
    static FASTPATH_ON: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
}
/// Enable/disable the interpreter fast paths (for benchmarking; on in production).
#[cfg(feature = "std")]
pub fn set_fastpaths(on: bool) {
    FASTPATH_ON.with(|c| c.set(on));
}
#[inline(always)]
fn fastpaths_on() -> bool {
    #[cfg(feature = "std")]
    {
        FASTPATH_ON.with(core::cell::Cell::get)
    }
    #[cfg(not(feature = "std"))]
    {
        true
    }
}

// JIT Rung 3 — the block JIT dispatch (the `jit` feature, OFF by default so the shipping
// run loop pays nothing). When armed (`set_jit_on`), `run()` discovers the linear block at
// each basic-block entry, keys it by **BLAKE3** (κ), and records it; the cache compiles a
// block to wasm once it crosses the hotness threshold. Step A wires the discovery + compile
// path on real boots (no execution yet — that, plus the differential, is the next sub-step;
// the discovery is side-effect-free, so it cannot perturb the boot).
/// A compiled block whose JIT result is being shadow-validated against the interpreter: the
/// executor's `(regs, rflags, dirty pages)` for the block `[start, end)`, recorded when
/// `jit_dispatch` ran the JIT dry, compared once the interpreter reaches `end` straight-line.
#[cfg(feature = "jit-native")]
struct JitPending {
    key: [u8; 32],
    start: u64,
    end: u64,
    regs: [u64; 16],
    rflags: u64,
    dirty: Vec<(usize, Vec<u8>)>,
    has_shift: bool,
}
/// A compiled REGION being shadow-validated (the chaining analogue of [`JitPending`]): the dry
/// result for the region entered at `start`, to be compared once the interpreter reaches `exit`
/// (the region's exit rip). `span_end` is the region's last byte rip — while the interpreter is
/// inside `[start, span_end)` (looping/branching within the region) shadowing is KEPT.
#[cfg(feature = "jit")]
struct JitRegionPending {
    key: [u8; 32],
    start: u64,
    span_end: u64,
    exit: u64,
    regs: [u64; 16],
    rflags: u64,
    dirty: Vec<(usize, Vec<u8>)>,
    /// `self.insns` at region entry — the interpreter's retired-instruction count when this region was
    /// shadowed. At the exit compare, `insns_now - entry_insns` = how many guest instructions the
    /// region actually executed; a region that executes too FEW (a short, few-iteration region) is
    /// refused — the per-commit marshalling cost exceeds the win (the real-Alpine 0.44× lesson).
    entry_insns: u64,
    /// Diagnostic-only (populated when `JIT_REGION_NOTRUST` is set): the region's decoded ops, so a
    /// divergence can be pinned to a specific instruction shape. Empty in normal operation.
    ops_dbg: String,
}
/// The cross-crate region-executor signature (the seam) — same shape as `exec_region_pooled`.
#[cfg(feature = "jit")]
type RegionExecFn = fn(
    [u8; 32],
    &[u8],
    [u64; 16],
    u64,
    &dyn Fn(u64) -> Option<(usize, Vec<u8>)>,
) -> Option<([u64; 16], u64, Vec<(usize, Vec<u8>)>, u64)>;
/// Shadow matches required before a block is trusted (and shadowing stops).
#[cfg(feature = "jit")]
const JIT_TRUST_K: u32 = 4;
/// Minimum guest instructions a region must execute PER ENTRY to be worth JITting. Below this the
/// per-commit marshalling (regs+rflags+TLB image+page pool through the wasm `Memory`) costs more than
/// just interpreting the region — measured on real Alpine, where short function-body regions made the
/// region JIT a 0.44× *slowdown*. A region that executes fewer than this on its shadow run is refused
/// (its κ never JITs again), so only genuinely hot/long-running regions (real loops) ever commit.
#[cfg(feature = "jit")]
const REGION_MIN_INSNS: u64 = 512;
/// Minimum block length (modelled ops) to JIT. With warm-instance reuse the per-commit cost
/// drops to a register marshal + page re-fetch, so short blocks become worth committing; this
/// gate just skips the tiniest blocks (4–7 ops) where even that overhead doesn't pay. Tune
/// against the A/B measurement. (The boot has NO blocks ≥32 ops — every hot block is short.)
/// Used only by the native block-JIT path (`jit_dispatch`); the region path gates on block count.
#[cfg(feature = "jit-native")]
const JIT_MIN_OPS: usize = 8;
#[cfg(feature = "jit")]
thread_local! {
    static JIT_ON: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static JIT_AT_ENTRY: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
    static JIT_CACHE: core::cell::RefCell<crate::emulator::jit::BlockCache> =
        core::cell::RefCell::new(crate::emulator::jit::BlockCache::new(16));
    static JIT_DECODED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    #[cfg(feature = "jit-native")]
    static JIT_PENDING: core::cell::RefCell<Option<JitPending>> =
        const { core::cell::RefCell::new(None) };
    static JIT_TRUSTED: core::cell::RefCell<std::collections::HashSet<[u8; 32]>> =
        core::cell::RefCell::new(std::collections::HashSet::new());
    static JIT_REFUSED: core::cell::RefCell<std::collections::HashSet<[u8; 32]>> =
        core::cell::RefCell::new(std::collections::HashSet::new());
    /// κ-keyed cache of compiled REGION wasm (the chaining dispatch — `jit_run_region`).
    static JIT_REGION_CACHE: core::cell::RefCell<std::collections::HashMap<[u8; 32], Vec<u8>>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
    static JIT_REGION_PENDING: core::cell::RefCell<Option<JitRegionPending>> =
        const { core::cell::RefCell::new(None) };
    static JIT_REGION_COUNT: core::cell::RefCell<std::collections::HashMap<[u8; 32], u32>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
    /// The CHAINING dispatch enable flag + a per-rip hotness counter (only try a region once a rip
    /// is hot — a loop back-edge makes its head hot fast). Separate from `JIT_ON` (the per-block path).
    static JIT_REGION_ON: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static JIT_REGION_HOT: core::cell::RefCell<std::collections::HashMap<u64, u32>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
    /// The injectable region EXECUTOR (the cross-crate seam): native leaves it `None` and falls back
    /// to the wasmtime `exec_region_pooled`; the browser peer (`holospaces-web`, which has `js_sys`)
    /// injects a `WebAssembly`-based executor via `set_region_executor` — so the core emulator never
    /// depends on a wasm runtime. Same signature as `exec_region_pooled` (key, wasm, entry regs/flags,
    /// page-fetch) → `(regs, rflags, dirty, exit_rip)`.
    static REGION_EXEC: core::cell::Cell<Option<RegionExecFn>> = const { core::cell::Cell::new(None) };
    /// Diagnostic: when set, `jit_run_region` NEVER takes the trusted-commit fast path — every region
    /// shadows, so the dry result is compared to the interpreter on EVERY entry (not just the first K).
    /// This catches a region whose codegen is wrong for an entry-state that the K samples happened to
    /// miss (the trust-by-κ hole). No region commits, so execution is unperturbed (== interpreter).
    static JIT_REGION_NOTRUST: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The FIRST region divergence caught under `JIT_REGION_NOTRUST` (rip + which field + ops), drained
    /// by `drain_region_divergence`. Pins the buggy region to a specific instruction shape.
    static JIT_REGION_DIVERGENCE: core::cell::RefCell<Option<String>> =
        const { core::cell::RefCell::new(None) };
    static JIT_COUNT: core::cell::RefCell<std::collections::HashMap<[u8; 32], u32>> =
        core::cell::RefCell::new(std::collections::HashMap::new());
    static JIT_MATCH: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static JIT_MISMATCH: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static JIT_COMMITTED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    // mismatch breakdown (which field diverged, and whether the block had a shift)
    static JIT_MM_REGS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static JIT_MM_RFLAGS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static JIT_MM_MEM: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static JIT_MM_SHIFT: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}
/// Drain the mismatch breakdown: `(diverged_regs, diverged_rflags, diverged_mem, had_shift)`.
#[cfg(feature = "jit")]
#[must_use]
pub fn drain_jit_diag() -> (u64, u64, u64, u64) {
    (
        JIT_MM_REGS.with(|c| c.replace(0)),
        JIT_MM_RFLAGS.with(|c| c.replace(0)),
        JIT_MM_MEM.with(|c| c.replace(0)),
        JIT_MM_SHIFT.with(|c| c.replace(0)),
    )
}
/// Arm/disarm the block JIT (off in production; a test or the host turns it on).
/// Block-JIT is the NATIVE path only (it runs through the wasmtime `exec_block_pooled`);
/// the browser uses the chaining REGION path (`set_region_jit_on`) over the executor seam.
#[cfg(feature = "jit-native")]
pub fn set_jit_on(on: bool) {
    JIT_ON.with(|c| c.set(on));
}
/// Arm/disarm the CHAINING (region) JIT — hot loops run as one wasm region. Off in production; a
/// test or the host turns it on. Clears the per-rip hotness counter on disable.
#[cfg(feature = "jit")]
pub fn set_region_jit_on(on: bool) {
    JIT_REGION_ON.with(|c| c.set(on));
    if !on {
        JIT_REGION_HOT.with(|c| c.borrow_mut().clear());
    }
}
/// Inject the region executor (the cross-crate seam) — the browser peer supplies a
/// `WebAssembly`-based executor; native leaves it unset and uses the wasmtime `exec_region_pooled`.
#[cfg(feature = "jit")]
pub fn set_region_executor(f: RegionExecFn) {
    REGION_EXEC.with(|c| c.set(Some(f)));
}
/// Diagnostic: never trust/commit a region — always shadow-check it against the interpreter. Use to
/// hunt a divergence the trust-by-κ path commits (the region JIT must still be armed via
/// `set_region_jit_on`). Execution is unperturbed (no region commits), so output equals the pure
/// interpreter; the first mismatching region is captured for `drain_region_divergence`.
#[cfg(feature = "jit")]
pub fn set_region_notrust(on: bool) {
    JIT_REGION_NOTRUST.with(|c| c.set(on));
    if !on {
        JIT_REGION_DIVERGENCE.with(|c| *c.borrow_mut() = None);
    }
}
/// Drain the first region divergence caught under `set_region_notrust` (the rip, the diverging field,
/// and the region's decoded ops) — `None` if every shadowed region matched the interpreter.
#[cfg(feature = "jit")]
#[must_use]
pub fn drain_region_divergence() -> Option<String> {
    JIT_REGION_DIVERGENCE.with(|c| c.borrow_mut().take())
}
/// Drain JIT stats: `(recorded, distinct, compiled, match, mismatch, trusted, refused, committed)`.
#[cfg(feature = "jit")]
#[must_use]
pub fn drain_jit_stats() -> (u64, usize, usize, u64, u64, usize, usize, u64) {
    let recorded = JIT_DECODED.with(|c| c.replace(0));
    let (entries, compiled) = JIT_CACHE.with(|c| c.borrow().stats());
    let m = JIT_MATCH.with(|c| c.replace(0));
    let mm = JIT_MISMATCH.with(|c| c.replace(0));
    let trusted = JIT_TRUSTED.with(|c| c.borrow().len());
    let refused = JIT_REFUSED.with(|c| c.borrow().len());
    let committed = JIT_COMMITTED.with(|c| c.replace(0));
    (recorded, entries, compiled, m, mm, trusted, refused, committed)
}

struct Sys {
    uart: Uart,
    /// The `virtio-blk` κ-disk rootfs (`CC-7`), when a disk is attached. The
    /// shared `devbus` services its queue against the κ-disk; the full long-mode
    /// boot path (`#12`) advertises the device to the guest.
    virtio: Option<super::VirtioBlk>,
    /// The `virtio-9p` device serving the shared workspace filesystem, when
    /// attached (`CC-15` parity); `None` otherwise.
    virtio9p: Option<super::Virtio9p>,
    /// The `virtio-net` device + the userspace TCP/IP NAT, when networking is
    /// attached (`CC-16` parity); `None` for an offline machine.
    virtionet: Option<super::VirtioNet>,
    /// The `virtio-input` device (keyboard + relative pointer), when attached
    /// (`CC-46`); `None` for a headless machine. Host input is enqueued here and
    /// drained into the eventq, waking the guest's input + X event loops.
    virtioinput: Option<super::VirtioInput>,
    /// The host side of the in-process loopback bridge (ADR-020, `CC-33`
    /// parity), when the workbench dials guest listeners; `None` until
    /// [`Cpu::enable_loopback`] attaches it.
    loopback: Option<super::net::LoopbackHandle>,
    /// The interrupt-descriptor table register (`IDTR`): `(base, limit)` of the
    /// 64-bit IDT the kernel installs with `LIDT` — the long-mode boot path's
    /// interrupt delivery (`#12`).
    idtr: (u64, u16),
    /// The global-descriptor table register (`GDTR`): `(base, limit)`. The loader
    /// installs the boot GDT; the kernel reloads its own with `LGDT`.
    gdtr: (u64, u16),
    /// The task register (`TR`) selector + the TSS base (loaded by `LTR`); the
    /// `RSP0` field of the TSS gives the kernel stack on a ring transition.
    tr_base: u64,
    /// The model-specific registers the boot path touches beyond `EFER`
    /// (`FS_BASE`, `GS_BASE`, `KERNEL_GS_BASE`, `STAR`/`LSTAR`/`SFMASK` for
    /// `syscall`, the APIC base, …), keyed by the MSR number.
    msr: BTreeMap<u32, u64>,
    /// The minimal interrupt controller (legacy 8259 PIC pair + the local APIC
    /// the kernel uses) — enough to vector the PIT timer tick and the `virtio`
    /// IRQs so the boot completes.
    pic: Pic,
    /// The 8254 PIT channel-0 timer: a free-running source whose terminal counts
    /// raise IRQ 0 (the boot time base the kernel calibrates against).
    pit: Pit,
    /// The local APIC: the kernel switches the timer/IPI path to it; only the
    /// registers a UP boot drives are modelled (`SVR`, `EOI`, the LVT timer, the
    /// timer initial/current count + divide).
    lapic: Lapic,
    /// The architected timestamp counter (`RDTSC`) — advanced in lockstep with the
    /// PIT so the kernel's delay loops and the TSC clocksource make progress.
    tsc: u64,
    /// A free-running step counter that paces the platform timers: they advance
    /// `true` once the machine has halted (a `hlt` with interrupts masked, the
    /// guest power-off) — the run loop then returns [`Halt::Halted`].
    halted: bool,
    /// SplitMix64 state for the hardware RNG (`RDRAND`/`RDSEED`). Seeded from a
    /// constant; each draw also mixes in the TSC so the stream is non-constant
    /// across the boot (the kernel only requires a varying value + `CF=1`).
    rng: u64,
    /// Free-running step counter pacing the PIT/APIC down-counters once per
    /// [`TICK_DIV`] steps (see [`Cpu::sys_tick`]).
    tdiv: u64,
    /// PCI CONFIG_ADDRESS latch (port 0xcf8). Readable so the kernel detects PCI
    /// config mechanism #1; the config-data port then returns all-ones (an empty
    /// bus — this machine's devices are virtio-mmio, not PCI).
    pci_addr: u32,
    /// Modelled L1 data-cache tags (one line number per set; direct-mapped). A miss
    /// charges [`DCACHE_MISS_CYCLES`] to the TSC, giving address-dependent access
    /// timing — the jitter `jitterentropy` needs on an otherwise deterministic core.
    dcache: Vec<u64>,
}

impl Sys {
    fn new() -> Self {
        Sys {
            uart: Uart {
                output: Vec::new(),
                input: Vec::new(),
                in_cursor: 0,
                lcr: 0,
                ier: 0,
                mcr: 0,
                scratch: 0,
                fcr: 0,
                divisor: 1,
                thre_pending: false,
            },
            virtio: None,
            virtio9p: None,
            virtionet: None,
            virtioinput: None,
            loopback: None,
            idtr: (0, 0),
            gdtr: (0, 0),
            tr_base: 0,
            msr: BTreeMap::new(),
            pic: Pic::new(),
            pit: Pit::new(),
            lapic: Lapic::new(),
            tsc: 0,
            halted: false,
            rng: 0x9e37_79b9_7f4a_7c15,
            tdiv: 0,
            pci_addr: 0,
            dcache: vec![u64::MAX; DCACHE_LINES],
        }
    }
}

/// A segment register's architectural cache: the selector plus the hidden base /
/// limit / attribute the descriptor loaded. In 64-bit mode `CS.L` selects long
/// mode and most bases are ignored (treated as 0) except `FS`/`GS`, whose bases
/// come from the `FS_BASE`/`GS_BASE` MSRs.
#[derive(Clone, Copy, Default)]
struct Seg {
    selector: u16,
    base: u64,
    /// Bit set from the descriptor's `L` (long-mode code) attribute — `CS.L`.
    long: bool,
}

/// The legacy 8259A PIC pair (master + slave). The boot path programs them
/// (ICW1..ICW4, the IRQ masks) before switching to the APIC; modelling the mask +
/// the in-service/request latches lets IRQ 0 (the PIT) and the `virtio` lines
/// vector through the IDT during early boot.
struct Pic {
    /// IRQ mask (1 = masked), master IRQs 0..8 in the low byte, slave 8..16 high.
    mask: u16,
    /// Pending (requested) IRQ lines.
    request: u16,
    /// The vector offset the master/slave were remapped to (ICW2).
    base_master: u8,
    base_slave: u8,
    /// ICW initialization sequence step per chip (0 = idle).
    init_master: u8,
    init_slave: u8,
}

impl Pic {
    fn new() -> Self {
        Pic {
            mask: 0xffff,
            request: 0,
            base_master: 0x08,
            base_slave: 0x70,
            init_master: 0,
            init_slave: 0,
        }
    }
    /// Raise IRQ line `n` (0..16) — latch it as requested.
    fn raise(&mut self, n: u8) {
        if n < 16 {
            self.request |= 1 << n;
        }
    }
    /// The highest-priority unmasked pending IRQ and its vector, or `None`.
    fn pending(&self) -> Option<(u8, u8)> {
        let active = self.request & !self.mask;
        if active == 0 {
            return None;
        }
        let irq = active.trailing_zeros() as u8;
        let vec = if irq < 8 {
            self.base_master.wrapping_add(irq)
        } else {
            self.base_slave.wrapping_add(irq - 8)
        };
        Some((irq, vec))
    }
    fn ack(&mut self, irq: u8) {
        self.request &= !(1 << irq);
    }
}

/// The 8254 PIT channel-0 timer. Only what the kernel's PIT clock-event needs is
/// modelled: the reload value and a step-driven down-counter that raises IRQ 0
/// each time it wraps (the periodic tick the scheduler/clocksource run on).
struct Pit {
    reload: u16,
    counter: u32,
    /// The latched low/high write toggle (the PIT is two byte-wide accesses).
    write_hi: bool,
    enabled: bool,
    /// Channel-0 mode: `true` = periodic (mode 2/3 — auto-reload, the legacy
    /// `HZ` tick), `false` = one-shot (mode 0 — fire once, then idle until the
    /// kernel reprograms a new deadline). The kernel's `clockevent` driver uses
    /// the PIT in one-shot mode and reprograms it from the tick handler; modelling
    /// that (rather than always auto-reloading) is what stops the tick handler's
    /// catch-up loop from re-firing forever and drowning the boot (`#12`).
    ch0_periodic: bool,
    /// Channel 2 — the speaker timer the kernel repurposes to *calibrate the TSC*
    /// (`pit_hpet_ptimer_calibrate_cpu`/`pit_calibrate_tsc`): it is gated on through
    /// port `0x61` bit 0, counts down once, and raises OUT2 (port `0x61` bit 5)
    /// when it reaches zero. The calibration busy-polls that OUT2 bit, so this must
    /// advance for the boot to get past TSC calibration (`#12`).
    ch2_reload: u16,
    ch2_counter: u32,
    ch2_write_hi: bool,
    /// The channel-2 gate (port `0x61` bit 0): counting only runs while it is set.
    ch2_gate: bool,
    /// OUT2 has gone high (the one-shot count expired) — reflected in `IN 0x61`
    /// bit 5, which the calibration loop waits on.
    ch2_out: bool,
}

impl Pit {
    fn new() -> Self {
        Pit {
            reload: 0,
            counter: 0,
            write_hi: false,
            enabled: false,
            ch0_periodic: true,
            ch2_reload: 0,
            ch2_counter: 0,
            ch2_write_hi: false,
            ch2_gate: false,
            ch2_out: false,
        }
    }
}

/// The local APIC (memory-mapped at the default `0xFEE0_0000`). Only the
/// registers a uniprocessor boot drives: the spurious-vector register (enable),
/// `EOI`, and the LVT timer + its initial/current count and divide — enough to
/// deliver the APIC-timer tick once the kernel migrates off the PIT.
struct Lapic {
    /// Spurious-interrupt vector register; bit 8 is the APIC software-enable.
    svr: u32,
    /// LVT timer entry: the vector (low 8 bits), mask (bit 16), mode (bit 17 =
    /// periodic).
    lvt_timer: u32,
    initial_count: u32,
    current_count: u32,
    divide: u32,
    /// The task-priority register — interrupts at or below `TPR` are deferred.
    tpr: u32,
    /// Interrupt Request Register — the 256-bit pending-interrupt bitmap (the LVT
    /// timer, and IPIs delivered through the ICR). A real local-APIC IRR.
    irr: [u64; 4],
    /// In-Service Register — vectors currently being handled, cleared on `EOI`.
    /// Provides interrupt priority/nesting exactly as the hardware LAPIC (and the
    /// AArch64 GIC active-state) do.
    isr: [u64; 4],
}

impl Lapic {
    fn new() -> Self {
        Lapic {
            svr: 0xff, // disabled at reset; vector 0xff
            lvt_timer: 1 << 16,
            initial_count: 0,
            current_count: 0,
            divide: 1,
            tpr: 0,
            irr: [0; 4],
            isr: [0; 4],
        }
    }
    fn enabled(&self) -> bool {
        self.svr & (1 << 8) != 0
    }
    /// Set an Interrupt Request Register bit (a pending interrupt vector).
    fn set_irr(&mut self, vec: u8) {
        self.irr[(vec >> 6) as usize] |= 1u64 << (vec & 63);
    }
    /// The highest set vector in a 256-bit register, or `None`.
    fn highest(reg: &[u64; 4]) -> Option<u8> {
        for w in (0..4).rev() {
            if reg[w] != 0 {
                return Some((w as u32 * 64 + (63 - reg[w].leading_zeros())) as u8);
            }
        }
        None
    }
    /// The highest pending vector deliverable now: its priority class (`vec >> 4`)
    /// must exceed both the TPR class and any in-service vector's class.
    fn deliverable(&self) -> Option<u8> {
        let v = Self::highest(&self.irr)?;
        let isr_class = u32::from(Self::highest(&self.isr).map_or(0, |i| i >> 4));
        if u32::from(v >> 4) > isr_class && u32::from(v >> 4) > (self.tpr >> 4) {
            Some(v)
        } else {
            None
        }
    }
    /// Move a delivered vector IRR -> ISR (in-service until `EOI`).
    fn take(&mut self, vec: u8) {
        self.irr[(vec >> 6) as usize] &= !(1u64 << (vec & 63));
        self.isr[(vec >> 6) as usize] |= 1u64 << (vec & 63);
    }
    /// End-of-interrupt: clear the highest in-service vector.
    fn eoi(&mut self) {
        if let Some(v) = Self::highest(&self.isr) {
            self.isr[(v >> 6) as usize] &= !(1u64 << (v & 63));
        }
    }
}

// The `virtio-mmio` transport slots the x86-64 core exposes. They sit in a
// dedicated high MMIO window (above any guest RAM the boot core sizes), so a
// physical access there is unambiguously a device, never RAM — the x86-64
// analogue of QEMU `microvm`'s `virtio-mmio` region. Each slot is the standard
// `virtio-mmio` register block; the devices behind them are the shared
// [`devbus`](super::devbus), identical to the other two cores (Law L4). Only the
// transport (this window + the interrupt path) is per-ISA.
const VIRTIO_BLK_BASE: u64 = 0xD000_0000;
const VIRTIO_BLK_END: u64 = 0xD000_0200;
/// The second `virtio-mmio` slot — the VirtIO **9P** device (the shared
/// workspace filesystem, `CC-15`), serviced by the shared `devbus`.
const VIRTIO_9P_BASE: u64 = 0xD000_0200;
const VIRTIO_9P_END: u64 = 0xD000_0400;
/// The third `virtio-mmio` slot — the VirtIO **network** device (`CC-16`): the
/// userspace TCP/IP NAT, serviced by the shared `devbus`.
const VIRTIO_NET_BASE: u64 = 0xD000_0400;
const VIRTIO_NET_END: u64 = 0xD000_0600;
/// The fourth `virtio-mmio` slot — the VirtIO **input** device (`CC-46`): a
/// keyboard + relative pointer, serviced by the shared `devbus`. Delivering its
/// events wakes the X server's main loop (driving the shadow → scanout flush).
const VIRTIO_INPUT_BASE: u64 = 0xD000_0600;
const VIRTIO_INPUT_END: u64 = 0xD000_0800;

/// The base of guest RAM (a flat physical address space; the boot core runs with
/// paging off / identity-mapped until the kernel installs its own page tables).
const RAM_BASE: u64 = 0x0;

// The legacy ISA IRQ lines the `virtio-mmio` devices vector through (the values
// the kernel learns from the `virtio_mmio.device=<size>@<base>:<irq>` command
// line). They sit on the slave 8259 so the boot path's IRQ delivery reaches the
// device drivers (`CC-45`).
const VIRTIO_BLK_IRQ: u8 = 11;
const VIRTIO_9P_IRQ: u8 = 10;
const VIRTIO_NET_IRQ: u8 = 12;
// IRQ 5 — a clean, free legacy ISA line on the *master* 8259 (the PIT on IRQ 0
// already proves master-line delivery), avoiding the FPU/FERR line (13) and the
// legacy IDE lines (14/15). The kernel learns it from the `…:5` cmdline suffix.
const VIRTIO_INPUT_IRQ: u8 = 5;

/// Emulator-steps per PIT/APIC down-counter decrement — one shared rate for every
/// platform timer. With the PIT at its architectural 1.193182 MHz, this fixes the
/// guest-time granularity at `TICK_DIV × 1.193182` steps per microsecond, which is
/// what bounds the cost of a busy-wait delay (`udelay`/`delay_tsc` and absent-device
/// poll timeouts spin in emulator steps proportional to it). Kept small so those
/// delays — and a long device-probe timeout — do not burn billions of steps, while
/// still large enough that the tick period (`reload × TICK_DIV` steps) far exceeds
/// the tick handler (no "timer storm"). One coherent rate keeps the kernel's
/// calibration and its tick in step. (See [`TSC_PER_STEP`] for `cpu_khz`.)
const TICK_DIV: u64 = 16;

/// TSC ticks advanced per emulator step. With [`TICK_DIV`] this sets the frequency
/// the kernel calibrates: `cpu_khz = TSC_PER_STEP × TICK_DIV × 1.193182 MHz`
/// (≈ 2.44 GHz here — a realistic CPU, so `udelay`'s cycle budget is sane). The TSC
/// advances every step for fine `sched_clock` resolution; only the down-counters
/// are paced by `TICK_DIV`, so this factor sets `cpu_khz` independently of the
/// delay-cost granularity above.
const TSC_PER_STEP: u64 = 128;

/// Lines in the modelled L1 data cache (direct-mapped, 64-byte lines → 32 KiB).
/// Smaller than the working sets entropy daemons deliberately stride across (the
/// kernel's `jitterentropy` buffer is 64 KiB), so their walks miss and the access
/// latency varies with the address pattern.
const DCACHE_LINES: usize = 512;
/// Extra TSC cycles charged on a data-cache miss — the microarchitectural timing
/// variance a real CPU exhibits, and that `jitterentropy` harvests as its noise
/// source. Without it the (otherwise perfectly deterministic) emulator gives a
/// constant per-access latency: jitterentropy's health test sees no jitter and the
/// crypto DRBG it seeds never initialises (the boot wedges before userspace).
const DCACHE_MISS_CYCLES: u64 = 32;

/// A direct-mapped software TLB entry: caches a virtual-page → physical-frame
/// translation so a hot loop does not re-walk the 4-level page table on every
/// access. An entry is live when its `gen` matches [`Cpu::tlb_gen`] (the global
/// flush counter, bumped on a `CR0`/`CR4` write), its `pgen` matches
/// [`Cpu::pcid_gen`]`[pcid]` (the per-PCID flush counter, bumped on a flushing
/// `CR3` load / `INVLPG`), and its `pcid` matches the active PCID — so a `CR3`
/// switch between address spaces (the kernel's ASID scheme, and `text_poke`'s
/// poking-mm ping-pong) keeps every PCID's translations instead of cold-flushing.
#[derive(Clone, Copy)]
struct TlbEntry {
    tag: u64,
    frame: u64,
    pcid: u16,
    gen: u64,
    pgen: u64,
    /// Whether this page is writable (proven by a successful write-walk). A cached
    /// write to a non-writable entry re-walks so write-protection (CoW) is enforced.
    writable: bool,
}

/// Direct-mapped TLB sets, indexed by the virtual page number.
const TLB_SETS: usize = 1024;

/// The x86-64 long-mode integer core.
pub struct Cpu {
    /// The 16 general-purpose registers (`rax`,`rcx`,`rdx`,`rbx`,`rsp`,`rbp`,
    /// `rsi`,`rdi`,`r8`..`r15`).
    r: [u64; 16],
    /// The 16 SSE registers (`xmm0`..`xmm15`), 128-bit each. SSE2 is baseline-
    /// mandatory on x86-64, so the dynamic loader (`ld-musl`) and every real
    /// userland use them from their first instructions. (Not yet carried in the
    /// κ-snapshot — an integer/control snapshot resumes with XMM zeroed; extending
    /// the snapshot is a deliberate follow-up gated against the resume witnesses.)
    xmm: [u128; 16],
    /// The x87 FPU register stack — 8 registers held as `f64`. On x86-64 `long double`
    /// is 80-bit x87, and musl's number formatting (`fmt_fp`) does its digit extraction
    /// in x87 (`FILD`/`FISTP`/`FMUL`/…); without a real FPU those produce garbage and a
    /// formatting pointer walks off its buffer (a real Xorg crash). `f64` is enough for
    /// the double-range values these paths carry. `ftop` is the 3-bit TOP pointer
    /// (`st(i) = fpr[(ftop + i) & 7]`); `fcw`/`fsw` are the control + status words.
    fpr: [f64; 8],
    ftop: u8,
    fcw: u16,
    fsw: u16,
    /// The instruction pointer.
    rip: u64,
    /// The flags register (`RFLAGS`).
    rflags: u64,
    /// Retired-instruction counter — a monotonic count of guest instructions the
    /// core has executed (informational; the x86-64 analogue of the RISC-V
    /// `INSTRET` CSR).
    insns: u64,
    /// Control registers: `cr0` (paging/protection), `cr2` (page-fault address),
    /// `cr3` (the PML4 physical base), `cr4` (PAE et al.).
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    /// `IA32_EFER` — `LME`/`LMA` (long mode enabled/active) live here.
    efer: u64,
    /// The debug registers `DR0..DR7`. `cpu_init` zeroes them on every CPU bring-up
    /// (`MOV DRn, r` / `MOV r, DRn` — opcodes `0F 23` / `0F 21`); no hardware
    /// breakpoints are armed during the boot, so they are plain storage that reads
    /// back what was written (DR4/DR5 alias DR6/DR7 architecturally, immaterial here).
    dr: [u64; 8],
    /// The six segment registers (`ES,CS,SS,DS,FS,GS` — indexed by [`SegId`]).
    /// In long mode the bases are 0 except `FS`/`GS`; `CS.long` selects 64-bit.
    seg: [Seg; 6],
    /// The current privilege level (`CPL`, `CS.RPL`): 0 in the kernel, 3 in
    /// userspace — selects the syscall/interrupt stack switch.
    cpl: u8,
    /// The segment override in effect for the instruction being decoded (an `FS`
    /// (`0x64`) / `GS` (`0x65`) prefix), so a memory operand adds that segment's
    /// base — the kernel's per-CPU `%gs:` and the userspace `%fs:` accesses.
    /// Reset at the start of every instruction.
    cur_seg: Option<SegId>,
    /// Whether a `REX` prefix is present on the instruction being decoded. With
    /// no `REX`, a byte register operand `4..=7` selects the **high byte**
    /// (`AH`/`CH`/`DH`/`BH`); with `REX` it selects the low byte of
    /// `RSP`/`RBP`/`RSI`/`RDI` (`SPL`/`BPL`/`SIL`/`DIL`).
    rex_present: bool,
    /// Guest RAM (physical, based at [`RAM_BASE`]).
    ram: Vec<u8>,
    /// A page fault latched mid-instruction by the MMU (a not-present page-table
    /// walk while paging is on). The faulting linear address and the `#PF` error
    /// code; [`Cpu::step`] restores the pre-instruction register state and
    /// vectors `#PF` (the kernel's early `do_early_exception` → `early_make_pgtable`
    /// lazily maps boot data through `early_top_pgt` exactly as on real hardware).
    fault: Option<PageFault>,
    /// A direct-mapped software TLB over [`Cpu::translate_acc`] (see [`TlbEntry`]).
    tlb: Vec<TlbEntry>,
    tlb_gen: u64,
    /// Inline instruction-fetch translation cache. A single instruction decodes
    /// through several `fetch_u8` calls (prefixes, opcode, ModRM/SIB, displacement,
    /// immediate) — almost always on one code page — and every byte would otherwise
    /// re-run [`translate_acc`]'s TLB lookup. This caches the *current code page's*
    /// VA→frame translation so the bytes after the first are a direct RAM read. It is
    /// validated against `tlb_gen` + the active PCID's `pcid_gen` (so any TLB/paging
    /// flush invalidates it for free) and is purely a read-through accelerator over
    /// `translate_acc` — portable (no JIT/`std` dependency). `ifetch_gen == 0` (the
    /// reserved pre-flush generation, real generations start at 1) means "empty".
    ifetch_tag: u64,
    ifetch_frame: u64,
    ifetch_gen: u64,
    ifetch_pgen: u64,
    ifetch_pcid: u16,
    /// Per-PCID TLB generations (`CR4.PCIDE`). A `CR3` load without the no-flush
    /// hint bumps only the loaded PCID's generation, so the other address spaces'
    /// cached translations survive — the kernel reuses a handful of ASIDs and
    /// `text_poke` ping-pongs between the kernel mm and a temporary poking-mm.
    /// Indexed by the 12-bit PCID.
    pcid_gen: Vec<u64>,
    sys: Option<Box<Sys>>,
}

/// A page fault latched by the MMU during an instruction's memory access (`#12`):
/// the faulting linear address (→ `CR2`) and the architectural error code.
#[derive(Clone, Copy)]
struct PageFault {
    addr: u64,
    error: u64,
}

/// `#PF` (page fault) — IDT vector 14.
const VEC_PAGE_FAULT: u8 = 14;
/// `#PF` error-code bits: P (the fault was a protection violation, not
/// not-present), W/R (write), U/S (user-mode access).
const PF_ERR_PRESENT: u64 = 1 << 0;
const PF_ERR_WRITE: u64 = 1 << 1;
const PF_ERR_USER: u64 = 1 << 2;

// Register indices.
const RAX: usize = 0;
const RCX: usize = 1;
const RDX: usize = 2;
const RSP: usize = 4;
const RBP: usize = 5;
const RSI: usize = 6;
const RDI: usize = 7;

/// Segment-register indices (the SReg field encoding: ES,CS,SS,DS,FS,GS).
#[derive(Clone, Copy)]
enum SegId {
    Es = 0,
    Cs = 1,
    Ss = 2,
    Ds = 3,
    Fs = 4,
    Gs = 5,
}

/// `RFLAGS.IF` — the interrupt-enable flag (`STI`/`CLI`).
const RFLAGS_IF: u64 = 1 << 9;
/// `CR4.PCIDE` — process-context identifiers enable.
const CR4_PCIDE: u64 = 1 << 17;
/// `RFLAGS.DF` — the direction flag (string-op increment/decrement).
const RFLAGS_DF: u64 = 1 << 10;
/// `RFLAGS.AC` — the alignment-check / SMAP access flag. The kernel's `stac`/`clac`
/// bracket every `copy_to_user`/`get_user` with `AC=1`, so its page-fault handler can
/// tell a legitimate user access (`regs->flags & AC`) from a stray kernel access to
/// user memory: a `#PF` taken with `AC=0` on a user address is `page_fault_oops`
/// (fatal). Modelling `AC` is required for a faulting `copy_to_user` to be recoverable.
const RFLAGS_AC: u64 = 1 << 18;

// The model-specific registers the long-mode boot path drives.
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
const MSR_APIC_BASE: u32 = 0x1B;

// The local-APIC MMIO window (the architectural default base).
const LAPIC_BASE: u64 = 0xFEE0_0000;
const LAPIC_END: u64 = 0xFEE0_1000;

/// A little-endian read cursor over κ-snapshot bytes (the dual of the `*.snap` writers below);
/// every reader returns `None` on truncation so a malformed snapshot is rejected, not panics.
struct Snap<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Snap<'a> {
    fn new(b: &'a [u8]) -> Self {
        Snap { b, p: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let s = self.b.get(self.p..self.p + 2)?;
        self.p += 2;
        Some(u16::from_le_bytes(s.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        let s = self.b.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn flag(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    /// A length-prefixed byte blob (`u64` len + bytes).
    fn blob(&mut self) -> Option<Vec<u8>> {
        let n = self.u64()? as usize;
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(s.to_vec())
    }
    /// `n` raw bytes (no length prefix).
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(s)
    }
}

/// The 4 KiB page granularity κ-snapshots content-address guest RAM at.
const KAPPA_PAGE: usize = 0x1000;
/// A BLAKE3 κ-label is 71 ASCII bytes (`blake3:` + 64 hex).
const KAPPA_LABEL_LEN: usize = 71;
/// Snapshot **format version** — the trailing digit of both magics below.
///
/// **BUMP THIS whenever the serialized layout changes** — anything written by `Sys::snap`
/// (CPU + device state), the RAM-page manifest encoding, or the blob framing. The version is
/// baked into the magic, so a snapshot minted by an older layout then fails the magic check on
/// restore and returns `false` **loudly** — instead of silently deserializing a mismatched
/// layout into a dead machine (empty console, zero execution). That silent-wrong-restore cost a
/// full cc62 "hang" *and* days of a browser "malformed blob" chase this session (CC-62/CC-66);
/// a version tag turns both into an explicit "this .holo was built by another core — rebuild it."
// Version 3 adds the virtio-net device block to the device state (`VirtioNet::snap_regs`), so a
// warm-snapshotted NIC'd server resumes reachable (CC-73). Version 2 blobs (no net block) are still
// read — `restore_kappa_blob` accepts both and skips the net block for v2 — so existing `.holo`
// fixtures keep working without regeneration.
const SNAPSHOT_FORMAT_VERSION: u8 = b'3';
/// The immediately-prior format version still accepted on restore (its state lacks the virtio-net block).
const SNAPSHOT_FORMAT_VERSION_PREV: u8 = b'2';
/// Magic prefixing a serialized κ-snapshot manifest (last byte = [`SNAPSHOT_FORMAT_VERSION`]).
const KAPPA_MANIFEST_MAGIC: &[u8; 8] = b"HOLOKSN3";
/// Magic prefixing a self-contained κ-snapshot *blob* (last byte = [`SNAPSHOT_FORMAT_VERSION`]).
const KAPPA_BLOB_MAGIC: &[u8; 8] = b"HOLOKSB3";
const _: () = {
    // The magics' trailing byte MUST equal SNAPSHOT_FORMAT_VERSION — bump all three together.
    assert!(KAPPA_MANIFEST_MAGIC[7] == SNAPSHOT_FORMAT_VERSION);
    assert!(KAPPA_BLOB_MAGIC[7] == SNAPSHOT_FORMAT_VERSION);
};

/// A **content-addressed** machine snapshot (the κ-snapshot path): the small CPU + device
/// `state` inline, and guest RAM as a per-4 KiB-page BLAKE3 κ **manifest** (`ram_pages`) whose
/// *unique* pages live in a [`KappaStore`]. Post-boot RAM is overwhelmingly zero/duplicate, so
/// the store holds a tiny working set (measured 22.8× on a real boot: 1 GiB → 44 MiB unique).
/// A resume streams only the unique pages and verifies each before use (L5). Build with
/// [`Cpu::snapshot_kappa`], resume with [`Cpu::restore_kappa`].
pub struct KappaSnapshot {
    state: Vec<u8>,
    ram_pages: Vec<KappaLabel71>,
    ram_len: usize,
}
impl KappaSnapshot {
    /// The number of 4 KiB RAM pages (the manifest length).
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.ram_pages.len()
    }
    /// Total guest RAM the snapshot reconstructs.
    #[must_use]
    pub fn ram_len(&self) -> usize {
        self.ram_len
    }
    /// The serialized CPU + device state (everything except RAM).
    #[must_use]
    pub fn state_len(&self) -> usize {
        self.state.len()
    }

    /// The ordered per-page RAM κ-labels (the manifest). An adopter walks these to fetch each
    /// page by κ from a peer (deduping on its store — only the unique κ are pulled over the wire).
    #[must_use]
    pub fn page_kappas(&self) -> &[KappaLabel71] {
        &self.ram_pages
    }

    /// Serialize the manifest (CPU+device state + the ordered RAM-page κ list) to a deterministic
    /// blob. `put`ting this blob yields the **snapshot κ** — one content label that transitively
    /// names the whole machine (the state inline + every RAM page by κ). The unique pages live in
    /// the store; this blob is small (state + 71 B per page label).
    #[must_use]
    pub fn to_manifest_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(32 + self.state.len() + self.ram_pages.len() * KAPPA_LABEL_LEN);
        out.extend_from_slice(KAPPA_MANIFEST_MAGIC);
        out.extend_from_slice(&(self.ram_len as u64).to_le_bytes());
        out.extend_from_slice(&(self.state.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.state);
        out.extend_from_slice(&(self.ram_pages.len() as u64).to_le_bytes());
        for k in &self.ram_pages {
            out.extend_from_slice(k.as_array());
        }
        out
    }

    /// Parse a [`KappaSnapshot::to_manifest_bytes`] blob back into a manifest. Returns `None` on a
    /// bad magic or truncation. (The RAM pages themselves are fetched lazily by κ at resume.)
    #[must_use]
    pub fn from_manifest_bytes(b: &[u8]) -> Option<KappaSnapshot> {
        let mut s = Snap::new(b);
        if s.take(KAPPA_MANIFEST_MAGIC.len())? != KAPPA_MANIFEST_MAGIC.as_slice() {
            return None;
        }
        let ram_len = s.u64()? as usize;
        let state_len = s.u64()? as usize;
        let state = s.take(state_len)?.to_vec();
        let npages = s.u64()? as usize;
        let mut ram_pages = Vec::with_capacity(npages);
        for _ in 0..npages {
            let arr: [u8; KAPPA_LABEL_LEN] = s.take(KAPPA_LABEL_LEN)?.try_into().ok()?;
            ram_pages.push(KappaLabel71::from_bytes(&arr).ok()?);
        }
        Some(KappaSnapshot { state, ram_pages, ram_len })
    }
}

impl Seg {
    fn snap(&self, o: &mut Vec<u8>) {
        o.extend_from_slice(&self.selector.to_le_bytes());
        o.extend_from_slice(&self.base.to_le_bytes());
        o.push(self.long as u8);
    }
    fn unsnap(s: &mut Snap) -> Option<Seg> {
        Some(Seg { selector: s.u16()?, base: s.u64()?, long: s.flag()? })
    }
}
impl Uart {
    fn snap(&self, o: &mut Vec<u8>) {
        o.extend_from_slice(&(self.output.len() as u64).to_le_bytes());
        o.extend_from_slice(&self.output);
        o.extend_from_slice(&(self.input.len() as u64).to_le_bytes());
        o.extend_from_slice(&self.input);
        o.extend_from_slice(&(self.in_cursor as u64).to_le_bytes());
        o.extend_from_slice(&[self.lcr, self.ier, self.mcr, self.scratch, self.fcr]);
        o.extend_from_slice(&self.divisor.to_le_bytes());
        o.push(self.thre_pending as u8);
    }
    fn unsnap(s: &mut Snap) -> Option<Uart> {
        Some(Uart {
            output: s.blob()?,
            input: s.blob()?,
            in_cursor: s.u64()? as usize,
            lcr: s.u8()?,
            ier: s.u8()?,
            mcr: s.u8()?,
            scratch: s.u8()?,
            fcr: s.u8()?,
            divisor: s.u16()?,
            thre_pending: s.flag()?,
        })
    }
}
impl Pic {
    fn snap(&self, o: &mut Vec<u8>) {
        o.extend_from_slice(&self.mask.to_le_bytes());
        o.extend_from_slice(&self.request.to_le_bytes());
        o.extend_from_slice(&[self.base_master, self.base_slave, self.init_master, self.init_slave]);
    }
    fn unsnap(s: &mut Snap) -> Option<Pic> {
        Some(Pic {
            mask: s.u16()?,
            request: s.u16()?,
            base_master: s.u8()?,
            base_slave: s.u8()?,
            init_master: s.u8()?,
            init_slave: s.u8()?,
        })
    }
}
impl Pit {
    fn snap(&self, o: &mut Vec<u8>) {
        o.extend_from_slice(&self.reload.to_le_bytes());
        o.extend_from_slice(&self.counter.to_le_bytes());
        o.extend_from_slice(&[self.write_hi as u8, self.enabled as u8, self.ch0_periodic as u8]);
        o.extend_from_slice(&self.ch2_reload.to_le_bytes());
        o.extend_from_slice(&self.ch2_counter.to_le_bytes());
        o.extend_from_slice(&[self.ch2_write_hi as u8, self.ch2_gate as u8, self.ch2_out as u8]);
    }
    fn unsnap(s: &mut Snap) -> Option<Pit> {
        Some(Pit {
            reload: s.u16()?,
            counter: s.u32()?,
            write_hi: s.flag()?,
            enabled: s.flag()?,
            ch0_periodic: s.flag()?,
            ch2_reload: s.u16()?,
            ch2_counter: s.u32()?,
            ch2_write_hi: s.flag()?,
            ch2_gate: s.flag()?,
            ch2_out: s.flag()?,
        })
    }
}
impl Lapic {
    fn snap(&self, o: &mut Vec<u8>) {
        for v in [
            self.svr, self.lvt_timer, self.initial_count, self.current_count, self.divide, self.tpr,
        ] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.irr.iter().chain(self.isr.iter()) {
            o.extend_from_slice(&v.to_le_bytes());
        }
    }
    fn unsnap(s: &mut Snap) -> Option<Lapic> {
        let (svr, lvt_timer, initial_count, current_count, divide, tpr) =
            (s.u32()?, s.u32()?, s.u32()?, s.u32()?, s.u32()?, s.u32()?);
        let mut irr = [0u64; 4];
        let mut isr = [0u64; 4];
        for x in irr.iter_mut().chain(isr.iter_mut()) {
            *x = s.u64()?;
        }
        Some(Lapic { svr, lvt_timer, initial_count, current_count, divide, tpr, irr, isr })
    }
}
impl Sys {
    /// Serialize the plain-data device state (NO external handles — `virtio`/`9p`/`net`/
    /// `loopback` are `None` for the embedded-initramfs boot and are restored as `None`).
    fn snap(&self, o: &mut Vec<u8>, inline_disk: bool) {
        debug_assert!(
            self.virtio9p.is_none() && self.loopback.is_none(),
            "κ-snapshot of an attached 9p/loopback device is not yet supported (the disk AND virtio-net \
             are now serialized; extend Sys::snap/unsnap before snapshotting a 9p/loopback machine)",
        );
        // The virtio-blk κ-disk: present-flag + a length-prefixed, L5-verifiable sector manifest.
        // `inline_disk` = self-contained (blob); else streaming (index only, sectors fetched by κ).
        match &self.virtio {
            None => o.push(0),
            Some(blk) => {
                o.push(1);
                let mut d = Vec::new();
                blk.snap(&mut d, inline_disk);
                o.extend_from_slice(&(d.len() as u64).to_le_bytes());
                o.extend_from_slice(&d);
            }
        }
        // The virtio-input device: present-flag + its register state, so a resumed desktop keeps its
        // keyboard + pointer (the queue addresses live here, NOT in guest RAM, so a `None` restore
        // would leave the guest's bound driver pointing at a vanished device). In-flight `pending`
        // events are transient (sub-ms host input) and intentionally dropped.
        match &self.virtioinput {
            None => o.push(0),
            Some(d) => {
                o.push(1);
                o.extend_from_slice(&d.status.to_le_bytes());
                o.extend_from_slice(&d.device_features_sel.to_le_bytes());
                o.extend_from_slice(&d.driver_features_sel.to_le_bytes());
                for v in d.driver_features {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                o.extend_from_slice(&d.queue_sel.to_le_bytes());
                for v in d.queue_num {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                for v in d.queue_ready {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                for v in d.desc_addr {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                for v in d.avail_addr {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                for v in d.used_addr {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                for v in d.last_avail {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                o.extend_from_slice(&d.interrupt_status.to_le_bytes());
                o.push(d.cfg_select);
                o.push(d.cfg_subsel);
            }
        }
        // The virtio-net device: present-flag + its negotiated register state (queue addresses live in
        // guest RAM; `last_avail` is the device's consumed position). The external transports
        // (nat/egress/ingress) are re-attached fresh on resume — see `VirtioNet::snap_regs`.
        match &self.virtionet {
            None => o.push(0),
            Some(d) => {
                o.push(1);
                d.snap_regs(o);
            }
        }
        self.uart.snap(o);
        o.extend_from_slice(&self.idtr.0.to_le_bytes());
        o.extend_from_slice(&self.idtr.1.to_le_bytes());
        o.extend_from_slice(&self.gdtr.0.to_le_bytes());
        o.extend_from_slice(&self.gdtr.1.to_le_bytes());
        o.extend_from_slice(&self.tr_base.to_le_bytes());
        o.extend_from_slice(&(self.msr.len() as u64).to_le_bytes());
        for (k, v) in &self.msr {
            o.extend_from_slice(&k.to_le_bytes());
            o.extend_from_slice(&v.to_le_bytes());
        }
        self.pic.snap(o);
        self.pit.snap(o);
        self.lapic.snap(o);
        o.extend_from_slice(&self.tsc.to_le_bytes());
        o.push(self.halted as u8);
        o.extend_from_slice(&self.rng.to_le_bytes());
        o.extend_from_slice(&self.tdiv.to_le_bytes());
        o.extend_from_slice(&self.pci_addr.to_le_bytes());
        o.extend_from_slice(&(self.dcache.len() as u64).to_le_bytes());
        for v in &self.dcache {
            o.extend_from_slice(&v.to_le_bytes());
        }
    }
    fn unsnap(s: &mut Snap, has_net: bool) -> Option<Sys> {
        // The virtio-blk κ-disk (present-flag + length-prefixed manifest), written before the UART.
        let virtio = match s.u8()? {
            0 => None,
            _ => Some(super::VirtioBlk::unsnap(&s.blob()?)?),
        };
        let virtioinput = match s.u8()? {
            0 => None,
            _ => Some(super::VirtioInput {
                status: s.u32()?,
                device_features_sel: s.u32()?,
                driver_features_sel: s.u32()?,
                driver_features: [s.u32()?, s.u32()?],
                queue_sel: s.u32()?,
                queue_num: [s.u32()?, s.u32()?],
                queue_ready: [s.u32()?, s.u32()?],
                desc_addr: [s.u64()?, s.u64()?],
                avail_addr: [s.u64()?, s.u64()?],
                used_addr: [s.u64()?, s.u64()?],
                last_avail: [s.u16()?, s.u16()?],
                interrupt_status: s.u32()?,
                cfg_select: s.u8()?,
                cfg_subsel: s.u8()?,
                pending: alloc::collections::VecDeque::new(),
            }),
        };
        let virtionet = if !has_net {
            None // a v2 blob predates the virtio-net device block
        } else {
            match s.u8()? {
            0 => None,
            _ => Some(super::VirtioNet::from_snapshot(
                [s.u8()?, s.u8()?, s.u8()?, s.u8()?, s.u8()?, s.u8()?], // mac
                s.u32()?,                                              // status
                s.u32()?,                                              // device_features_sel
                s.u32()?,                                              // driver_features_sel
                [s.u32()?, s.u32()?],                                  // driver_features
                s.u32()?,                                              // queue_sel
                [s.u32()?, s.u32()?],                                  // queue_num
                [s.u32()?, s.u32()?],                                  // queue_ready
                [s.u64()?, s.u64()?],                                  // desc_addr
                [s.u64()?, s.u64()?],                                  // avail_addr
                [s.u64()?, s.u64()?],                                  // used_addr
                [s.u16()?, s.u16()?],                                  // last_avail
                s.u32()?,                                              // interrupt_status
            )),
            }
        };
        let uart = Uart::unsnap(s)?;
        let idtr = (s.u64()?, s.u16()?);
        let gdtr = (s.u64()?, s.u16()?);
        let tr_base = s.u64()?;
        let nmsr = s.u64()? as usize;
        let mut msr = BTreeMap::new();
        for _ in 0..nmsr {
            let (k, v) = (s.u32()?, s.u64()?);
            msr.insert(k, v);
        }
        let pic = Pic::unsnap(s)?;
        let pit = Pit::unsnap(s)?;
        let lapic = Lapic::unsnap(s)?;
        let tsc = s.u64()?;
        let halted = s.flag()?;
        let rng = s.u64()?;
        let tdiv = s.u64()?;
        let pci_addr = s.u32()?;
        let ndc = s.u64()? as usize;
        let mut dcache = Vec::with_capacity(ndc);
        for _ in 0..ndc {
            dcache.push(s.u64()?);
        }
        Some(Sys {
            uart,
            virtio,
            virtio9p: None,
            virtionet,
            virtioinput,
            loopback: None,
            idtr,
            gdtr,
            tr_base,
            msr,
            pic,
            pit,
            lapic,
            tsc,
            halted,
            rng,
            tdiv,
            pci_addr,
            dcache,
        })
    }
}

impl Cpu {
    /// A fresh core with `ram_bytes` of zeroed RAM and `rip`/`rsp` reset.
    #[must_use]
    pub fn new(ram_bytes: usize) -> Self {
        Cpu {
            r: [0; 16],
            xmm: [0; 16],
            fpr: [0.0; 8],
            ftop: 0,
            fcw: 0x037f,
            fsw: 0,
            rip: RAM_BASE,
            rflags: 0x2, // bit 1 is reserved-1
            insns: 0,
            dr: [0; 8],
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            seg: [Seg::default(); 6],
            cpl: 0,
            cur_seg: None,
            rex_present: false,
            ram: vec![0u8; ram_bytes],
            fault: None,
            tlb: vec![
                TlbEntry {
                    tag: 0,
                    frame: 0,
                    pcid: 0,
                    gen: 0,
                    pgen: 0,
                    writable: false,
                };
                TLB_SETS
            ],
            tlb_gen: 1,
            ifetch_tag: 0,
            ifetch_frame: 0,
            ifetch_gen: 0,
            ifetch_pgen: 0,
            ifetch_pcid: 0,
            pcid_gen: vec![1; 4096],
            sys: Some(Box::new(Sys::new())),
        }
    }

    /// Whether 4-level paging is active (long mode: `CR0.PG` & `CR4.PAE` &
    /// `EFER.LME`). When off, virtual addresses are physical (the boot core runs
    /// identity-mapped until the kernel installs `CR3`).
    fn paging(&self) -> bool {
        const PG: u64 = 1 << 31;
        const PAE: u64 = 1 << 5;
        const LME: u64 = 1 << 8;
        self.cr0 & PG != 0 && self.cr4 & PAE != 0 && self.efer & LME != 0
    }

    /// Translate a linear address to a physical one through the 4-level page
    /// tables (`PML4 → PDPT → PD → PT`), honouring 2 MiB and 1 GiB large pages.
    /// Returns the linear address unchanged when paging is off; a not-present
    /// entry falls back to the linear address. A non-faulting view for tooling
    /// and tests ([`Cpu::vv_dbg`], the paging unit test); the executing core
    /// translates through [`Cpu::translate_acc`], which delivers a `#PF` instead.
    fn translate(&self, vaddr: u64) -> u64 {
        self.walk(vaddr, false, false).unwrap_or(vaddr)
    }

    /// Walk the 4-level page tables for `vaddr`, returning the physical address or
    /// the `#PF` error code for the first not-present level (the page-fault path a
    /// real long-mode boot takes — the kernel maps boot data lazily on `#PF`).
    /// `write`/`user` shape the error code; when paging is off the address is
    /// physical (the identity-mapped boot core before it installs `CR3`).
    fn walk(&self, vaddr: u64, write: bool, user: bool) -> Result<u64, u64> {
        if !self.paging() {
            return Ok(vaddr);
        }
        let np = if write { PF_ERR_WRITE } else { 0 } | if user { PF_ERR_USER } else { 0 };
        let pml4 = self.cr3 & 0x000f_ffff_ffff_f000;
        let idx = |lvl: u32| ((vaddr >> (12 + 9 * lvl)) & 0x1ff) * 8;
        let ent = |base: u64, i: u64| self.rd_phys(base + i, 8);
        let present = |e: u64| e & 1 != 0;
        let next = |e: u64| e & 0x000f_ffff_ffff_f000;
        let rw = |e: u64| e & 2 != 0; // the page is writable only if RW=1 at every level
        // A present page: enforce write-protection (the copy-on-write trigger). A write
        // to a read-only page faults when CPL=3, or when CPL=0 with CR0.WP set (Linux
        // runs WP=1) — so `do_wp_page` copies the page instead of the write silently
        // succeeding (which would break fork's CoW: parent + child sharing memory).
        let perm = |pa: u64, writable: bool| -> Result<u64, u64> {
            let wp = self.cr0 & (1 << 16) != 0;
            if write && !writable && (user || wp) {
                return Err(PF_ERR_PRESENT | PF_ERR_WRITE | if user { PF_ERR_USER } else { 0 });
            }
            Ok(pa)
        };

        let e4 = ent(pml4, idx(3));
        if !present(e4) {
            return Err(np);
        }
        let mut writable = rw(e4);
        let e3 = ent(next(e4), idx(2));
        if !present(e3) {
            return Err(np);
        }
        writable &= rw(e3);
        if e3 & (1 << 7) != 0 {
            // 1 GiB page
            return perm((e3 & 0x000f_ffff_c000_0000) | (vaddr & 0x3fff_ffff), writable);
        }
        let e2 = ent(next(e3), idx(1));
        if !present(e2) {
            return Err(np);
        }
        writable &= rw(e2);
        if e2 & (1 << 7) != 0 {
            // 2 MiB page
            return perm((e2 & 0x000f_ffff_ffe0_0000) | (vaddr & 0x1f_ffff), writable);
        }
        let e1 = ent(next(e2), idx(0));
        if !present(e1) {
            return Err(np);
        }
        writable &= rw(e1);
        #[cfg(feature = "cc44-trace")]
        if TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) && vaddr < 0x0001_0000_0000_0000 {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] WALK va={vaddr:#x} cr3={:#x} pml4={pml4:#x} i4={:#x} i3={:#x} i2={:#x} i1={:#x} e4={e4:#x} e3={e3:#x} e2={e2:#x} e1={e1:#x}",
                self.cr3, idx(3), idx(2), idx(1), idx(0),
            );
        }
        perm((e1 & 0x000f_ffff_ffff_f000) | (vaddr & 0xfff), writable)
    }

    /// Translate a linear address for the executing core, latching a [`PageFault`]
    /// (the first level that was not-present) when the walk fails so [`Cpu::step`]
    /// can roll the instruction back and vector `#PF`. Until the fault is taken,
    /// returns a benign physical address (`0`) so the in-flight access reads/writes
    /// harmlessly; the instruction is discarded and restarted after the handler
    /// maps the page. `write` and `user` set the error-code bits.
    ///
    /// Physical `0` is deliberately below every device MMIO window
    /// (`VIRTIO_BLK_BASE` = `0xD000_0000`, the local APIC at `0xFEE0_0000`), so a
    /// faulting access never resolves to a device — `rd`/`wr` take no MMIO side
    /// effect on a fault, only a harmless phys-0 scratch read/write that the
    /// instruction restart overwrites. This phys-0 scratch is load-bearing for the
    /// early boot's demand-paging: removing it (returning early from `rd`/`wr`)
    /// regresses the boot to a hang, so the scratch access is kept, not elided.
    fn translate_acc(&mut self, vaddr: u64, write: bool, user: bool) -> u64 {
        if self.fault.is_some() {
            return 0; // a fault is already pending; do not double-latch
        }
        // Fast path: the software TLB caches present translations so a hot loop
        // does not re-walk the 4-level page table on every access (the dominant
        // interpreter cost). It holds only successful (present) walks; a miss or a
        // not-present page falls through to the full walk below.
        let paging = self.paging();
        let pcid = self.active_pcid();
        // The cache holds a `writable` bit: a read/fetch always hits, but a write hits
        // only when the page is known-writable — otherwise it re-walks so write-
        // protection (the CoW trigger) is enforced. Without this, a write to a
        // read-only (COW'd) page would silently succeed and break fork's CoW.
        if paging && Self::tlb_fastpath() {
            let page = vaddr & !0xfff;
            let set = (page >> 12) as usize & (TLB_SETS - 1);
            let e = self.tlb[set];
            if e.gen == self.tlb_gen
                && e.pcid as usize == pcid
                && e.pgen == self.pcid_gen[pcid]
                && e.tag == page
                && (!write || e.writable)
            {
                return e.frame | (vaddr & 0xfff);
            }
        }
        match self.walk(vaddr, write, user) {
            Ok(pa) => {
                if paging {
                    let page = vaddr & !0xfff;
                    let set = (page >> 12) as usize & (TLB_SETS - 1);
                    self.tlb[set] = TlbEntry {
                        tag: page,
                        frame: pa & !0xfff,
                        pcid: pcid as u16,
                        gen: self.tlb_gen,
                        pgen: self.pcid_gen[pcid],
                        // A successful write-walk proves the page is writable; a read-walk
                        // leaves it conservatively non-writable (the next write re-walks).
                        writable: write,
                    };
                }
                pa
            }
            Err(mut error) => {
                if write {
                    error |= PF_ERR_WRITE;
                }
                if user {
                    error |= PF_ERR_USER;
                }
                self.fault = Some(PageFault { addr: vaddr, error });
                0
            }
        }
    }

    /// Flush the whole software TLB (bump the global generation) — the architected
    /// flush on a `CR0`/`CR4` write (a change of paging regime).
    fn flush_tlb(&mut self) {
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
    }

    /// Debug knob: `HOLO_NO_TLB=1` disables the software TLB + ifetch fast paths
    /// (every access re-walks the page table). Used to isolate a stale-translation
    /// bug from a walk/store bug. Cached from the env on first use; one relaxed
    /// atomic load on the hot path otherwise.
    #[inline]
    fn tlb_fastpath() -> bool {
        use std::sync::atomic::{AtomicU8, Ordering};
        static V: AtomicU8 = AtomicU8::new(0);
        match V.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on = std::env::var("HOLO_NO_TLB").map(|v| v == "1").unwrap_or(false);
                V.store(if on { 2 } else { 1 }, Ordering::Relaxed);
                !on
            }
        }
    }

    /// Whether to record the per-userspace-instruction boot diagnostic (`USER_INSNS`/`USER_PAGES`).
    /// OFF by default: the `USER_PAGES` HashMap insert ran on EVERY CPL=3 instruction — pure overhead
    /// for a real userland workload (a GUI desktop is almost all userspace). Tests that read the
    /// counters set `HOLO_BOOT_DIAG=1`. Cached from env on first use (one relaxed load on the hot path).
    #[inline]
    fn boot_diag() -> bool {
        use std::sync::atomic::{AtomicU8, Ordering};
        static V: AtomicU8 = AtomicU8::new(0);
        match V.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on = std::env::var("HOLO_BOOT_DIAG").map(|v| v == "1").unwrap_or(false);
                V.store(if on { 1 } else { 2 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// Flush the software TLB entries for a single PCID — a `CR3` load without the
    /// no-flush hint, or `INVLPG` for the active address space.
    fn flush_pcid(&mut self, pcid: usize) {
        self.pcid_gen[pcid] = self.pcid_gen[pcid].wrapping_add(1);
    }

    /// The active PCID: `CR3[11:0]` when `CR4.PCIDE` is enabled, else 0.
    fn active_pcid(&self) -> usize {
        if self.cr4 & CR4_PCIDE != 0 {
            (self.cr3 & 0xfff) as usize
        } else {
            0
        }
    }

    /// Account a RAM access in the modelled L1 data cache: a miss installs the line
    /// (evicting its set) and adds [`DCACHE_MISS_CYCLES`] to the TSC, so the access
    /// latency varies with the address pattern — real microarchitectural jitter.
    fn dcache_touch(&mut self, pa: u64) {
        let line = pa >> 6;
        let idx = (line as usize) & (DCACHE_LINES - 1);
        if let Some(sys) = self.sys.as_mut() {
            if sys.dcache[idx] != line {
                sys.dcache[idx] = line;
                sys.tsc = sys.tsc.wrapping_add(DCACHE_MISS_CYCLES);
            }
        }
    }

    /// Read control register `idx` (0/2/3/4; others read 0).
    fn cr(&self, idx: usize) -> u64 {
        match idx {
            0 => self.cr0,
            2 => self.cr2,
            3 => self.cr3,
            4 => self.cr4,
            _ => 0,
        }
    }

    /// Write control register `idx`.
    fn set_cr(&mut self, idx: usize, val: u64) {
        #[cfg(feature = "cc44-trace")]
        if idx == 3 && self.cr3 != val && TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] CR3-SWITCH {:#x} -> {val:#x} rip={:#x}",
                self.cr3,
                self.rip,
            );
        }
        match idx {
            0 => self.cr0 = val,
            2 => self.cr2 = val,
            // Bit 63 of a CR3 load is the transient no-flush hint, not retained.
            3 => self.cr3 = val & !(1u64 << 63),
            4 => self.cr4 = val,
            _ => {}
        }
        match idx {
            // CR0 (PG/WP) and CR4 (PAE/PGE/PCIDE/LA57) change the paging regime.
            0 | 4 => self.flush_tlb(),
            // CR3: with PCID a no-flush load (bit 63) keeps every PCID's entries;
            // otherwise it flushes only the PCID being loaded. Without PCIDE a CR3
            // load flushes the whole TLB, as on real hardware.
            3 => {
                if self.cr4 & CR4_PCIDE != 0 {
                    if val & (1u64 << 63) == 0 {
                        self.flush_pcid((val & 0xfff) as usize);
                    }
                } else {
                    self.flush_tlb();
                }
            }
            _ => {}
        }
    }

    /// A raw physical read (the page-table walk reads physical memory directly).
    fn rd_phys(&self, addr: u64, size: u8) -> u64 {
        let a = addr as usize;
        let mut v = 0u64;
        for i in 0..size as usize {
            v |= u64::from(*self.ram.get(a + i).unwrap_or(&0)) << (8 * i);
        }
        v
    }

    /// Read `size` bytes from a *linear* (virtual) address, walking the page
    /// tables — the descriptor-table reads (`IDT`, `TSS`) the CPU does on its own
    /// behalf during interrupt delivery. The `IDTR`/`GDTR`/`TR` bases the kernel
    /// loads are kernel virtual addresses (e.g. `0xffffffff8347e000`), so reading
    /// them as raw physical offsets would return zero and vector every fault to
    /// `RIP 0`. A non-faulting walk (the CPU's own table access never page-faults
    /// against the kernel's permanently-mapped descriptor tables).
    fn rd_virt(&self, vaddr: u64, size: u8) -> u64 {
        let pa = self.sys.as_ref().map_or(vaddr, |_| self.translate(vaddr));
        self.rd_phys(pa, size)
    }

    /// Load a flat code/image blob at guest physical `addr` and set `rip` to it —
    /// the instruction-conformance entry (the analogue of loading a riscv-test).
    pub fn load_at(&mut self, addr: u64, image: &[u8]) {
        let a = addr as usize;
        let n = image.len().min(self.ram.len().saturating_sub(a));
        self.ram[a..a + n].copy_from_slice(&image[..n]);
        self.rip = addr;
        self.r[RSP] = self.ram.len() as u64; // a stack at the top of RAM
    }

    /// The console bytes the guest has written to the serial port.
    #[must_use]
    pub fn console(&self) -> &[u8] {
        self.sys.as_ref().map_or(&[], |s| &s.uart.output)
    }

    /// Read `n` bytes of guest *virtual* memory at `vaddr` (translating through the current page
    /// tables). A boot diagnostic: dump the opcode bytes at a faulting rip so an unimplemented
    /// instruction is identified by its ACTUAL (post-alternatives-patch) bytes, not the static image.
    #[must_use]
    pub fn peek_code(&self, vaddr: u64, n: usize) -> Vec<u8> {
        (0..n as u64).map(|i| self.rd_virt(vaddr.wrapping_add(i), 1) as u8).collect()
    }

    // ── linear framebuffer (the graphical scanout) ───────────────────────────
    // A passive RGBA framebuffer reserved at the TOP of guest RAM — the x86 twin of the aarch64
    // `simple-framebuffer`. The guest's `efifb`/`simpledrm` driver, pointed here by the boot
    // protocol's `screen_info` (an EFI/VESA linear framebuffer) + an e820 reservation, writes pixels;
    // the host reads them out for the κ render stack (super-res + tile-κ). NO per-frame device logic —
    // it is just owned RAM the guest draws into. x86 RAM is based at physical 0, so the FB's guest
    // physical base is simply `ram.len() - FB_SIZE`. (virtio-gpu is the later phase for dynamic modes.)
    /// Framebuffer width in pixels.
    pub const FB_W: usize = 1280;
    /// Framebuffer height in pixels.
    pub const FB_H: usize = 800;
    /// Framebuffer byte length (`FB_W * FB_H * 4`, RGBA8888 / XRGB little-endian).
    pub const FB_SIZE: usize = Self::FB_W * Self::FB_H * 4;

    /// Guest-physical base of the framebuffer — what `screen_info.lfb_base` advertises and the e820
    /// reservation protects. (x86 RAM starts at physical 0, so this is `ram.len() - FB_SIZE`.)
    #[must_use]
    pub fn fb_phys_base(&self) -> u64 {
        self.ram.len().saturating_sub(Self::FB_SIZE) as u64
    }

    /// Read the framebuffer scanout (RGBA, `FB_W`×`FB_H`) — the surface the κ render stack projects.
    #[must_use]
    pub fn read_framebuffer(&self) -> Vec<u8> {
        let s = self.ram.len().saturating_sub(Self::FB_SIZE);
        self.ram[s..].to_vec()
    }

    /// Write the framebuffer region — a host-side push, or the smoke test in lieu of a graphical kernel.
    pub fn write_framebuffer(&mut self, bytes: &[u8]) {
        let s = self.ram.len().saturating_sub(Self::FB_SIZE);
        let n = bytes.len().min(self.ram.len() - s);
        self.ram[s..s + n].copy_from_slice(&bytes[..n]);
    }

    /// The guest's physical RAM (read-only) — for the κ-snapshot path: content-address it by
    /// 4 KiB page so a booted machine's mostly-zero/duplicate RAM deduplicates to a small
    /// working set (the dual of [`Cpu::run`]'s `ram`).
    #[must_use]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Serialize the core architectural CPU state + guest RAM into a deterministic snapshot
    /// (Law L1: identical machine → identical bytes → identical κ). The GP/instruction/flags
    /// registers, control registers, `EFER`, the debug registers, and RAM. (Segments, CPL,
    /// and the device `Sys` state are the remaining fields for a full boot-resume; the
    /// TLB/`ifetch` caches are intentionally NOT serialized — a resume rebuilds them lazily.)
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = self.snapshot_state(true);
        out.reserve(self.ram.len() + 8);
        out.extend_from_slice(&(self.ram.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.ram);
        out
    }

    /// The CPU + device state — everything in [`Cpu::snapshot`] **except** guest RAM. Shared by
    /// the flat [`Cpu::snapshot`] (which appends RAM as a blob) and [`Cpu::snapshot_kappa`]
    /// (which content-addresses RAM into a κ page manifest instead).
    fn snapshot_state(&self, inline_disk: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        for v in self.r {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            self.rip, self.rflags, self.insns, self.cr0, self.cr2, self.cr3, self.cr4, self.efer,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.dr {
            out.extend_from_slice(&v.to_le_bytes());
        }
        // The SSE register file (xmm0..15) — so a machine snapshotted mid-SSE (any real
        // userland: ld-musl, crypto, memcpy) resumes bit-exact instead of with XMM zeroed.
        for v in self.xmm {
            out.extend_from_slice(&v.to_le_bytes());
        }
        // The x87 FPU register file + control/status — REQUIRED for a correct resume: the kernel's
        // FXSAVE/FXRSTOR on a thread context switch saves/restores this, so a multithreaded desktop
        // resumed with x87 zeroed computes garbage (the same class of bug as the FXSAVE NOP). `fpr`
        // is the 8 stack slots (f64 bits), `ftop` the 3-bit TOP, `fcw`/`fsw` the control/status words.
        for v in self.fpr {
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        out.push(self.ftop);
        out.extend_from_slice(&self.fcw.to_le_bytes());
        out.extend_from_slice(&self.fsw.to_le_bytes());
        for sg in &self.seg {
            sg.snap(&mut out);
        }
        out.push(self.cpl);
        match &self.sys {
            Some(sys) => {
                out.push(1);
                sys.snap(&mut out, inline_disk);
            }
            None => out.push(0),
        }
        out
    }

    /// Content-address the machine into a [`KappaSnapshot`] (the κ-snapshot path): serialize the
    /// CPU + device state, split guest RAM into 4 KiB pages, and `put` each into `store` keyed by
    /// BLAKE3 — identical/zero pages dedup to one κ automatically (idempotent `put`). The returned
    /// manifest is a `Vec<κ>`; the store ends up holding only the *unique* pages. Returns `None`
    /// if a store write fails.
    pub fn snapshot_kappa(&self, store: &dyn KappaStore) -> Option<KappaSnapshot> {
        let state = self.snapshot_state(true);
        let mut ram_pages = Vec::with_capacity(self.ram.len() / KAPPA_PAGE + 1);
        for page in self.ram.chunks(KAPPA_PAGE) {
            ram_pages.push(store.put("blake3", page).ok()?);
        }
        Some(KappaSnapshot { state, ram_pages, ram_len: self.ram.len() })
    }

    /// Restore the core CPU state + RAM from a [`Cpu::snapshot`], resetting the TLB/`ifetch`
    /// caches (they rebuild lazily). Returns `false` if the bytes are truncated/malformed.
    pub fn restore(&mut self, snap: &[u8]) -> bool {
        let mut s = Snap::new(snap);
        // The flat snapshot has no version header; it is always same-session (current format).
        if self.restore_state(&mut s, true).is_none() {
            return false;
        }
        match s.blob() {
            Some(ram) => {
                self.ram = ram;
                self.flush_caches_after_restore();
                true
            }
            None => false,
        }
    }

    /// Resume a [`KappaSnapshot`]: restore the CPU + device state, then reconstruct guest RAM by
    /// fetching each page's κ from `store` and **verifying it before use** (L5 — re-derive the
    /// BLAKE3 digest and compare; a tampered/wrong page is refused). The cost is bounded by the
    /// *unique* pages fetched, not nominal RAM — boot once, resume in seconds. Returns `false` on
    /// malformed state, a missing page, or a κ verification failure.
    pub fn restore_kappa(&mut self, snap: &KappaSnapshot, store: &dyn KappaStore) -> bool {
        // The manifest magic (checked in `from_manifest_bytes`) is current-version only ⇒ has the net block.
        if self.restore_state(&mut Snap::new(&snap.state), true).is_none() {
            return false;
        }
        let mut ram = vec![0u8; snap.ram_len];
        for (i, k) in snap.ram_pages.iter().enumerate() {
            let bytes = match store.get(k) {
                Ok(Some(b)) => b,
                _ => return false,
            };
            if !matches!(verify_kappa(&bytes, k), Ok(true)) {
                return false; // L5: never trust a fetched page without re-deriving its κ
            }
            let off = i * KAPPA_PAGE;
            let end = (off + bytes.len()).min(ram.len());
            ram[off..end].copy_from_slice(&bytes[..end - off]);
        }
        self.ram = ram;
        self.flush_caches_after_restore();
        true
    }

    /// Seal the whole machine into ONE content κ: content-address RAM (pages → `store`), then
    /// `put` the manifest (CPU+device state + the page-κ list) itself → the returned **snapshot
    /// κ** transitively names the entire machine. An adopter that has only this κ + access to the
    /// store (locally or over `content_net`) can [`resume_kappa`](Cpu::resume_kappa) it. Returns
    /// `None` if a store write fails.
    pub fn seal_kappa(&self, store: &dyn KappaStore) -> Option<KappaLabel71> {
        let snap = self.snapshot_kappa(store)?;
        store.put("blake3", &snap.to_manifest_bytes()).ok()
    }

    /// Resume from a sealed snapshot κ ([`Cpu::seal_kappa`]): fetch the manifest by κ and
    /// **verify it before use** (L5), parse it, then [`restore_kappa`](Cpu::restore_kappa) (which
    /// fetches + verifies each RAM page). The adopter needs ONLY the snapshot κ + a store — the
    /// core "boot once, resume anywhere" entry point. Returns `false` on a missing/tampered
    /// manifest, a missing/tampered page, or malformed state.
    pub fn resume_kappa(&mut self, snapshot_kappa: &KappaLabel71, store: &dyn KappaStore) -> bool {
        let manifest = match store.get(snapshot_kappa) {
            Ok(Some(b)) => b,
            _ => return false,
        };
        if !matches!(verify_kappa(&manifest, snapshot_kappa), Ok(true)) {
            return false; // L5: the manifest blob must hash to its claimed κ
        }
        match KappaSnapshot::from_manifest_bytes(&manifest) {
            Some(snap) => self.restore_kappa(&snap, store),
            None => false,
        }
    }

    /// Serialize a **self-contained, content-addressed** snapshot blob: the manifest (CPU+device
    /// state + the per-page κ list) plus each UNIQUE 4 KiB page's bytes exactly once. Zero and
    /// duplicate pages collapse to a single copy, so the blob is far smaller than the flat
    /// [`Cpu::snapshot`] yet needs no external store to resume — the browser persists it to OPFS
    /// (and may gzip it further) so a fresh tab resumes from only the unique pages, not the
    /// nominal RAM. The dual of [`Cpu::restore_kappa_blob`].
    #[must_use]
    pub fn snapshot_kappa_blob(&self) -> Vec<u8> {
        let state = self.snapshot_state(true);
        let mut labels: Vec<KappaLabel71> = Vec::with_capacity(self.ram.len() / KAPPA_PAGE + 1);
        let mut seen: BTreeMap<[u8; KAPPA_LABEL_LEN], ()> = BTreeMap::new();
        let mut unique: Vec<(KappaLabel71, &[u8])> = Vec::new();
        for page in self.ram.chunks(KAPPA_PAGE) {
            let k = address_bytes(page);
            if seen.insert(*k.as_array(), ()).is_none() {
                unique.push((k, page));
            }
            labels.push(k);
        }
        let mut out = Vec::with_capacity(
            32 + state.len() + labels.len() * KAPPA_LABEL_LEN + unique.len() * (KAPPA_PAGE + 80),
        );
        out.extend_from_slice(KAPPA_BLOB_MAGIC);
        out.extend_from_slice(&(self.ram.len() as u64).to_le_bytes());
        out.extend_from_slice(&(state.len() as u64).to_le_bytes());
        out.extend_from_slice(&state);
        out.extend_from_slice(&(labels.len() as u64).to_le_bytes());
        for k in &labels {
            out.extend_from_slice(k.as_array());
        }
        out.extend_from_slice(&(unique.len() as u64).to_le_bytes());
        for (k, bytes) in &unique {
            out.extend_from_slice(k.as_array());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        out
    }

    /// Classify a κ-snapshot *blob*'s framing **without touching the CPU** — so a caller can tell a
    /// version mismatch ("rebuild this .holo, it was minted by another core") apart from actual
    /// corruption, and surface the right message instead of a silent dead machine. Returns
    /// `Ok(())` if the magic matches this core's [`SNAPSHOT_FORMAT_VERSION`]; `Err(Some(v))` with
    /// the blob's format version if it's a well-formed κ-blob of a *different* version;
    /// `Err(None)` if it isn't a κ-blob at all (too short / wrong prefix).
    pub fn classify_kappa_blob(blob: &[u8]) -> Result<(), Option<u8>> {
        if blob.len() < KAPPA_BLOB_MAGIC.len() {
            return Err(None);
        }
        let magic = &blob[..KAPPA_BLOB_MAGIC.len()];
        if magic == KAPPA_BLOB_MAGIC.as_slice() {
            return Ok(());
        }
        // Same "HOLOKSB" family, different trailing version byte → an older/newer format.
        if magic[..7] == KAPPA_BLOB_MAGIC[..7] {
            Err(Some(magic[7]))
        } else {
            Err(None)
        }
    }

    /// Resume from a [`Cpu::snapshot_kappa_blob`]: rebuild guest RAM from the bundled unique pages,
    /// **verifying each before use** (L5 — re-derive its BLAKE3 κ; refuse a tampered blob), then
    /// restore CPU+device state. Returns `false` on a bad/version-mismatched magic, truncation, a κ
    /// mismatch, or a page the manifest references but the blob omits. To distinguish a
    /// version mismatch from corruption before resuming, call [`Cpu::classify_kappa_blob`].
    pub fn restore_kappa_blob(&mut self, blob: &[u8]) -> bool {
        self.restore_kappa_blob_inner(blob).is_some()
    }
    fn restore_kappa_blob_inner(&mut self, blob: &[u8]) -> Option<()> {
        let mut s = Snap::new(blob);
        // Accept the current format AND the immediately-prior one (whose state lacks the virtio-net
        // block): same 7-byte family prefix, version byte = current or prev. `has_net` gates the net
        // read so old fixtures restore unchanged — the rest still fails loud (CC-72).
        let magic = s.take(KAPPA_BLOB_MAGIC.len())?;
        if magic[..7] != KAPPA_BLOB_MAGIC[..7] {
            return None;
        }
        let has_net = match magic[7] {
            v if v == SNAPSHOT_FORMAT_VERSION => true,
            v if v == SNAPSHOT_FORMAT_VERSION_PREV => false,
            _ => return None,
        };
        let ram_len = s.u64()? as usize;
        let state_len = s.u64()? as usize;
        let state = s.take(state_len)?.to_vec();
        let n_pages = s.u64()? as usize;
        let mut labels = Vec::with_capacity(n_pages);
        for _ in 0..n_pages {
            let arr: [u8; KAPPA_LABEL_LEN] = s.take(KAPPA_LABEL_LEN)?.try_into().ok()?;
            labels.push(arr);
        }
        let n_unique = s.u64()? as usize;
        let mut pages: BTreeMap<[u8; KAPPA_LABEL_LEN], Vec<u8>> = BTreeMap::new();
        for _ in 0..n_unique {
            let arr: [u8; KAPPA_LABEL_LEN] = s.take(KAPPA_LABEL_LEN)?.try_into().ok()?;
            let len = s.u32()? as usize;
            let bytes = s.take(len)?;
            let k = KappaLabel71::from_bytes(&arr).ok()?;
            if !matches!(verify_kappa(bytes, &k), Ok(true)) {
                return None; // L5: a page that doesn't re-derive to its κ is refused
            }
            pages.insert(arr, bytes.to_vec());
        }
        let mut ram = vec![0u8; ram_len];
        for (i, label) in labels.iter().enumerate() {
            let bytes = pages.get(label)?;
            let off = i * KAPPA_PAGE;
            let end = (off + bytes.len()).min(ram.len());
            ram[off..end].copy_from_slice(&bytes[..end - off]);
        }
        self.restore_state(&mut Snap::new(&state), has_net)?;
        self.ram = ram;
        self.flush_caches_after_restore();
        Some(())
    }

    /// The κ page-manifest alone (CPU+device state + the per-page BLAKE3 κ list) — no page bytes.
    /// A peer publishes this (small) and serves the unique pages by κ; an adopter walks it and
    /// streams each page on demand via [`Cpu::restore_kappa_streaming`].
    #[must_use]
    pub fn snapshot_kappa_manifest(&self) -> Vec<u8> {
        let ram_pages = self.ram.chunks(KAPPA_PAGE).map(address_bytes).collect();
        KappaSnapshot { state: self.snapshot_state(true), ram_pages, ram_len: self.ram.len() }
            .to_manifest_bytes()
    }

    /// As [`Cpu::snapshot_kappa_manifest`] but with the κ-disk STREAMED: the manifest carries only
    /// the per-sector κ index (not the ~10 MiB of sectors), so it stays light. The unique sectors —
    /// from [`Cpu::disk_unique_sectors`] — are published to the transport alongside the RAM pages
    /// and fetched by κ ON DEMAND at resume via [`Cpu::restore_kappa_streaming_lazy_disk`], so only
    /// the working set (~104 KiB measured) crosses the wire. Disk reads stay verify-on-receipt (L5).
    #[must_use]
    pub fn snapshot_kappa_manifest_streaming_disk(&self) -> Vec<u8> {
        let ram_pages = self.ram.chunks(KAPPA_PAGE).map(address_bytes).collect();
        KappaSnapshot { state: self.snapshot_state(false), ram_pages, ram_len: self.ram.len() }
            .to_manifest_bytes()
    }

    /// Streaming κ-resume from a [`Cpu::snapshot_kappa_manifest_streaming_disk`]: stream the RAM
    /// pages by κ via `fetch` (L5, as [`Cpu::restore_kappa_streaming`]), then plug the κ-disk onto a
    /// **lazy** backing that fetches sectors by κ on demand via the owned `disk_fetch` (verify-on-
    /// receipt L5). The manifest carried only the disk INDEX, so the disk is empty until read — and
    /// then pulls only the sectors actually touched. Returns `false` on a bad manifest/page.
    pub fn restore_kappa_streaming_lazy_disk<F: FnMut(&[u8; KAPPA_LABEL_LEN]) -> Option<Vec<u8>>>(
        &mut self,
        manifest: &[u8],
        mut ram_fetch: F,
        disk_fetch: Box<dyn Fn(&[u8; KAPPA_LABEL_LEN]) -> Option<Vec<u8>> + Send + Sync>,
    ) -> bool {
        if !self.restore_kappa_streaming(manifest, |k| ram_fetch(k.as_array())) {
            return false;
        }
        // The disk restored index-only (empty store); make it lazy over the transport. A machine
        // with no disk simply has nothing to restream (returns false there) — tolerate it.
        let _ = self.restream_disk(disk_fetch);
        true
    }

    /// **Streaming** κ-resume: given a [`Cpu::snapshot_kappa_manifest`], fetch each page's bytes
    /// on demand via `fetch` (the transport — a local store, `content_net`, OPFS, the page's
    /// `fetch`, …), **verify each before use** (L5 — re-derive its κ; refuse a missing/tampered
    /// page), and reconstruct. Duplicate/zero pages are fetched once (cached by κ), so only the
    /// unique working set crosses the wire. Returns `false` on a bad manifest or any page that
    /// fails to arrive or verify.
    pub fn restore_kappa_streaming<F: FnMut(&KappaLabel71) -> Option<Vec<u8>>>(
        &mut self,
        manifest: &[u8],
        mut fetch: F,
    ) -> bool {
        let Some(snap) = KappaSnapshot::from_manifest_bytes(manifest) else {
            return false;
        };
        let mut cache: BTreeMap<[u8; KAPPA_LABEL_LEN], Vec<u8>> = BTreeMap::new();
        let mut ram = vec![0u8; snap.ram_len];
        for (i, k) in snap.ram_pages.iter().enumerate() {
            let key = *k.as_array();
            if !cache.contains_key(&key) {
                let bytes = match fetch(k) {
                    Some(b) => b,
                    None => return false,
                };
                if !matches!(verify_kappa(&bytes, k), Ok(true)) {
                    return false; // L5: never write a page that doesn't re-derive to its κ
                }
                cache.insert(key, bytes);
            }
            let b = &cache[&key];
            let off = i * KAPPA_PAGE;
            let end = (off + b.len()).min(ram.len());
            ram[off..end].copy_from_slice(&b[..end - off]);
        }
        // Streaming resume is from a current-version manifest ⇒ has the net block.
        if self.restore_state(&mut Snap::new(&snap.state), true).is_none() {
            return false;
        }
        self.ram = ram;
        self.flush_caches_after_restore();
        true
    }

    /// Read the CPU + device state (everything except RAM) — the inverse of
    /// [`Cpu::snapshot_state`]. Returns `None` on truncated/malformed bytes.
    /// `has_net`: whether the serialized device state carries the virtio-net block (format v3+). A v2
    /// blob predates it, so its `Sys` state has no net block to read.
    fn restore_state(&mut self, s: &mut Snap, has_net: bool) -> Option<()> {
        for r in &mut self.r {
            *r = s.u64()?;
        }
        self.rip = s.u64()?;
        self.rflags = s.u64()?;
        self.insns = s.u64()?;
        self.cr0 = s.u64()?;
        self.cr2 = s.u64()?;
        self.cr3 = s.u64()?;
        self.cr4 = s.u64()?;
        self.efer = s.u64()?;
        for d in &mut self.dr {
            *d = s.u64()?;
        }
        for x in &mut self.xmm {
            let lo = s.u64()?;
            let hi = s.u64()?;
            *x = u128::from(lo) | (u128::from(hi) << 64);
        }
        // x87 FPU register file + control/status (mirrors `snapshot_state`).
        for f in &mut self.fpr {
            *f = f64::from_bits(s.u64()?);
        }
        self.ftop = s.u8()?;
        self.fcw = s.u16()?;
        self.fsw = s.u16()?;
        for sg in &mut self.seg {
            *sg = Seg::unsnap(s)?;
        }
        self.cpl = s.u8()?;
        self.sys = match s.u8()? {
            0 => None,
            _ => Some(Box::new(Sys::unsnap(s, has_net)?)),
        };
        Some(())
    }

    /// After swapping in restored state, the software TLB/`ifetch` caches are stale — flush them
    /// (they rebuild lazily from the restored `cr3` + RAM, so they are never serialized).
    fn flush_caches_after_restore(&mut self) {
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
        self.ifetch_gen = 0;
    }

    /// The machine's UNIQUE RAM pages as (κ-bytes, page-bytes) — what a transport serves so a
    /// streaming resume can fetch each page by κ (dedup'd; the same mechanism the disk uses).
    #[must_use]
    pub fn kappa_ram_pages(&self) -> Vec<([u8; KAPPA_LABEL_LEN], Vec<u8>)> {
        let mut seen: BTreeSet<[u8; KAPPA_LABEL_LEN]> = BTreeSet::new();
        let mut out = Vec::new();
        for chunk in self.ram.chunks(KAPPA_PAGE) {
            let k = *address_bytes(chunk).as_array();
            if seen.insert(k) {
                out.push((k, chunk.to_vec()));
            }
        }
        out
    }

    /// The disk's UNIQUE (κ-bytes, sector-bytes) pairs — what a transport serves for a streaming
    /// resume (so the disk streams by κ on demand, like RAM pages). Empty if no disk is attached.
    #[must_use]
    pub fn disk_unique_sectors(&self) -> Vec<([u8; KAPPA_LABEL_LEN], Vec<u8>)> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio.as_ref())
            .map(|blk| blk.unique_sectors().into_iter().map(|(k, b)| (*k.as_array(), b)).collect())
            .unwrap_or_default()
    }

    /// Swap the κ-disk for a LAZY streaming backing: sectors fetch by κ on demand via `fetch`
    /// (verify-on-receipt L5), so a resumed machine pulls only the disk working set. The `fetch`
    /// takes the κ as raw bytes. Returns `false` if no disk is attached.
    pub fn restream_disk(
        &mut self,
        fetch: Box<dyn Fn(&[u8; KAPPA_LABEL_LEN]) -> Option<Vec<u8>> + Send + Sync>,
    ) -> bool {
        let adapted: Box<dyn Fn(&KappaLabel71) -> Option<Vec<u8>> + Send + Sync> =
            Box::new(move |k| fetch(k.as_array()));
        match self.sys.as_mut().and_then(|s| s.virtio.as_mut()) {
            Some(blk) => {
                blk.restream(adapted);
                true
            }
            None => false,
        }
    }

    /// Feed terminal input to the guest's serial console (readable at `0x3f8`).
    pub fn feed_console(&mut self, bytes: &[u8]) {
        if let Some(sys) = self.sys.as_mut() {
            sys.uart.input.extend_from_slice(bytes);
            // Raise the receive interrupt if the driver runs input interrupt-driven.
            if sys.uart.ier & 0x01 != 0 {
                sys.pic.raise(4);
            }
        }
    }

    /// Read a general-purpose register (for tests / introspection).
    #[must_use]
    pub fn reg(&self, i: usize) -> u64 {
        self.r[i & 15]
    }

    /// `RFLAGS` (for tests / introspection).
    #[must_use]
    pub fn rflags(&self) -> u64 {
        self.rflags
    }

    /// `rip` (for tests / introspection).
    #[must_use]
    pub fn rip(&self) -> u64 {
        self.rip
    }

    /// The 16 general-purpose registers (diagnostic — for stall/fault dumps).
    #[must_use]
    pub fn regs(&self) -> [u64; 16] {
        self.r
    }

    /// Retired guest-instruction count — a monotonic tally of executed
    /// instructions (the x86-64 analogue of RISC-V `INSTRET`); informational.
    #[must_use]
    pub fn insns(&self) -> u64 {
        self.insns
    }

    /// Advance the PIT (channel 0 + the channel-2 calibration gate) and the LAPIC
    /// timer down-counters by one tick, raising the PIC/LAPIC interrupt when a
    /// counter expires (reloading periodic timers). The per-`TICK_DIV` body of
    /// [`sys_tick`](Self::sys_tick) — the platform timer cadence a real Linux boot
    /// calibrates against and uses for jiffies.
    fn tick_down_counters(&mut self) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        if sys.pit.ch2_gate && !sys.pit.ch2_out && sys.pit.ch2_counter > 0 {
            sys.pit.ch2_counter = sys.pit.ch2_counter.saturating_sub(1);
            if sys.pit.ch2_counter == 0 {
                sys.pit.ch2_out = true;
            }
        }
        if sys.pit.enabled && sys.pit.reload != 0 {
            if sys.pit.counter == 0 {
                sys.pit.counter = u32::from(sys.pit.reload);
            }
            sys.pit.counter = sys.pit.counter.saturating_sub(1);
            if sys.pit.counter == 0 {
                sys.pic.raise(0);
                if sys.pit.ch0_periodic {
                    sys.pit.counter = u32::from(sys.pit.reload);
                } else {
                    sys.pit.enabled = false;
                }
            }
        }
        if sys.lapic.enabled()
            && sys.lapic.initial_count != 0
            && sys.lapic.lvt_timer & (1 << 16) == 0
        {
            if sys.lapic.current_count == 0 {
                sys.lapic.current_count = sys.lapic.initial_count;
            }
            sys.lapic.current_count = sys.lapic.current_count.saturating_sub(1);
            if sys.lapic.current_count == 0 {
                if sys.lapic.lvt_timer & (1 << 17) != 0 {
                    sys.lapic.current_count = sys.lapic.initial_count;
                }
                sys.lapic.set_irr((sys.lapic.lvt_timer & 0xff) as u8);
            }
        }
    }

    // ── Memory ───────────────────────────────────────────────────────────────
    fn rd(&mut self, addr: u64, size: u8) -> u64 {
        let user = self.cpl == 3;
        let pa = self.translate_acc(addr, false, user);
        if (VIRTIO_BLK_BASE..VIRTIO_INPUT_END).contains(&pa) {
            return self.mmio_read(pa, size as usize);
        }
        if (LAPIC_BASE..LAPIC_END).contains(&pa) {
            return u64::from(self.lapic_read((pa - LAPIC_BASE) as u32));
        }
        self.dcache_touch(pa);
        // Same-page accesses (the common case) map to contiguous physical bytes
        // `pa..pa+size`; the first-byte translate above filled the TLB + A/D bits, so
        // the rest need no re-translation — byte-identical to the per-byte loop, one
        // translation instead of `size`. Only a page crossing falls back to per-byte.
        let last = addr.wrapping_add(u64::from(size).saturating_sub(1));
        let same_page = (addr >> 12) == (last >> 12);
        let mut v = 0u64;
        if same_page {
            for i in 0..u64::from(size) {
                v |= u64::from(*self.ram.get((pa + i) as usize).unwrap_or(&0)) << (8 * i);
            }
        } else {
            for i in 0..u64::from(size) {
                let p = self.translate_acc(addr.wrapping_add(i), false, user) as usize;
                v |= u64::from(*self.ram.get(p).unwrap_or(&0)) << (8 * i);
            }
        }
        #[cfg(feature = "cc44-trace")]
        if addr == 0xffff_ffff_827b_b1e8 && TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] RD vmemmap_base va={addr:#x} -> pa={pa:#x} val={v:#x} rip={:#x}",
                self.rip,
            );
        }
        v
    }

    fn wr(&mut self, addr: u64, size: u8, val: u64) {
        let user = self.cpl == 3;
        let pa = self.translate_acc(addr, true, user);
        if (VIRTIO_BLK_BASE..VIRTIO_INPUT_END).contains(&pa) {
            self.mmio_write(pa, size as usize, val);
            return;
        }
        if (LAPIC_BASE..LAPIC_END).contains(&pa) {
            self.lapic_write((pa - LAPIC_BASE) as u32, val as u32);
            return;
        }
        self.dcache_touch(pa);
        // Stack-frame corruption watch (M1): the `ip=0x1` crash is a `ret` popping a
        // clobbered return address from ~0x7fff0a4f3948. Log any store whose byte
        // range covers that slot — the last one before the bad `ret` is the culprit.
        #[cfg(feature = "cc44-trace")]
        {
            let lo = addr;
            let hi = addr.wrapping_add(u64::from(size));
            if lo < 0x7fff_0a4f_3958 && hi > 0x7fff_0a4f_3938 {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] STACK-WR va={addr:#x} size={size} val={val:#x} rip={:#x} cpl={}",
                    self.rip,
                    self.cpl,
                );
            }
        }
        #[cfg(feature = "cc44-trace")]
        if TP_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
            && (0xffff_ffff_82a0_3e38..=0xffff_ffff_82a0_3e48).contains(&addr)
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] POKE-WR va={addr:#x} -> pa={pa:#x} size={size} val={val:#x} cr3={:#x} rip={:#x}",
                self.cr3,
                self.rip,
            );
            let _ = std::io::stderr().flush();
        }
        let last = addr.wrapping_add(u64::from(size).saturating_sub(1));
        if (addr >> 12) == (last >> 12) {
            // Same-page store: contiguous physical bytes, one translation (above).
            for i in 0..u64::from(size) {
                if let Some(b) = self.ram.get_mut((pa + i) as usize) {
                    *b = (val >> (8 * i)) as u8;
                }
            }
        } else {
            for i in 0..u64::from(size) {
                let p = self.translate_acc(addr.wrapping_add(i), true, user) as usize;
                if let Some(b) = self.ram.get_mut(p) {
                    *b = (val >> (8 * i)) as u8;
                }
            }
        }
    }

    fn fetch_u8(&mut self) -> u8 {
        let vaddr = self.rip;
        // Inline code-page cache: the bytes after the first in an instruction are on
        // the same code page, so reuse its VA→frame translation (validated against the
        // TLB/PCID generations) instead of re-running `translate_acc` per byte. A
        // pending fault disables it (don't read past a fault). Byte-identical to
        // `translate_acc` on a hit (same frame the TLB holds), and any flush bumps a
        // generation so a stale entry can never be used.
        if self.fault.is_none() && Self::tlb_fastpath() {
            let page = vaddr & !0xfff;
            let pcid = self.active_pcid();
            if self.ifetch_gen == self.tlb_gen
                && self.ifetch_tag == page
                && self.ifetch_pcid as usize == pcid
                && self.ifetch_pgen == self.pcid_gen[pcid]
            {
                let p = (self.ifetch_frame | (vaddr & 0xfff)) as usize;
                let b = *self.ram.get(p).unwrap_or(&0);
                self.rip = vaddr.wrapping_add(1);
                return b;
            }
        }
        let user = self.cpl == 3;
        let pa = self.translate_acc(vaddr, false, user);
        // Refresh the cache only on a successful (present) paged translation — exactly
        // the condition under which `translate_acc` filled the TLB.
        if self.fault.is_none() && self.paging() {
            self.ifetch_tag = vaddr & !0xfff;
            self.ifetch_frame = pa & !0xfff;
            self.ifetch_gen = self.tlb_gen;
            let pcid = self.active_pcid();
            self.ifetch_pcid = pcid as u16;
            self.ifetch_pgen = self.pcid_gen[pcid];
        }
        let b = *self.ram.get(pa as usize).unwrap_or(&0);
        self.rip = vaddr.wrapping_add(1);
        b
    }

    fn fetch(&mut self, n: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(self.fetch_u8()) << (8 * i);
        }
        v
    }

    /// Fetch the operand-size immediate (`imm16`/`imm32`, sign-extended to the
    /// operand `size`): 2 bytes under a `0x66` prefix, otherwise 4 bytes (a
    /// 64-bit operand takes a sign-extended `imm32`). The common immediate form of
    /// `MOV`/`ADD`/`TEST`/… r/m, imm.
    fn fetch_imm_z(&mut self, size: u8) -> u64 {
        if size == 2 {
            self.fetch(2)
        } else {
            let i = self.fetch(4);
            if size == 8 {
                i as i32 as i64 as u64
            } else {
                i
            }
        }
    }

    // ── Flags ────────────────────────────────────────────────────────────────
    fn set(&mut self, bit: u64, on: bool) {
        if on {
            self.rflags |= bit;
        } else {
            self.rflags &= !bit;
        }
    }

    fn mask(size: u8) -> u64 {
        if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (8 * size as u32)) - 1
        }
    }

    fn sign(v: u64, size: u8) -> bool {
        (v >> (8 * size as u32 - 1)) & 1 == 1
    }

    /// Set ZF/SF/PF (and clear CF/OF) from a logical result.
    fn flags_logic(&mut self, res: u64, size: u8) {
        let m = Self::mask(size);
        let r = res & m;
        self.set(flag::ZF, r == 0);
        self.set(flag::SF, Self::sign(r, size));
        self.set(flag::PF, (r as u8).count_ones().is_multiple_of(2));
        self.set(flag::CF, false);
        self.set(flag::OF, false);
    }

    /// Set all arithmetic flags for `a (+/-) b = res` (`sub` selects subtraction).
    fn flags_arith(&mut self, a: u64, b: u64, res: u64, size: u8, sub: bool) {
        let m = Self::mask(size);
        let r = res & m;
        self.set(flag::ZF, r == 0);
        self.set(flag::SF, Self::sign(r, size));
        self.set(flag::PF, (r as u8).count_ones().is_multiple_of(2));
        if sub {
            self.set(flag::CF, (a & m) < (b & m));
            let of = (Self::sign(a, size) != Self::sign(b, size))
                && (Self::sign(a, size) != Self::sign(r, size));
            self.set(flag::OF, of);
        } else {
            self.set(flag::CF, r < (a & m));
            let of = (Self::sign(a, size) == Self::sign(b, size))
                && (Self::sign(a, size) != Self::sign(r, size));
            self.set(flag::OF, of);
        }
    }

    /// Run up to `max_steps` instructions; returns why it stopped.
    /// JIT block discovery at a basic-block entry: translate `rip` to its guest bytes,
    /// decode the linear block, key it by BLAKE3 (κ), and record it (the cache compiles hot
    /// blocks). Side-effect-free w.r.t. architectural state — a probe-only fetch translation
    /// (any fault it raises is cleared; `step()` does the real fetch). Only substantial
    /// blocks (≥4 modelled ops) are recorded — tiny blocks aren't worth compiling.
    /// Returns `true` if it executed the block (the caller skips `step()` this iteration) —
    /// only for a TRUSTED κ, where the result is committed. An un-trusted block is run dry and
    /// shadow-validated (returns `false`; `step()` runs and `jit_shadow_check` compares).
    #[cfg(feature = "jit-native")]
    fn jit_dispatch(&mut self) -> bool {
        if self.fault.is_some() {
            return false;
        }
        let pa = self.translate_acc(self.rip, false, false);
        if self.fault.is_some() {
            self.fault = None; // page not currently mapped — let step() fetch + fault
            return false;
        }
        let off = pa as usize;
        let end = ((off & !0xfff) + 0x1000).min(self.ram.len());
        if off >= end {
            return false;
        }
        let (ops, _offsets, len) = crate::emulator::jit::decode_block(&self.ram[off..end]);
        if ops.len() < JIT_MIN_OPS || len == 0 {
            return false; // short blocks lose to the interpreter — don't JIT them
        }
        let key: [u8; 32] = *blake3::hash(&self.ram[off..off + len]).as_bytes();
        JIT_DECODED.with(|c| c.set(c.get() + 1));
        // compile hot blocks; clone the wasm so the cache borrow is released
        let wasm = JIT_CACHE.with(|c| c.borrow_mut().record(key, &ops).map(<[u8]>::to_vec));
        let Some(wasm) = wasm else { return false };
        if JIT_REFUSED.with(|c| c.borrow().contains(&key)) {
            return false; // known-divergent block — always interpret
        }
        let trusted = JIT_TRUSTED.with(|c| c.borrow().contains(&key));
        // run the JIT — one `self` borrow via the page-fetch closure (read-only `walk`)
        let (entry_r, entry_f, block_start) = (self.r, self.rflags, self.rip);
        let jit = crate::emulator::jit_exec::exec_block_pooled(key, &wasm, &ops, entry_r, entry_f, |va| {
            let pa = self.walk(va, false, false).ok()?;
            let f = (pa & !0xfff) as usize;
            (f + 0x1000 <= self.ram.len()).then(|| (f, self.ram[f..f + 0x1000].to_vec()))
        });
        let Some((regs, rflags, dirty)) = jit else {
            return false; // couldn't complete (fault/pool/collision) — interpret
        };
        if trusted {
            // COMMIT — the block is validated bit-identical, so apply its result and skip step()
            self.r = regs;
            self.rflags = rflags;
            for (pa, bytes) in dirty {
                self.ram[pa..pa + bytes.len()].copy_from_slice(&bytes);
            }
            self.rip = block_start + len as u64;
            self.insns = self.insns.wrapping_add(ops.len() as u64);
            JIT_COMMITTED.with(|c| c.set(c.get() + 1));
            true
        } else {
            // SHADOW — record the dry result; the interpreter runs and jit_shadow_check compares
            JIT_PENDING.with(|c| {
                *c.borrow_mut() = Some(JitPending {
                    key,
                    start: block_start,
                    end: block_start + len as u64,
                    regs,
                    rflags,
                    dirty,
                    has_shift: crate::emulator::jit::block_has_shift(&ops),
                });
            });
            false
        }
    }

    /// After `step()`: if a shadowed JIT block has just been completed by the interpreter
    /// (straight-line to `end`), compare the JIT's dry result to the real architectural state.
    /// K matches → trust (stop shadowing); a mismatch → refuse the block's κ forever. A block
    /// the interpreter left early (interrupt/branch/fault) is discarded uncompared.
    #[cfg(feature = "jit-native")]
    fn jit_shadow_check(&mut self) {
        enum Act {
            Compare,
            Discard,
            Keep,
        }
        let act = JIT_PENDING.with(|c| match c.borrow().as_ref() {
            None => None,
            Some(p) if self.rip == p.end => Some(Act::Compare),
            Some(p) if self.rip >= p.start && self.rip < p.end => Some(Act::Keep),
            Some(_) => Some(Act::Discard),
        });
        match act {
            Some(Act::Compare) => {
                let p = JIT_PENDING.with(|c| c.borrow_mut().take()).unwrap();
                let regs_ok = self.r == p.regs;
                let rflags_ok = self.rflags == p.rflags;
                let mem_ok = p.dirty.iter().all(|(pa, b)| {
                    *pa + b.len() <= self.ram.len() && self.ram[*pa..*pa + b.len()] == b[..]
                });
                let ok = regs_ok && rflags_ok && mem_ok;
                if !ok {
                    if !regs_ok {
                        JIT_MM_REGS.with(|c| c.set(c.get() + 1));
                    }
                    if !rflags_ok {
                        JIT_MM_RFLAGS.with(|c| c.set(c.get() + 1));
                    }
                    if !mem_ok {
                        JIT_MM_MEM.with(|c| c.set(c.get() + 1));
                    }
                    if p.has_shift {
                        JIT_MM_SHIFT.with(|c| c.set(c.get() + 1));
                    }
                }
                if ok {
                    JIT_MATCH.with(|c| c.set(c.get() + 1));
                    let n = JIT_COUNT.with(|c| {
                        let mut m = c.borrow_mut();
                        let e = m.entry(p.key).or_insert(0);
                        *e += 1;
                        *e
                    });
                    if n >= JIT_TRUST_K {
                        JIT_TRUSTED.with(|c| {
                            c.borrow_mut().insert(p.key);
                        });
                    }
                } else {
                    JIT_MISMATCH.with(|c| c.set(c.get() + 1));
                    JIT_REFUSED.with(|c| {
                        c.borrow_mut().insert(p.key);
                    });
                }
            }
            Some(Act::Discard) => JIT_PENDING.with(|c| *c.borrow_mut() = None),
            Some(Act::Keep) | None => {}
        }
    }

    /// CHAINING dispatch (the region JIT): at the current rip, discover a region of linked blocks
    /// over the live code page, compile it (`compile_region`, κ-keyed), and run it via
    /// `exec_region_pooled` against this `Cpu`'s paged memory (mirrors `jit_dispatch`'s
    /// `exec_block_pooled` call). On success the region's effects are APPLIED and `rip` is set to
    /// the exit rip — many guest blocks retired in one wasm entry. Returns `true` if it ran a region
    /// (so the run loop skips `step()`), `false` to fall back to the per-block path / interpreter.
    /// NB: this applies directly (the test path); production wraps it in SHADOW→TRUST like
    /// `jit_dispatch`. Only loops / multi-block regions are taken (the per-block path handles the rest).
    #[cfg(feature = "jit")]
    fn jit_run_region(&mut self) -> bool {
        const REGION_MAX_BLOCKS: usize = 8;
        const REGION_MAX_ITERS: u64 = 1_000_000;
        if self.fault.is_some() {
            return false;
        }
        let pa = self.translate_acc(self.rip, false, false);
        if self.fault.is_some() {
            self.fault = None;
            return false;
        }
        let off = pa as usize;
        let page_end = ((off & !0xfff) + 0x1000).min(self.ram.len());
        if off >= page_end {
            return false;
        }
        let region = crate::emulator::jit::discover_region(
            &self.ram[off..page_end],
            self.rip,
            REGION_MAX_BLOCKS,
        );
        // Worth a region only if it loops (a terminator targets an in-region block) or is multi-block.
        let in_region = |t: u64| region.index_of(t).is_some();
        let loops = region.blocks.iter().any(|b| match b.term {
            crate::emulator::jit::Terminator::Jmp { target } => in_region(target),
            crate::emulator::jit::Terminator::Jcc { taken, fall, .. } => {
                in_region(taken) || in_region(fall)
            }
            crate::emulator::jit::Terminator::Exit => false,
        });
        if region.blocks.len() < 2 && !loops {
            return false;
        }
        // κ = BLAKE3 of the region's byte span (entry → furthest block end); SMC changes it.
        let span = region
            .blocks
            .iter()
            .map(|b| (b.start + b.len as u64).saturating_sub(self.rip) as usize)
            .max()
            .unwrap_or(0)
            .min(page_end - off);
        if span == 0 {
            return false;
        }
        let key: [u8; 32] = *blake3::hash(&self.ram[off..off + span]).as_bytes();
        if JIT_REFUSED.with(|c| c.borrow().contains(&key)) {
            return false;
        }
        let wasm = JIT_REGION_CACHE.with(|c| {
            c.borrow_mut()
                .entry(key)
                .or_insert_with(|| crate::emulator::jit::compile_region(&region, REGION_MAX_ITERS))
                .clone()
        });
        let (entry_rip, entry_r, entry_f) = (self.rip, self.r, self.rflags);
        let fetch = |va: u64| -> Option<(usize, Vec<u8>)> {
            let pa = self.walk(va, false, false).ok()?;
            let f = (pa & !0xfff) as usize;
            (f + 0x1000 <= self.ram.len()).then(|| (f, self.ram[f..f + 0x1000].to_vec()))
        };
        // The executor seam: a browser-injected `WebAssembly` executor, else the native wasmtime one.
        // With `jit-native` the `None` arm runs the wasmtime executor; in a browser-only `jit` build
        // wasmtime is absent, so an un-injected executor simply declines (the peer always injects one).
        let res = match REGION_EXEC.with(core::cell::Cell::get) {
            Some(exec) => exec(key, &wasm, entry_r, entry_f, &fetch),
            #[cfg(feature = "jit-native")]
            None => crate::emulator::jit_exec::exec_region_pooled(key, &wasm, entry_r, entry_f, &fetch),
            #[cfg(not(feature = "jit-native"))]
            None => None,
        };
        let Some((regs, rflags, dirty, exit_rip)) = res else {
            return false;
        };
        let notrust = JIT_REGION_NOTRUST.with(core::cell::Cell::get);
        if !notrust && JIT_TRUSTED.with(|c| c.borrow().contains(&key)) {
            // COMMIT — validated bit-identical; apply the region's effects and skip the interpreter.
            self.r = regs;
            self.rflags = rflags;
            for (pa, bytes) in dirty {
                if pa + bytes.len() <= self.ram.len() {
                    self.ram[pa..pa + bytes.len()].copy_from_slice(&bytes);
                }
            }
            self.rip = exit_rip;
            JIT_COMMITTED.with(|c| c.set(c.get() + 1));
            true
        } else {
            // SHADOW — record the dry result; the interpreter runs and jit_region_shadow_check compares.
            let ops_dbg = if notrust {
                let mut s = String::new();
                for b in &region.blocks {
                    s.push_str(&format!("@{:#x}{:?}->{:?} ", b.start, b.ops, b.term));
                }
                s
            } else {
                String::new()
            };
            JIT_REGION_PENDING.with(|c| {
                *c.borrow_mut() = Some(JitRegionPending {
                    key,
                    start: entry_rip,
                    span_end: entry_rip + span as u64,
                    exit: exit_rip,
                    regs,
                    rflags,
                    dirty,
                    entry_insns: self.insns,
                    ops_dbg,
                });
            });
            false
        }
    }

    /// After `step()`: validate a shadowed REGION once the interpreter reaches its exit rip. K
    /// matches → trust (commit thereafter); a mismatch → refuse the region's κ forever. While the
    /// interpreter is still inside the region's rip span (looping), keep shadowing; if it leaves
    /// elsewhere (an interrupt/fault took it out), discard uncompared. Mirrors `jit_shadow_check`.
    #[cfg(feature = "jit")]
    fn jit_region_shadow_check(&mut self) {
        enum Act {
            Compare,
            Discard,
            Keep,
        }
        let act = JIT_REGION_PENDING.with(|c| match c.borrow().as_ref() {
            None => None,
            Some(p) if self.rip == p.exit => Some(Act::Compare),
            Some(p) if self.rip >= p.start && self.rip < p.span_end => Some(Act::Keep),
            Some(_) => Some(Act::Discard),
        });
        match act {
            Some(Act::Compare) => {
                let p = JIT_REGION_PENDING.with(|c| c.borrow_mut().take()).unwrap();
                let ok = self.r == p.regs
                    && self.rflags == p.rflags
                    && p.dirty.iter().all(|(pa, b)| {
                        *pa + b.len() <= self.ram.len() && self.ram[*pa..*pa + b.len()] == b[..]
                    });
                let executed = self.insns.wrapping_sub(p.entry_insns);
                if ok && executed < REGION_MIN_INSNS {
                    // Correct, but too SHORT to pay for the per-commit marshalling — refuse its κ so it
                    // never JITs again (the fix for the 0.44× real-Alpine slowdown: only long-running
                    // regions amortize the marshal). The interpreter keeps running it.
                    JIT_REFUSED.with(|c| {
                        c.borrow_mut().insert(p.key);
                    });
                } else if ok {
                    let n = JIT_REGION_COUNT.with(|c| {
                        let mut m = c.borrow_mut();
                        let e = m.entry(p.key).or_insert(0);
                        *e += 1;
                        *e
                    });
                    if n >= JIT_TRUST_K {
                        JIT_TRUSTED.with(|c| {
                            c.borrow_mut().insert(p.key);
                        });
                    }
                } else {
                    JIT_REFUSED.with(|c| {
                        c.borrow_mut().insert(p.key);
                    });
                    // Diagnostic: capture the FIRST divergence (which field, region-dry vs interpreter)
                    // so a real-workload run pins the buggy region to a specific instruction shape.
                    if !p.ops_dbg.is_empty() {
                        JIT_REGION_DIVERGENCE.with(|c| {
                            let mut slot = c.borrow_mut();
                            if slot.is_none() {
                                let reg_diff: Vec<String> = (0..16)
                                    .filter(|&i| self.r[i] != p.regs[i])
                                    .map(|i| format!("r{i}: jit={:#x} interp={:#x}", p.regs[i], self.r[i]))
                                    .collect();
                                let dirty_bad: Vec<String> = p
                                    .dirty
                                    .iter()
                                    .filter(|(pa, b)| {
                                        *pa + b.len() > self.ram.len() || self.ram[*pa..*pa + b.len()] != b[..]
                                    })
                                    .map(|(pa, _)| format!("page {pa:#x}"))
                                    .collect();
                                *slot = Some(format!(
                                    "DIVERGENCE at exit={:#x} start={:#x}\n  regs: [{}]\n  rflags: jit={:#x} interp={:#x} (xor={:#x})\n  dirty-mismatch: [{}]\n  region ops: {}",
                                    p.exit, p.start,
                                    reg_diff.join(", "),
                                    p.rflags, self.rflags, p.rflags ^ self.rflags,
                                    dirty_bad.join(", "),
                                    p.ops_dbg,
                                ));
                            }
                        });
                    }
                }
            }
            Some(Act::Discard) => JIT_REGION_PENDING.with(|c| *c.borrow_mut() = None),
            Some(Act::Keep) | None => {}
        }
    }

    /// Run up to `max_steps` guest instructions, returning why execution stopped (the guest
    /// powered off, hit a budget, etc.). The interpreter loop: pump timers/interrupts/devices,
    /// optionally dispatch the block JIT (off by default), then `step()` one instruction.
    pub fn run(&mut self, max_steps: u64) -> Halt {
        for i in 0..max_steps {
            #[cfg(feature = "cc44-trace")]
            if i & 0x3ff_ffff == 0 {
                use std::io::Write as _;
                let tsc = self.sys.as_ref().map(|s| s.tsc).unwrap_or(0);
                let _ = writeln!(
                    std::io::stderr(),
                    "\n[cc44-trace] step={i} rip={:#x} tsc={tsc} if={}",
                    self.rip,
                    self.rflags & RFLAGS_IF != 0
                );
            }
            // Pump the network periodically so host-side data and connection
            // events reach the guest without it having to transmit first (the
            // `virtio-net` receive path; `CC-16` parity, `CC-46`) — the same
            // shared `devbus` pump the other cores drive from their run loops.
            if i & 0x3ff == 0 && self.sys.as_ref().is_some_and(|s| s.virtionet.is_some()) {
                self.virtio_net_pump();
            }
            // JIT Rung 0: sample the executing code page (cheap, 1/1024 instructions).
            #[cfg(feature = "std")]
            if i & 0x3ff == 0 {
                hotprof_sample(self.rip);
            }
            // Diagnostic: does this resumed machine ever run userspace? Count CPL==3 steps + the
            // code pages they run on (so a test can see WHAT userspace ran — shell vs fault handler).
            // OFF by default (HOLO_BOOT_DIAG) — the USER_PAGES HashMap insert per userspace instruction
            // was a real drag on the desktop workload (almost all userspace).
            #[cfg(feature = "std")]
            if self.cpl == 3 && Self::boot_diag() {
                USER_INSNS.with(|c| c.set(c.get() + 1));
                let page = self.rip & !0xfff;
                USER_PAGES.with(|h| *h.borrow_mut().entry(page).or_insert(0) += 1);
            }
            // Advance the platform timers (PIT + APIC timer + TSC) and latch a
            // tick when one expires, then deliver any pending interrupt through
            // the IDT — the interrupt path a real Linux boot needs (`#12`). The
            // periodic timers advance unconditionally (like the riscv64/aarch64
            // cores); a latched tick is delivered only when `RFLAGS.IF` is set.
            self.sys_tick();
            if self.sys.as_ref().is_some_and(|s| s.halted) {
                return Halt::Halted;
            }
            self.take_pending_interrupt();
            #[cfg(feature = "cc44-trace")]
            let prev_rip = self.rip;
            #[cfg(feature = "cc44-trace")]
            if (0xffff_ffff_8103_6970..0xffff_ffff_8103_6e00).contains(&self.rip) {
                TP_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            #[cfg(feature = "cc44-trace")]
            if matches!(
                self.rip,
                0xffff_ffff_8103_69fe
                    | 0xffff_ffff_8103_6a04
                    | 0xffff_ffff_8103_6a53
                    | 0xffff_ffff_8103_6a65
                    | 0xffff_ffff_8103_6a7d
                    | 0xffff_ffff_8103_6a82
                    | 0xffff_ffff_8103_6a92
            ) {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] BR rip={:#x} rax={:#x} rcx={:#x} rsp={:#x}",
                    self.rip,
                    self.r[RAX],
                    self.r[RCX],
                    self.r[RSP],
                );
            }
            // JIT Rung 2 (off by default): remember rip to detect a control transfer.
            #[cfg(feature = "std")]
            let rip0 = self.rip;
            // JIT Rung 3 (off by default): at a block entry, dispatch the JIT. A trusted block
            // executes + commits here (handled = true → skip step); others run dry + shadow.
            #[cfg(feature = "jit")]
            let jit_handled = if JIT_REGION_ON.with(core::cell::Cell::get)
                && JIT_AT_ENTRY.with(core::cell::Cell::get)
            {
                // CHAINING path: try a region once this rip is hot (cheap after the first compile —
                // κ-cached). A trusted region commits here (skip step); else it shadows + step runs.
                const REGION_HOT: u32 = 8;
                let hot = JIT_REGION_HOT.with(|c| {
                    let mut m = c.borrow_mut();
                    let e = m.entry(self.rip).or_insert(0);
                    *e = e.saturating_add(1);
                    *e >= REGION_HOT
                });
                // Skip while a shadow is already pending — else a long loop re-runs the region DRY
                // at every iteration (O(L²)). One shadow per loop pass; it resolves at the exit.
                let pending = JIT_REGION_PENDING.with(|c| c.borrow().is_some());
                hot && !pending && self.jit_run_region()
            } else {
                // The per-block JIT path is native-only (it runs through the wasmtime executor);
                // a browser-only `jit` build has no block path — regions are the whole story.
                #[cfg(feature = "jit-native")]
                {
                    JIT_ON.with(core::cell::Cell::get)
                        && JIT_AT_ENTRY.with(core::cell::Cell::get)
                        && self.jit_dispatch()
                }
                #[cfg(not(feature = "jit-native"))]
                {
                    false
                }
            };
            #[cfg(not(feature = "jit"))]
            let jit_handled = false;
            if !jit_handled {
                match self.step() {
                    Ok(()) => self.insns = self.insns.wrapping_add(1),
                    Err(h) => {
                        #[cfg(feature = "cc44-trace")]
                        if let Halt::Undefined(addr) = h {
                            // Read the faulting bytes through the SAME translation the
                            // fetch used (user walk at CPL 3), not the kernel `translate`.
                            let user = self.cpl == 3;
                            let fault_pa = self.translate_acc(addr, false, user);
                            let mut bytes = [0u8; 16];
                            for (i, b) in bytes.iter_mut().enumerate() {
                                let pa = self.translate_acc(addr.wrapping_add(i as u64), false, user);
                                *b = *self.ram.get(pa as usize).unwrap_or(&0);
                            }
                            self.fault = None; // a probe must not leave a latched fault
                            // The last instruction starts leading into the fault (the
                            // branch path in — a bad jump shows as a non-sequential rip).
                            let ring: Vec<String> = RIP_RING.with(|t| {
                                t.borrow()
                                    .iter()
                                    .rev()
                                    .take(24)
                                    .map(|(r, _, _)| format!("{r:#x}"))
                                    .collect()
                            });
                            // Top of stack — a corrupted return address shows here.
                            let mut stk = [0u64; 6];
                            for (i, s) in stk.iter_mut().enumerate() {
                                let a = self.r[RSP].wrapping_add(i as u64 * 8);
                                let pa = self.translate_acc(a, false, user);
                                *s = u64::from_le_bytes(
                                    self.ram.get(pa as usize..pa as usize + 8)
                                        .and_then(|s| s.try_into().ok())
                                        .unwrap_or([0; 8]),
                                );
                            }
                            // Bytes at the last few distinct instruction starts (the
                            // branch site that jumped here — `call *reg`/`*mem` vs `ret`).
                            let recent: Vec<u64> = RIP_RING.with(|t| {
                                t.borrow().iter().rev().skip(1).take(4).map(|(r, _, _)| *r).collect()
                            });
                            let mut sites = String::new();
                            for r in &recent {
                                let mut bs = [0u8; 8];
                                for (i, b) in bs.iter_mut().enumerate() {
                                    let pa = self.translate_acc(r.wrapping_add(i as u64), false, user);
                                    *b = *self.ram.get(pa as usize).unwrap_or(&0);
                                }
                                sites.push_str(&format!("\n    {r:#x}: {bs:02x?}"));
                            }
                            self.fault = None;
                            use std::io::Write as _;
                            let _ = writeln!(
                                std::io::stderr(),
                                "[cc44-trace] UNDEFINED-INSN at rip={addr:#x} pa={fault_pa:#x} cpl={} bytes={:02x?}\n  \
                                 branch-site bytes:{sites}\n  \
                                 GPR rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsp={:#x} rbp={:#x} rsi={:#x} rdi={:#x}\n  \
                                 stack@rsp={:#x?}\n  recent-rips(newest→){:x?}",
                                self.cpl, bytes,
                                self.r[0], self.r[1], self.r[2], self.r[3],
                                self.r[4], self.r[5], self.r[6], self.r[7],
                                stk, ring,
                            );
                        }
                        return h;
                    }
                }
            }
            #[cfg(feature = "jit")]
            if JIT_ON.with(core::cell::Cell::get) || JIT_REGION_ON.with(core::cell::Cell::get) {
                let r = self.rip; // a non-sequential rip → the next instruction is a block entry
                JIT_AT_ENTRY.with(|c| c.set(r < rip0 || r > rip0.wrapping_add(15)));
                if !jit_handled {
                    #[cfg(feature = "jit-native")]
                    if JIT_ON.with(core::cell::Cell::get) {
                        self.jit_shadow_check();
                    }
                    if JIT_REGION_ON.with(core::cell::Cell::get) {
                        self.jit_region_shadow_check();
                    }
                }
            }
            // A non-sequential rip (backward, or > 15 B ahead) means the instruction
            // branched/returned/faulted — the new rip is a basic-block entry.
            #[cfg(feature = "std")]
            if BLOCKPROF_ON.with(core::cell::Cell::get) {
                let r = self.rip;
                if r < rip0 || r > rip0.wrapping_add(15) {
                    blockprof_entry(r);
                }
            }
            #[cfg(feature = "cc44-trace")]
            if self.rip < 0x1000 && prev_rip >= 0x1000 {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "\n[cc44-trace] CONTROL→{:#x} from rip={prev_rip:#x} at step={i} \
                     cr2={:#x} regs: rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} \
                     rdi={:#x} rbp={:#x} rsp={:#x} cpl={} if={}",
                    self.rip,
                    self.cr2,
                    self.r[0],
                    self.r[3],
                    self.r[1],
                    self.r[2],
                    self.r[6],
                    self.r[7],
                    self.r[RBP],
                    self.r[RSP],
                    self.cpl,
                    self.rflags & RFLAGS_IF != 0,
                );
                // Bytes of the jumping instruction + the stack top (a `ret` to a
                // corrupted return address shows the bad value at [rsp-8]).
                let u = self.cpl == 3;
                // Wide window around the jumping instruction so the function body can be
                // disassembled from the runtime bytes (objdump -b binary).
                let win_lo = prev_rip.wrapping_sub(0x40);
                let mut ib = [0u8; 0x90];
                for (k, b) in ib.iter_mut().enumerate() {
                    let pa = self.translate_acc(win_lo.wrapping_add(k as u64), false, u);
                    *b = *self.ram.get(pa as usize).unwrap_or(&0);
                }
                let mut stk = [0u64; 6];
                for (k, s) in stk.iter_mut().enumerate() {
                    let a = self.r[RSP].wrapping_sub(8).wrapping_add(k as u64 * 8);
                    let pa = self.translate_acc(a, false, u);
                    *s = u64::from_le_bytes(
                        self.ram.get(pa as usize..pa as usize + 8).and_then(|s| s.try_into().ok()).unwrap_or([0; 8]),
                    );
                }
                self.fault = None;
                // The full user-instruction (rip:rsp) trace into the crash — diff the
                // call/ret stack balance here to find the callee leaking 8 bytes.
                let utrace: Vec<String> = RIP_RING.with(|t| {
                    let v = t.borrow();
                    let all: Vec<(u64, u64, u64)> = v.iter().copied().collect();
                    let start = all.len().saturating_sub(6000);
                    all[start..].iter().map(|(r, sp, bp)| format!("{r:#x}:{sp:#x}:{bp:#x}")).collect()
                });
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] code-window@{win_lo:#x} (jumping-insn@{prev_rip:#x})={ib:02x?}\n  stack[rsp-8..]={stk:#x?}\n  cr3={:#x}\n  USER-TRACE(rip:rsp, oldest→newest):\n{}",
                    self.cr3,
                    utrace.join("\n"),
                );
                panic!("[cc44-trace] control transferred to low memory (rip<0x1000)");
            }
        }
        Halt::OutOfBudget
    }

    // ── ModRM / effective-address decode ──────────────────────────────────────
    /// Decode the ModRM (and SIB/displacement), returning `(reg, rm)` where `reg`
    /// is the register field (REX.R-extended) and `rm` is the operand location.
    fn modrm(&mut self, rex: u8) -> (usize, Rm) {
        let modrm = self.fetch_u8();
        let md = modrm >> 6;
        let reg = ((modrm >> 3) & 7) as usize | (((rex >> 2) & 1) as usize) << 3; // REX.R
        let rm_field = (modrm & 7) as usize;
        if md == 3 {
            let r = rm_field | (((rex & 1) as usize) << 3); // REX.B
            return (reg, Rm::Reg(r));
        }
        // Memory operand.
        let mut base_disp: i64;
        let mut addr: u64;
        if rm_field == 4 {
            // SIB.
            let sib = self.fetch_u8();
            let scale = sib >> 6;
            let index = ((sib >> 3) & 7) as usize | (((rex >> 1) & 1) as usize) << 3; // REX.X
            let base = (sib & 7) as usize | (((rex & 1) as usize) << 3); // REX.B
            let idx_val = if index == 4 {
                0
            } else {
                self.r[index] << scale
            };
            if (sib & 7) == 5 && md == 0 {
                addr = idx_val; // disp32, no base
                base_disp = i64::from(self.fetch(4) as i32);
            } else {
                addr = self.r[base].wrapping_add(idx_val);
                base_disp = 0;
            }
        } else if rm_field == 5 && md == 0 {
            // RIP-relative: disp32 relative to the address of the *next*
            // instruction. The end-of-instruction rip is only known once any
            // trailing immediate is consumed, so this is resolved lazily (the
            // operand is accessed after the full instruction has decoded).
            let disp = i64::from(self.fetch(4) as i32);
            let seg = self.seg_base();
            return (reg, Rm::RipRel(disp, seg));
        } else {
            addr = self.r[rm_field | (((rex & 1) as usize) << 3)];
            base_disp = 0;
        }
        match md {
            1 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(1) as i8)),
            2 => base_disp = base_disp.wrapping_add(i64::from(self.fetch(4) as i32)),
            _ => {}
        }
        addr = addr
            .wrapping_add(base_disp as u64)
            .wrapping_add(self.seg_base());
        (reg, Rm::Mem(addr))
    }

    /// The base of the segment overriding this instruction's memory operand (the
    /// `FS`/`GS` base from the corresponding MSR), or 0 with no override / in the
    /// flat segments. The kernel's per-CPU `%gs:` and userspace `%fs:` accesses
    /// need this; every other segment is flat (base 0) in long mode.
    fn seg_base(&self) -> u64 {
        match self.cur_seg {
            Some(SegId::Fs) => self.seg[SegId::Fs as usize].base,
            Some(SegId::Gs) => self.seg[SegId::Gs as usize].base,
            _ => 0,
        }
    }

    /// Resolve a decoded r/m operand to its effective address (for memory forms);
    /// `None` for a register operand. Used by `LEA` and the string/atomic ops.
    fn rm_addr(&self, rm: Rm) -> Option<u64> {
        match rm {
            Rm::Reg(_) => None,
            Rm::Mem(a) => Some(a),
            Rm::RipRel(disp, seg) => Some(self.rip.wrapping_add(disp as u64).wrapping_add(seg)),
        }
    }

    /// Read an 8-bit register operand named by the ModRM `reg` field, honouring
    /// the AH/CH/DH/BH high-byte encoding when no `REX` prefix is present.
    fn reg8(&self, reg: usize) -> u64 {
        if !self.rex_present && (4..8).contains(&reg) {
            (self.r[reg - 4] >> 8) & 0xff
        } else {
            self.r[reg] & 0xff
        }
    }

    fn load_rm(&mut self, rm: Rm, size: u8) -> u64 {
        match rm {
            Rm::Reg(i) => {
                if size == 1 && !self.rex_present && (4..8).contains(&i) {
                    // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                    (self.r[i - 4] >> 8) & 0xff
                } else {
                    self.r[i] & Self::mask(size)
                }
            }
            Rm::Mem(a) => self.rd(a, size),
            Rm::RipRel(disp, seg) => {
                let a = self.rip.wrapping_add(disp as u64).wrapping_add(seg);
                self.rd(a, size)
            }
        }
    }

    fn store_rm(&mut self, rm: Rm, size: u8, val: u64) {
        match rm {
            Rm::Reg(i) => {
                if size == 1 && !self.rex_present && (4..8).contains(&i) {
                    // AH/CH/DH/BH — bits[15:8] of RAX/RCX/RDX/RBX.
                    let r = i - 4;
                    self.r[r] = (self.r[r] & !0xff00) | ((val & 0xff) << 8);
                } else if size >= 4 {
                    // 32-bit writes zero the upper 32 bits; 64-bit is full.
                    self.r[i] = val & Self::mask(size);
                } else {
                    let m = Self::mask(size);
                    self.r[i] = (self.r[i] & !m) | (val & m);
                }
            }
            Rm::Mem(a) => self.wr(a, size, val),
            Rm::RipRel(disp, seg) => {
                let a = self.rip.wrapping_add(disp as u64).wrapping_add(seg);
                self.wr(a, size, val);
            }
        }
    }

    /// The effective address of a non-register `r/m` operand (`Mem` or RIP-relative).
    /// Mirrors `load_rm`/`store_rm`'s address computation; only valid for memory rms.
    fn xmm_mem_addr(&self, rm: Rm) -> u64 {
        match rm {
            Rm::Mem(a) => a,
            Rm::RipRel(disp, seg) => self.rip.wrapping_add(disp as u64).wrapping_add(seg),
            Rm::Reg(_) => 0,
        }
    }
    /// Load a 128-bit XMM `r/m` operand (register `xmm[i]` or 16 bytes of memory, LE).
    fn xmm_load128(&mut self, rm: Rm) -> u128 {
        match rm {
            Rm::Reg(i) => self.xmm[i],
            _ => {
                let a = self.xmm_mem_addr(rm);
                u128::from(self.rd(a, 8)) | (u128::from(self.rd(a.wrapping_add(8), 8)) << 64)
            }
        }
    }
    /// Store a 128-bit value to an XMM `r/m` operand.
    fn xmm_store128(&mut self, rm: Rm, v: u128) {
        match rm {
            Rm::Reg(i) => self.xmm[i] = v,
            _ => {
                let a = self.xmm_mem_addr(rm);
                self.wr(a, 8, v as u64);
                self.wr(a.wrapping_add(8), 8, (v >> 64) as u64);
            }
        }
    }
    /// Load the low 64 bits of an XMM `r/m` operand (register low quad or `m64`).
    fn xmm_load64(&mut self, rm: Rm) -> u64 {
        match rm {
            Rm::Reg(i) => self.xmm[i] as u64,
            _ => self.rd(self.xmm_mem_addr(rm), 8),
        }
    }
    /// Load the low 32 bits of an XMM `r/m` operand (register low dword or `m32`).
    fn xmm_load32(&mut self, rm: Rm) -> u32 {
        match rm {
            Rm::Reg(i) => self.xmm[i] as u32,
            _ => self.rd(self.xmm_mem_addr(rm), 4) as u32,
        }
    }

    fn push(&mut self, val: u64) {
        self.r[RSP] = self.r[RSP].wrapping_sub(8);
        let sp = self.r[RSP];
        self.wr(sp, 8, val);
    }

    fn pop(&mut self) -> u64 {
        let sp = self.r[RSP];
        let v = self.rd(sp, 8);
        self.r[RSP] = sp.wrapping_add(8);
        v
    }

    /// Pop `size` bytes off the stack (the far-return slot width).
    fn pop_sized(&mut self, size: u8) -> u64 {
        let sp = self.r[RSP];
        let v = self.rd(sp, size);
        self.r[RSP] = sp.wrapping_add(u64::from(size));
        v
    }

    /// One of the eight ALU group-1 operations on `(a, b)` → result, setting flags.
    fn alu(&mut self, op: u8, a: u64, b: u64, size: u8) -> u64 {
        let m = Self::mask(size);
        match op {
            0 => {
                let r = a.wrapping_add(b);
                self.flags_arith(a, b, r, size, false);
                r & m
            } // ADD
            1 => {
                let r = a | b;
                self.flags_logic(r, size);
                r & m
            } // OR
            4 => {
                let r = a & b;
                self.flags_logic(r, size);
                r & m
            } // AND
            5 => {
                let r = a.wrapping_sub(b);
                self.flags_arith(a, b, r, size, true);
                r & m
            } // SUB
            6 => {
                let r = a ^ b;
                self.flags_logic(r, size);
                r & m
            } // XOR
            7 => {
                let r = a.wrapping_sub(b);
                self.flags_arith(a, b, r, size, true);
                a & m
            } // CMP (discards result)
            2 => {
                // ADC: a + b + CF, with carry/overflow accounting for the carry-in.
                let cin = u64::from(self.rflags & flag::CF != 0);
                let r = a.wrapping_add(b).wrapping_add(cin);
                let rm = r & m;
                self.set(flag::ZF, rm == 0);
                self.set(flag::SF, Self::sign(rm, size));
                self.set(flag::PF, (rm as u8).count_ones().is_multiple_of(2));
                self.set(flag::CF, rm < (a & m) || (cin == 1 && rm == (a & m)));
                let of = (Self::sign(a, size) == Self::sign(b, size))
                    && (Self::sign(a, size) != Self::sign(rm, size));
                self.set(flag::OF, of);
                rm
            }
            3 => {
                // SBB: a - b - CF.
                let cin = u64::from(self.rflags & flag::CF != 0);
                let r = a.wrapping_sub(b).wrapping_sub(cin);
                let rm = r & m;
                self.set(flag::ZF, rm == 0);
                self.set(flag::SF, Self::sign(rm, size));
                self.set(flag::PF, (rm as u8).count_ones().is_multiple_of(2));
                self.set(
                    flag::CF,
                    (a & m) < (b & m).wrapping_add(cin) || (b & m) == m && cin == 1,
                );
                let of = (Self::sign(a, size) != Self::sign(b, size))
                    && (Self::sign(a, size) != Self::sign(rm, size));
                self.set(flag::OF, of);
                rm
            }
            _ => unreachable!("alu op {op} out of range"),
        }
    }

    /// Evaluate a condition code (the low nibble of a `Jcc`/`SETcc` opcode).
    fn cond(&self, cc: u8) -> bool {
        let f = self.rflags;
        let zf = f & flag::ZF != 0;
        let cf = f & flag::CF != 0;
        let sf = f & flag::SF != 0;
        let of = f & flag::OF != 0;
        let pf = f & flag::PF != 0;
        let base = match cc >> 1 {
            0 => of,               // O
            1 => cf,               // B/C
            2 => zf,               // E/Z
            3 => cf || zf,         // BE
            4 => sf,               // S
            5 => pf,               // P
            6 => sf != of,         // L
            _ => (sf != of) || zf, // LE
        };
        if cc & 1 == 1 {
            !base
        } else {
            base
        }
    }

    /// Port output. The boot path drives the `16550` serial console (`0x3f8`),
    /// the 8259 PIC pair (`0x20`/`0x21`, `0xa0`/`0xa1`), and the 8254 PIT
    /// (`0x40`/`0x43`); other ports are ignored (the kernel probes many).
    fn port_out(&mut self, port: u16, val: u8) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        let dlab = sys.uart.lcr & 0x80 != 0;
        match port {
            0x3f8 if dlab => sys.uart.divisor = (sys.uart.divisor & 0xff00) | u16::from(val),
            0x3f8 if sys.uart.mcr & 0x10 != 0 => {
                // Loopback (the 8250 autodetection): the transmitted byte loops
                // back to the receive register rather than reaching the console.
                sys.uart.input.push(val);
            }
            0x3f8 => {
                sys.uart.output.push(val);
                #[cfg(feature = "cc44-trace")]
                {
                    use std::io::Write as _;
                    let mut o = std::io::stderr();
                    let _ = o.write_all(&[val]);
                    let _ = o.flush();
                }
                // The TX holding register is empty again immediately (we emit at
                // once); if the driver runs the console interrupt-driven (THRE
                // enabled, IER bit 1), signal it can send the next byte. COM1 =
                // IRQ4. Without this an interrupt-driven userspace tty write blocks
                // forever waiting to transmit (the idle<->init boot livelock).
                if sys.uart.ier & 0x02 != 0 {
                    sys.uart.thre_pending = true;
                    sys.pic.raise(4);
                }
            }
            0x3f9 if dlab => {
                sys.uart.divisor = (sys.uart.divisor & 0x00ff) | (u16::from(val) << 8);
            }
            0x3f9 => {
                sys.uart.ier = val;
                // Enabling ETBEI (bit 1) with an empty THR (always — we emit at
                // once) asserts the one-shot THRE interrupt; enabling ERBFI (bit 0)
                // with a byte waiting asserts RX. COM1 = IRQ4.
                let dr = sys.uart.in_cursor < sys.uart.input.len();
                if val & 0x02 != 0 {
                    sys.uart.thre_pending = true;
                }
                if val & 0x02 != 0 || (val & 0x01 != 0 && dr) {
                    sys.pic.raise(4);
                }
            }
            0x3fa => sys.uart.fcr = val, // FCR (write side of IIR/FCR)
            0x3fb => sys.uart.lcr = val,
            0x3fc => sys.uart.mcr = val,
            0x3ff => sys.uart.scratch = val,
            // 8259 master/slave command + data ports (ICW/OCW programming).
            0x20 | 0xa0 => Self::pic_command(&mut sys.pic, port == 0xa0, val),
            0x21 | 0xa1 => Self::pic_data(&mut sys.pic, port == 0xa1, val),
            // 8254 PIT: 0x43 = mode/command, 0x40 = channel-0, 0x42 = channel-2.
            0x43 => {
                // bits 7:6 select the channel: 00 = ch0 (periodic tick), 10 = ch2
                // (the TSC-calibration one-shot). A new mode/command resets that
                // channel's byte toggle (and re-arms channel 2).
                match val >> 6 {
                    0 => {
                        sys.pit.write_hi = false;
                        // bits 3:1 = mode. Mode 2/3 = periodic (rate generator /
                        // square wave); anything else (mode 0) = one-shot.
                        let mode = (val >> 1) & 0x7;
                        sys.pit.ch0_periodic = mode == 2 || mode == 3 || mode == 6 || mode == 7;
                    }
                    2 => {
                        sys.pit.ch2_write_hi = false;
                        sys.pit.ch2_out = false;
                    }
                    _ => {}
                }
            }
            0x40 => {
                if sys.pit.write_hi {
                    sys.pit.reload = (sys.pit.reload & 0x00ff) | (u16::from(val) << 8);
                    sys.pit.enabled = true;
                    // The high byte completes the count — (re)arm the down-counter.
                    // For one-shot this is the new deadline the tick handler set.
                    sys.pit.counter = u32::from(sys.pit.reload);
                } else {
                    sys.pit.reload = (sys.pit.reload & 0xff00) | u16::from(val);
                }
                sys.pit.write_hi = !sys.pit.write_hi;
            }
            0x42 => {
                if sys.pit.ch2_write_hi {
                    sys.pit.ch2_reload = (sys.pit.ch2_reload & 0x00ff) | (u16::from(val) << 8);
                } else {
                    sys.pit.ch2_reload = (sys.pit.ch2_reload & 0xff00) | u16::from(val);
                }
                sys.pit.ch2_write_hi = !sys.pit.ch2_write_hi;
                // The high byte completes the reload → (re)load the one-shot.
                if !sys.pit.ch2_write_hi {
                    sys.pit.ch2_counter = u32::from(sys.pit.ch2_reload);
                    sys.pit.ch2_out = false;
                }
            }
            // NMI status/control (port 0x61): bit 0 gates channel 2, bit 1 the
            // speaker. A gate rising edge re-arms the one-shot from its reload.
            0x61 => {
                let gate = val & 1 != 0;
                if gate && !sys.pit.ch2_gate {
                    sys.pit.ch2_counter = u32::from(sys.pit.ch2_reload);
                    sys.pit.ch2_out = false;
                }
                sys.pit.ch2_gate = gate;
            }
            _ => {}
        }
    }

    /// Program an 8259 command port (`0x20`/`0xa0`): ICW1 begins init; an `EOI`
    /// (`0x20`) ends the in-service interrupt.
    fn pic_command(pic: &mut Pic, slave: bool, val: u8) {
        if val & 0x10 != 0 {
            // ICW1 — start the init sequence (ICW2 next).
            if slave {
                pic.init_slave = 1;
            } else {
                pic.init_master = 1;
            }
        }
        // 0x20 = non-specific EOI (handled by the ack in take_pending_interrupt).
    }

    /// Program an 8259 data port (`0x21`/`0xa1`): ICW2 (vector base), ICW3/ICW4
    /// during init, otherwise the IRQ mask (OCW1).
    fn pic_data(pic: &mut Pic, slave: bool, val: u8) {
        let init = if slave {
            &mut pic.init_slave
        } else {
            &mut pic.init_master
        };
        if *init == 1 {
            // ICW2: the vector base (the IRQ → vector remap).
            if slave {
                pic.base_slave = val & 0xf8;
            } else {
                pic.base_master = val & 0xf8;
            }
            *init = 2; // ICW3 next
        } else if *init >= 2 && *init <= 3 {
            *init += 1;
            if *init > 3 {
                *init = 0; // ICW4 consumed → init done
            }
        } else {
            // OCW1: the IRQ mask.
            if slave {
                pic.mask = (pic.mask & 0x00ff) | (u16::from(val) << 8);
            } else {
                pic.mask = (pic.mask & 0xff00) | u16::from(val);
            }
        }
    }

    fn port_in(&mut self, port: u16) -> u8 {
        if let Some(sys) = self.sys.as_mut() {
            let dlab = sys.uart.lcr & 0x80 != 0;
            match port {
                0x3f8 if dlab => return (sys.uart.divisor & 0xff) as u8, // DLL
                0x3f8 if sys.uart.in_cursor < sys.uart.input.len() => {
                    let b = sys.uart.input[sys.uart.in_cursor];
                    sys.uart.in_cursor += 1;
                    return b;
                }
                0x3f8 => return 0, // RBR, no data
                0x3f9 if dlab => return (sys.uart.divisor >> 8) as u8, // DLM
                0x3f9 => return sys.uart.ier,
                0x3fa => {
                    // IIR: report the pending UART interrupt by priority — RX-data
                    // (id 0x04) when receive ints are enabled and a byte waits,
                    // else transmit-holding-empty (id 0x02) while THRE ints are
                    // enabled (the TX register is always empty — we emit at once),
                    // else none (bit0 = 1). FIFO bits reflect FCR.
                    let dr = sys.uart.in_cursor < sys.uart.input.len();
                    let id = if sys.uart.ier & 0x01 != 0 && dr {
                        0x04
                    } else if sys.uart.ier & 0x02 != 0 && sys.uart.thre_pending {
                        // Reading the IIR clears the THRE interrupt (16550): it is a
                        // one-shot per THR-empty transition, NOT a level held while
                        // ETBEI is set — otherwise the serial ISR re-fires forever.
                        sys.uart.thre_pending = false;
                        0x02
                    } else {
                        0x01
                    };
                    return id | (sys.uart.fcr & 0xc0);
                }
                0x3fb => return sys.uart.lcr,
                0x3fc => return sys.uart.mcr,
                0x3fd => {
                    // Line Status Register: THR-empty (0x20) + transmitter-empty
                    // (0x40) always set; data-ready (0x01) when input is pending.
                    let dr = u8::from(sys.uart.in_cursor < sys.uart.input.len());
                    return 0x60 | dr;
                }
                0x3fe => {
                    // Modem Status Register. In loopback (MCR bit4) the modem
                    // inputs reflect the MCR outputs — the 8250 autodetection test.
                    if sys.uart.mcr & 0x10 != 0 {
                        let m = sys.uart.mcr;
                        // CTS<-RTS(bit1->4), DSR<-DTR(bit0->5), RI<-OUT1(bit2->6),
                        // DCD<-OUT2(bit3->7).
                        return ((m & 0x02) << 3)
                            | ((m & 0x01) << 5)
                            | ((m & 0x04) << 4)
                            | ((m & 0x08) << 4);
                    }
                    return 0xb0; // CTS|DSR|DCD asserted (a present, ready line)
                }
                0x3ff => return sys.uart.scratch,
                0x21 => return (sys.pic.mask & 0xff) as u8,
                0xa1 => return (sys.pic.mask >> 8) as u8,
                // NMI status/control: bit 5 mirrors OUT2 (channel-2 expiry) — the
                // bit the TSC-calibration loop spins on; the low bits echo the
                // gate/speaker state we were last told to set.
                0x61 => {
                    let mut v = 0u8;
                    if sys.pit.ch2_gate {
                        v |= 0x01;
                    }
                    if sys.pit.ch2_out {
                        v |= 0x20;
                    }
                    return v;
                }
                // Channel-2 counter readback (latched low-then-high) — some
                // calibration paths read the residual count.
                0x42 => {
                    let lo = sys.pit.ch2_write_hi;
                    sys.pit.ch2_write_hi = !sys.pit.ch2_write_hi;
                    return if lo {
                        (sys.pit.ch2_counter & 0xff) as u8
                    } else {
                        (sys.pit.ch2_counter >> 8) as u8
                    };
                }
                _ => {}
            }
        }
        0
    }

    /// The 32-bit PCI configuration register addressed by `pci_addr` (mechanism
    /// #1). Only bus 0 / device 0 / function 0 exists — a minimal Intel i440FX-style
    /// host bridge, so the kernel's type-1 sanity check finds a host bridge and uses
    /// mechanism #1; every other function reads all-ones (absent). The machine's
    /// real devices are virtio-mmio, not PCI.
    fn pci_config_dword(pci_addr: u32) -> u32 {
        let bus = (pci_addr >> 16) & 0xff;
        let dev = (pci_addr >> 11) & 0x1f;
        let func = (pci_addr >> 8) & 0x7;
        if bus != 0 || dev != 0 || func != 0 {
            return 0xffff_ffff; // no device
        }
        match pci_addr & 0xfc {
            0x00 => 0x7190_8086, // vendor 0x8086 (Intel), device 0x7190 (i440FX)
            0x08 => 0x0600_0000, // class 0x06 host bridge, subclass 0x00, rev 0
            _ => 0,
        }
    }

    /// A word/dword (`size` 2/4) port read. The PCI config-data port
    /// (`0xcfc`..`0xcff`) returns all-ones — no PCI host bridge is present, so the
    /// kernel's PCI scan finds no devices (it uses `virtio-mmio`, not PCI).
    /// Other ports compose from byte reads.
    fn port_in_wide(&mut self, port: u16, size: u8) -> u64 {
        if port == 0xcf8 {
            return u64::from(self.sys().pci_addr); // CONFIG_ADDRESS read-back
        }
        if (0xcfc..=0xcff).contains(&port) {
            // Config-data read (mechanism #1): the latched address selects the
            // device/register; the port offset within 0xcfc..0xcff selects the byte
            // within the register dword.
            let dword = Self::pci_config_dword(self.sys().pci_addr);
            let shift = 8 * u32::from(port - 0xcfc);
            return u64::from(dword >> shift) & Self::mask(size);
        }
        let mut v = 0u64;
        for i in 0..u64::from(size) {
            v |= u64::from(self.port_in(port.wrapping_add(i as u16))) << (8 * i);
        }
        v
    }

    /// A word/dword port write. The PCI config-address port (`0xcf8`) and config
    /// data are accepted and discarded (no PCI bridge); other ports compose into
    /// byte writes.
    fn port_out_wide(&mut self, port: u16, size: u8, val: u64) {
        if port == 0xcf8 {
            // CONFIG_ADDRESS — latch it so the kernel's mechanism-#1 detection
            // (write 0x80000000, read it back) succeeds; the config-data scan then
            // returns all-ones (no devices) and completes, instead of falling back
            // to the unhandled mechanism #2 (which wedged the boot).
            self.sys_mut().pci_addr = val as u32;
            return;
        }
        if (0xcfc..=0xcff).contains(&port) {
            return; // PCI config data — no host bridge / no devices
        }
        for i in 0..u64::from(size) {
            self.port_out(port.wrapping_add(i as u16), (val >> (8 * i)) as u8);
        }
    }

    // ── local APIC MMIO (the long-mode boot path; #12) ─────────────────────────

    /// Read a local-APIC register at byte offset `off` (the MMIO window at
    /// [`LAPIC_BASE`]). Only the registers a UP boot reads are modelled.
    fn lapic_read(&mut self, off: u32) -> u32 {
        let l = &self.sys().lapic;
        match off {
            0x020 => 0,               // APIC ID (CPU 0)
            0x030 => 0x0005_0014,     // version
            0x080 => l.tpr,           // TPR
            0x0f0 => l.svr,           // spurious vector
            0x320 => l.lvt_timer,     // LVT timer
            0x380 => l.initial_count, // timer initial count
            0x390 => l.current_count, // timer current count
            0x3e0 => l.divide,        // divide config
            0x100..=0x170 if off & 0xf == 0 => {
                let r = ((off - 0x100) / 0x10) as usize; // 8 x 32-bit ISR regs
                (l.isr[r / 2] >> (32 * (r & 1))) as u32
            }
            0x200..=0x270 if off & 0xf == 0 => {
                let r = ((off - 0x200) / 0x10) as usize; // 8 x 32-bit IRR regs
                (l.irr[r / 2] >> (32 * (r & 1))) as u32
            }
            _ => 0,
        }
    }

    /// Write a local-APIC register. Drives the spurious-vector (enable), the LVT
    /// timer, the timer count/divide, the TPR, and `EOI` (offset `0xb0`).
    fn lapic_write(&mut self, off: u32, val: u32) {
        let l = &mut self.sys_mut().lapic;
        match off {
            0x080 => l.tpr = val,
            0x0b0 => l.eoi(), // EOI
            0x0f0 => l.svr = val,
            0x320 => {
                l.lvt_timer = val;
            }
            0x380 => {
                l.initial_count = val;
                l.current_count = val;
            }
            0x3e0 => l.divide = val,
            0x300 => {
                // Interrupt Command Register (low): send an IPI. Bits 8-10 =
                // delivery mode (0 = fixed), bits 18-19 = destination shorthand
                // (1 = self, 2 = all-incl-self, 3 = all-excl-self). On this
                // uniprocessor the only target is CPU 0, so a fixed IPI to self,
                // all-incl-self, or a directed destination all deliver the vector
                // locally — the path the kernel's reschedule and `irq_work`
                // self-IPIs take. (NMI/INIT/SIPI and all-excl-self: no local
                // effect here.)
                let fixed = (val >> 8) & 7 == 0;
                let not_excl_self = (val >> 18) & 3 != 3;
                if fixed && not_excl_self {
                    l.set_irr((val & 0xff) as u8);
                }
            }
            _ => {}
        }
    }

    // ── shared device bus (virtio-mmio transport; CC-46) ─────────────────────

    /// Read a `virtio-mmio` register of one of the shared `devbus` devices at
    /// guest-physical `pa` — the x86-64 transport's side of the *same* devbus the
    /// RISC-V and AArch64 cores drive (Law L4; only the MMIO window differs).
    fn mmio_read(&mut self, pa: u64, _width: usize) -> u64 {
        if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            return super::devbus::blk_mmio_read(self.sys().virtio.as_ref(), pa - VIRTIO_BLK_BASE);
        }
        if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            return super::devbus::p9_mmio_read(self.sys().virtio9p.as_ref(), pa - VIRTIO_9P_BASE);
        }
        if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            return super::devbus::net_mmio_read(
                self.sys().virtionet.as_ref(),
                pa - VIRTIO_NET_BASE,
            );
        }
        if (VIRTIO_INPUT_BASE..VIRTIO_INPUT_END).contains(&pa) {
            return super::devbus::input_mmio_read(
                self.sys().virtioinput.as_ref(),
                pa - VIRTIO_INPUT_BASE,
            );
        }
        0
    }

    /// Write a `virtio-mmio` register of one of the shared `devbus` devices. A
    /// `QueueNotify` services the queue through the shared `devbus` over a
    /// [`GuestRam`](super::devbus::GuestRam) view of x86-64 RAM. Interrupt
    /// delivery is deferred to the long-mode boot path (`#12`, the APIC); the
    /// device-level witness drains the used ring directly (the same way the
    /// AArch64 witness reads the device's reply without taking the SPI).
    fn mmio_write(&mut self, pa: u64, _width: usize, value: u64) {
        if (VIRTIO_BLK_BASE..VIRTIO_BLK_END).contains(&pa) {
            self.virtio_blk_write(pa - VIRTIO_BLK_BASE, value as u32);
        } else if (VIRTIO_9P_BASE..VIRTIO_9P_END).contains(&pa) {
            self.virtio_9p_write(pa - VIRTIO_9P_BASE, value as u32);
        } else if (VIRTIO_NET_BASE..VIRTIO_NET_END).contains(&pa) {
            self.virtio_net_write(pa - VIRTIO_NET_BASE, value as u32);
        } else if (VIRTIO_INPUT_BASE..VIRTIO_INPUT_END).contains(&pa) {
            self.virtio_input_write(pa - VIRTIO_INPUT_BASE, value as u32);
        }
    }

    #[cfg(feature = "std")]
    #[doc(hidden)]
    pub fn vv_dbg(&self, vaddr: u64) {
        let pml4 = self.cr3 & 0x000f_ffff_ffff_f000;
        let idx = |lvl: u32| ((vaddr >> (12 + 9 * lvl)) & 0x1ff) * 8;
        let e4 = self.rd_phys(pml4 + idx(3), 8);
        std::eprintln!(
            "cr3={:#x} PML4[{}]={:#x}",
            self.cr3,
            (vaddr >> 39) & 0x1ff,
            e4
        );
        if e4 & 1 == 0 {
            std::eprintln!("  PML4 not present");
            return;
        }
        let e3 = self.rd_phys((e4 & 0x000f_ffff_ffff_f000) + idx(2), 8);
        std::eprintln!("  PDPT[{}]={:#x}", (vaddr >> 30) & 0x1ff, e3);
    }

    /// A guest-RAM view for the shared devbus to walk virtqueues over (x86-64 RAM
    /// is based at [`RAM_BASE`] = 0).
    fn guest_ram(&mut self) -> super::devbus::GuestRam<'_> {
        super::devbus::GuestRam {
            ram: &mut self.ram,
            base: RAM_BASE,
        }
    }

    /// A `virtio-blk` MMIO register write; a `QueueNotify` services the queue
    /// against the κ-disk through the shared `devbus` and raises the device's IRQ
    /// (`VIRTIO_BLK_IRQ`) so the guest's driver completes the request (`CC-45`).
    fn virtio_blk_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio.take() else {
            return;
        };
        let mut raise = false;
        if super::devbus::blk_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = super::devbus::blk_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_BLK_IRQ);
        }
    }

    /// A `virtio-9p` MMIO register write; a `QueueNotify` services the workspace
    /// filesystem queue through the shared `devbus` — the same servicing the
    /// other cores drive (`CC-46`).
    fn virtio_9p_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtio9p.take() else {
            return;
        };
        let mut raise = false;
        if super::devbus::p9_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = super::devbus::p9_service_queue(&mut mem, &mut dev);
        }
        self.sys_mut().virtio9p = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_9P_IRQ);
        }
    }

    /// A `virtio-net` MMIO register write; a `QueueNotify` services the transmit
    /// queue or pumps the NAT through the shared `devbus` (`CC-46`).
    fn virtio_net_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut raise = false;
        match super::devbus::net_mmio_write(&mut dev, off, value) {
            super::devbus::NetNotify::Transmit => {
                let mut mem = self.guest_ram();
                raise |= super::devbus::net_service_tx(&mut mem, &mut dev);
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::Receive => {
                let mut mem = self.guest_ram();
                raise |= super::devbus::net_pump(&mut mem, &mut dev);
            }
            super::devbus::NetNotify::None => {}
        }
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_NET_IRQ);
        }
    }

    /// Pump the NAT and deliver pending receive frames into the guest's receive
    /// queue — called periodically from the run loop so host-side data arrives
    /// without the guest having to transmit first (the same shared `devbus` pump
    /// the other cores drive).
    fn virtio_net_pump(&mut self) {
        let Some(mut dev) = self.sys_mut().virtionet.take() else {
            return;
        };
        let mut mem = self.guest_ram();
        let raise = super::devbus::net_pump(&mut mem, &mut dev);
        self.sys_mut().virtionet = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_NET_IRQ);
        }
    }

    /// A `virtio-input` MMIO register write; a `QueueNotify` services the notified
    /// queue (0 = eventq, 1 = statusq) through the shared `devbus` and raises the
    /// device IRQ so the guest's input driver makes progress (`CC-46`).
    fn virtio_input_write(&mut self, off: u64, value: u32) {
        let Some(mut dev) = self.sys_mut().virtioinput.take() else {
            return;
        };
        let mut raise = false;
        if let Some(q) = super::devbus::input_mmio_write(&mut dev, off, value) {
            let mut mem = self.guest_ram();
            raise = if q == 1 {
                super::devbus::input_service_statusq(&mut mem, &mut dev)
            } else {
                super::devbus::input_service_eventq(&mut mem, &mut dev)
            };
        }
        self.sys_mut().virtioinput = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_INPUT_IRQ);
        }
    }

    /// Drain any host-queued input events into the eventq and raise the IRQ —
    /// called after enqueuing events (and periodically from the run loop) so the
    /// guest's input layer + X event loop wake promptly.
    fn virtio_input_pump(&mut self) {
        let Some(mut dev) = self.sys_mut().virtioinput.take() else {
            return;
        };
        let mut mem = self.guest_ram();
        let raise = super::devbus::input_service_eventq(&mut mem, &mut dev);
        self.sys_mut().virtioinput = Some(dev);
        if raise {
            self.sys_mut().pic.raise(VIRTIO_INPUT_IRQ);
        }
    }

    #[inline]
    fn sys(&self) -> &Sys {
        self.sys.as_ref().expect("system mode")
    }
    #[inline]
    fn sys_mut(&mut self) -> &mut Sys {
        self.sys.as_mut().expect("system mode")
    }

    // ── device attach + the shared workspace / network surface (CC-46) ───────

    /// Attach a `virtio-blk` κ-disk rootfs (`CC-7`) — the same κ-disk the other
    /// cores boot, serviced by the shared `devbus`.
    pub fn attach_disk(&mut self, rootfs: Vec<u8>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtio = Some(super::VirtioBlk::new(rootfs));
        }
    }

    /// Attach a `virtio-input` device (keyboard + relative pointer; `CC-46`). The
    /// kernel binds it (CONFIG_VIRTIO_INPUT) → `evdev` exposes `/dev/input/eventN`
    /// → `libinput` feeds the X server, so host events drive both interactivity
    /// and the X main loop (the shadow → scanout flush). The fourth virtio-mmio
    /// slot must be on the kernel command line:
    /// `virtio_mmio.device=0x200@0xd0000600:13`.
    pub fn attach_virtio_input(&mut self) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtioinput = Some(super::VirtioInput::new());
        }
    }

    /// Queue a key (or mouse-button) press/release on the `virtio-input` device,
    /// followed by a `SYN_REPORT`, and deliver it. `code` is an evdev `KEY_*` /
    /// `BTN_*` code; `down` = pressed. No-op if no input device is attached.
    pub fn input_key(&mut self, code: u16, down: bool) {
        const EV_KEY: u16 = 0x01;
        if let Some(dev) = self.sys_mut().virtioinput.as_mut() {
            dev.push_event(EV_KEY, code, if down { 1 } else { 0 });
            Self::push_syn(dev);
        }
        self.virtio_input_pump();
    }

    /// Queue a relative pointer motion (`dx`, `dy`) followed by a `SYN_REPORT`,
    /// and deliver it. No-op if no input device is attached.
    pub fn input_motion(&mut self, dx: i32, dy: i32) {
        const EV_REL: u16 = 0x02;
        const REL_X: u16 = 0x00;
        const REL_Y: u16 = 0x01;
        if let Some(dev) = self.sys_mut().virtioinput.as_mut() {
            if dx != 0 {
                dev.push_event(EV_REL, REL_X, dx);
            }
            if dy != 0 {
                dev.push_event(EV_REL, REL_Y, dy);
            }
            Self::push_syn(dev);
        }
        self.virtio_input_pump();
    }

    /// Queue a vertical scroll-wheel step (`+1` up / `-1` down) + `SYN_REPORT`.
    pub fn input_wheel(&mut self, clicks: i32) {
        const EV_REL: u16 = 0x02;
        const REL_WHEEL: u16 = 0x08;
        if let Some(dev) = self.sys_mut().virtioinput.as_mut() {
            dev.push_event(EV_REL, REL_WHEEL, clicks);
            Self::push_syn(dev);
        }
        self.virtio_input_pump();
    }

    /// Append the `EV_SYN`/`SYN_REPORT` that terminates an evdev event packet.
    fn push_syn(dev: &mut super::VirtioInput) {
        const EV_SYN: u16 = 0x00;
        const SYN_REPORT: u16 = 0x00;
        dev.push_event(EV_SYN, SYN_REPORT, 0);
    }

    /// Attach a shared **workspace filesystem** to the `virtio-9p` device
    /// (`CC-15` parity). `seed` is the files holospaces places on the share; the
    /// guest mounts it (tag `hsworkspace`) and the editor and the running OS read
    /// and write the *same* files over the shared `devbus`.
    pub fn attach_workspace(&mut self, seed: &[(&str, &[u8])]) {
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        let mut fs = super::ninep::Fs9p::new();
        for (name, data) in seed {
            fs.seed_file(name, data);
        }
        sys.virtio9p = Some(super::Virtio9p::new(fs, super::WORKSPACE_TAG));
    }

    /// Read a file from the shared workspace filesystem — how holospaces observes
    /// the edits the guest made over 9P (`CC-15`).
    #[must_use]
    pub fn workspace_file(&self, name: &str) -> Option<&[u8]> {
        self.sys
            .as_ref()
            .and_then(|s| s.virtio9p.as_ref())
            .and_then(|d| d.fs.read_file(name))
    }

    /// Write a file into the shared workspace — the editor saving content the
    /// running OS reads over `virtio-9p` (one content, Law L1; `CC-17`).
    pub fn workspace_write(&mut self, name: &str, data: &[u8]) {
        if let Some(d) = self.sys.as_mut().and_then(|s| s.virtio9p.as_mut()) {
            d.fs.write_file(name, data);
        }
    }

    /// Attach the **VirtIO network device** + the userspace TCP/IP NAT (`CC-16`
    /// parity): the guest drives a real NIC, its frames terminate in the shared
    /// [`net`](super::net) NAT and stream out over `egress`.
    pub fn attach_net(&mut self, egress: Box<dyn super::net::Egress>) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtionet = Some(super::VirtioNet::new(
                egress,
                Box::new(super::net::NoIngress),
            ));
        }
    }

    /// Attach the network device with both an `egress` (outbound) and an
    /// `ingress` (forwarded-port inbound) transport (`CC-16` + `CC-21` parity) —
    /// the x86-64 analogue of [`Emulator::attach_net_forward`](super::Emulator::attach_net_forward).
    /// A server the guest runs on a forwarded port is then reachable from outside
    /// over `ingress` (a host socket via [`net::StdIngress`](super::net::StdIngress)),
    /// the running-app preview. A no-op if the machine has no device subsystem.
    pub fn attach_net_forward(
        &mut self,
        egress: Box<dyn super::net::Egress>,
        ingress: Box<dyn super::net::Ingress>,
    ) {
        if let Some(sys) = self.sys.as_mut() {
            sys.virtionet = Some(super::VirtioNet::new(egress, ingress));
        }
    }

    /// Re-attach real network transports to a device **restored from a κ-snapshot** (which carries the
    /// negotiated virtio-net registers + `last_avail` but placeholder `NoEgress`/`NoIngress`
    /// transports). Unlike [`Cpu::attach_net_forward`], this PRESERVES the restored device state (queue
    /// addresses, consumed position) instead of building a fresh device — so a warm-snapshotted server
    /// keeps serving and stays reachable after resume. Returns `false` if no device was restored.
    pub fn reattach_net_forward(
        &mut self,
        egress: Box<dyn super::net::Egress>,
        ingress: Box<dyn super::net::Ingress>,
    ) -> bool {
        let Some(net) = self.sys.as_mut().and_then(|s| s.virtionet.as_mut()) else {
            return false;
        };
        net.set_transports(egress, ingress);
        true
    }

    /// Enable the **in-process loopback bridge** (ADR-020, `CC-33` parity) on the
    /// already-attached network device: the workbench (same process) can
    /// `dial`/`send`/`recv`/`close` a connection to a server *inside* the guest.
    /// Returns `false` if no network device is attached.
    pub fn enable_loopback(&mut self) -> bool {
        let Some(net) = self.sys.as_mut().and_then(|s| s.virtionet.as_mut()) else {
            return false;
        };
        let (ingress, handle) = super::net::LoopbackIngress::new();
        net.ingress = Box::new(ingress);
        self.sys_mut().loopback = Some(handle);
        true
    }

    /// Dial an in-process connection to the guest's listening `guest_port` over
    /// the loopback bridge (`CC-33`). Returns the connection id, or `None` if the
    /// loopback ingress is not enabled.
    pub fn dial_guest(&mut self, guest_port: u16) -> Option<u32> {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .map(|h| h.dial(guest_port))
    }

    /// Write host bytes toward the guest server on a loopback connection.
    pub fn guest_send(&mut self, id: u32, data: &[u8]) {
        if let Some(h) = self.sys.as_ref().and_then(|s| s.loopback.as_ref()) {
            h.send(id, data);
        }
    }

    /// Drain the guest server's reply bytes on a loopback connection.
    #[must_use]
    pub fn guest_recv(&mut self, id: u32) -> Vec<u8> {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .map(|h| h.recv(id))
            .unwrap_or_default()
    }

    /// Close the host side of a loopback connection.
    pub fn guest_close(&mut self, id: u32) {
        if let Some(h) = self.sys.as_ref().and_then(|s| s.loopback.as_ref()) {
            h.close(id);
        }
    }

    /// Whether a loopback connection is still usable.
    #[must_use]
    pub fn guest_is_open(&self, id: u32) -> bool {
        self.sys
            .as_ref()
            .and_then(|s| s.loopback.as_ref())
            .is_some_and(|h| h.is_open(id))
    }

    // ── V&V device-driver hooks (CC-46) ──────────────────────────────────────

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO store at the
    /// guest-physical address `pa`, exactly as a guest's `virtio` driver would —
    /// the same `mmio_write` the executing CPU routes device stores through. A
    /// conformance witness uses it to drive the shared `devbus` devices over the
    /// x86-64 MMIO transport without booting a full guest kernel.
    #[doc(hidden)]
    pub fn vv_mmio_write(&mut self, pa: u64, width: usize, value: u64) {
        self.mmio_write(pa, width, value);
    }

    /// **V&V device-driver hook** (`CC-46`): perform a device-MMIO load at `pa` —
    /// the same path the executing CPU routes device loads through.
    #[doc(hidden)]
    pub fn vv_mmio_read(&mut self, pa: u64, width: usize) -> u64 {
        self.mmio_read(pa, width)
    }

    /// **V&V hook** (`CC-46`): write `bytes` into guest RAM at guest-physical
    /// `pa` — a witness lays out the virtqueue and the T-message buffers a guest
    /// driver would build in RAM.
    #[doc(hidden)]
    pub fn vv_ram_write(&mut self, pa: u64, bytes: &[u8]) {
        let o = (pa - RAM_BASE) as usize;
        self.ram[o..o + bytes.len()].copy_from_slice(bytes);
    }

    /// **V&V hook** (`CC-46`): read `len` bytes of guest RAM at guest-physical
    /// `pa` — a witness reads back the R-message the device scattered and the
    /// used-ring the device updated.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_ram_read(&self, pa: u64, len: usize) -> Vec<u8> {
        let o = (pa - RAM_BASE) as usize;
        self.ram[o..o + len].to_vec()
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-9p` MMIO
    /// slot — a witness drives the device at the same address the x86-64 platform
    /// advertises.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_9p_base() -> u64 {
        VIRTIO_9P_BASE
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-net` slot.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_net_base() -> u64 {
        VIRTIO_NET_BASE
    }

    /// **V&V hook** (`CC-46`): the guest-physical base of the `virtio-blk` slot.
    #[doc(hidden)]
    #[must_use]
    pub fn vv_virtio_blk_base() -> u64 {
        VIRTIO_BLK_BASE
    }

    /// Decode + execute one instruction.
    #[allow(clippy::too_many_lines)]
    fn step(&mut self) -> Result<(), Halt> {
        let start = self.rip;
        #[cfg(feature = "cc44-trace")]
        if self.cpl == 3 {
            rip_ring_push(start, self.r[RSP], self.r[RBP]);
            // One-shot: dump the suspect callee's bytes so it can be disassembled.
            if let Ok(want) = std::env::var("HOLO_DUMP_RIP") {
                if let Ok(w) = u64::from_str_radix(want.trim_start_matches("0x"), 16) {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static DONE: AtomicBool = AtomicBool::new(false);
                    if start == w && !DONE.swap(true, Ordering::Relaxed) {
                        let mut b = [0u8; 0x80];
                        for (k, x) in b.iter_mut().enumerate() {
                            let pa = self.translate_acc(start.wrapping_add(k as u64), false, true);
                            *x = *self.ram.get(pa as usize).unwrap_or(&0);
                        }
                        self.fault = None;
                        use std::io::Write as _;
                        let _ = writeln!(std::io::stderr(), "[cc44-trace] DUMP-RIP@{start:#x}={b:02x?}");
                    }
                }
            }
            // Targeted: musl fork()'s post-clone `test ebp,ebp` (ld-musl base
            // 0x7ffff7f5c000 + fork+0x176). ebp = clone return; qemu sees 0/pid,
            // we (suspected) see garbage → the child's rax wrong on clone-return.
            if start == 0x7fff_f7fa_0065 {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] FORK-TEST rbp={:#x} ebp={:#x} rax={:#x} r13={:#x}",
                    self.r[RBP], self.r[RBP] as u32, self.r[0], self.r[13],
                );
            }
            // The `movups %xmm0,0x10(%rdx)` struct-zeroing store in _Fork's caller.
            // If xmm0 != 0 here, an upstream SSE op failed to zero it → garbage
            // (a TLS pointer) lands in the struct → the corrupt rbp downstream.
            if start == 0x7fff_f7f9_f918 {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "[cc44-trace] MOVUPS-ZERO xmm0={:#034x} rdx={:#x} al={:#x}",
                    self.xmm[0], self.r[RDX], self.r[0] & 0xff,
                );
            }
        }
        // Kernel probe: the child's `ret_from_fork_asm` (rsp = pt_regs). Dump the
        // pt_regs the child will restore on its way to userspace — if rbp/r13 are
        // already the stale TLS pointer here, copy_thread set up pt_regs wrong;
        // if correct, the restore/context-switch corrupts them. (x86-64 pt_regs:
        // r13@0x10, rbp@0x20, rax@0x50, rip@0x80, rsp@0x98.)
        #[cfg(feature = "cc44-trace")]
        if start == 0xffff_ffff_8100_1be0 {
            let pr = self.r[RSP];
            let g = |o: u64| self.rd_virt(pr.wrapping_add(o), 8);
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[cc44-trace] RET-FROM-FORK pt_regs@{pr:#x} r13={:#x} rbp={:#x} rax={:#x} user_rip={:#x} user_rsp={:#x}",
                g(0x10), g(0x20), g(0x50), g(0x80), g(0x98),
            );
            self.fault = None;
        }
        #[cfg(feature = "cc44-trace")]
        if self.cpl == 3 && selfck_on() {
            // Per-instruction self-consistency capture (see the module note + scratchpad
            // analyzers): rip, 16 opcode bytes, rflags, all GPRs, 32 bytes @ each low
            // GPR (disp-0..24 memory operands + 16-byte SSE loads), and the XMM file.
            let mut op = [0u8; 16];
            for (k, x) in op.iter_mut().enumerate() {
                let pa = self.translate_acc(start.wrapping_add(k as u64), false, true) as usize;
                *x = *self.ram.get(pa).unwrap_or(&0);
            }
            self.fault = None;
            let mut line = format!("{start:x} ");
            for x in op {
                line.push_str(&format!("{x:02x}"));
            }
            line.push_str(&format!(" {:x}", self.rflags));
            for g in self.r {
                line.push_str(&format!(" {g:x}"));
            }
            for i in 0..8usize {
                for w in 0..4u64 {
                    line.push_str(&format!(" {:x}", self.rd_virt(self.r[i].wrapping_add(w * 8), 8)));
                }
            }
            for x in self.xmm {
                line.push_str(&format!(" {x:x}"));
            }
            line.push('\n');
            selfck_push(line);
        }
        // Snapshot the architectural register state so a `#PF` latched mid-access
        // can discard this instruction's partial effects and restart it after the
        // handler maps the page (RAM is not snapshotted — early boot's faulting
        // accesses touch a fresh page, so any bytes written before the fault are
        // re-written identically on restart).
        let snap = self.reg_snapshot();
        let mut rex = 0u8;
        let mut opsz = false; // 0x66 operand-size override
        let mut rep = RepKind::None; // F3 (REP/REPE) / F2 (REPNE)
        self.cur_seg = None;
        self.rex_present = false;
        loop {
            let user = self.cpl == 3;
            let p = self.translate_acc(self.rip, false, user) as usize;
            // A code-fetch that hit a not-present page latches a `#PF` and makes
            // `translate_acc` return phys 0 from here on. Stop decoding NOW — otherwise
            // `b` reads the phys-0 scratch every iteration, and if that byte happens to be
            // a legal prefix (`0x66`/`0xf3`/`0x64`/REX/…) the `_ => break` arm never fires
            // and this loop spins forever inside one `step()` (a real hang: `/init`'s text
            // is demand-paged, and a prior faulting `rep stos`/`movs` can leave a
            // prefix-valued byte in the scratch). Break so `step` vectors the `#PF` and the
            // instruction re-runs once the handler maps the page.
            if self.fault.is_some() {
                break;
            }
            let b = *self.ram.get(p).unwrap_or(&0);
            match b {
                0x66 => opsz = true,
                0xf3 => rep = RepKind::Rep,
                0xf2 => rep = RepKind::Repne,
                0x64 => self.cur_seg = Some(SegId::Fs),
                0x65 => self.cur_seg = Some(SegId::Gs),
                0x67 | 0xf0 | 0x2e | 0x36 | 0x3e | 0x26 => {}
                0x40..=0x4f => {
                    rex = b; // REX (last prefix)
                    self.rex_present = true;
                }
                _ => break,
            }
            self.rip = self.rip.wrapping_add(1);
            if (0x40..=0x4f).contains(&b) {
                break; // REX must be the final prefix
            }
        }
        let op = self.fetch_u8();
        #[cfg(feature = "std")]
        if OPHIST_ON.load(core::sync::atomic::Ordering::Relaxed) {
            OPHIST.with(|h| h.borrow_mut()[op as usize] += 1);
        }
        // An instruction FETCH that hit a not-present code page (a lazily-mapped
        // user text page — `ld-musl`, the program binary — demand-paged on first
        // execution) latched a `#PF` in `translate_acc`. Deliver it (CR2 = the
        // faulting rip) so the kernel pages the code in and re-runs, instead of
        // decoding the garbage byte the faulting read returned. Mirrors the
        // operand-fault path below; without it the first call into an unpaged text
        // page decodes phys-0 and dies `Undefined`.
        if self.fault.is_some() {
            let pf = self.fault.take().expect("fetch fault");
            self.restore_snapshot(snap);
            self.rip = start;
            self.cr2 = pf.addr;
            self.raise_exception(VEC_PAGE_FAULT, pf.error, true);
            return Ok(());
        }
        let size: u8 = if rex & 8 != 0 {
            8
        } else if opsz {
            2
        } else {
            4
        };
        match op {
            // ── ALU group (add/or/adc/sbb/and/sub/xor/cmp), all six forms ──
            0x00 | 0x08 | 0x10 | 0x18 | 0x20 | 0x28 | 0x30 | 0x38 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.reg8(reg));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x01 | 0x09 | 0x11 | 0x19 | 0x21 | 0x29 | 0x31 | 0x39 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x02 | 0x0a | 0x12 | 0x1a | 0x22 | 0x2a | 0x32 | 0x3a => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.reg8(reg), self.load_rm(rm, 1));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(reg), 1, res);
                }
            }
            0x03 | 0x0b | 0x13 | 0x1b | 0x23 | 0x2b | 0x33 | 0x3b => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.r[reg] & Self::mask(size), self.load_rm(rm, size));
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(reg), size, res);
                }
            }
            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                let res = self.alu(op >> 3, a, b, 1);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(0), 1, res);
                }
            }
            0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d => {
                let b = self.fetch_imm_z(size);
                let a = self.r[0] & Self::mask(size);
                let res = self.alu(op >> 3, a, b, size);
                if op >> 3 != 7 {
                    self.store_rm(Rm::Reg(0), size, res);
                }
            }
            0x50..=0x57 => {
                let r = (op - 0x50) as usize | (((rex & 1) as usize) << 3);
                let v = self.r[r];
                self.push(v);
            }
            0x58..=0x5f => {
                let r = (op - 0x58) as usize | (((rex & 1) as usize) << 3);
                let v = self.pop();
                self.r[r] = v;
            }
            0x68 => {
                let imm = self.fetch(4) as i32 as i64 as u64;
                self.push(imm);
            }
            0x6a => {
                let imm = self.fetch(1) as i8 as i64 as u64;
                self.push(imm);
            }
            0x70..=0x7f => {
                let rel = self.fetch(1) as i8 as i64;
                if self.cond(op - 0x70) {
                    self.rip = self.rip.wrapping_add(rel as u64);
                }
            }
            0x80 => {
                // The immediate is fetched *before* the operand is touched so a
                // RIP-relative `rm` resolves against the instruction-end `rip`
                // (the same address for the load and the store).
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch(1);
                let a = self.load_rm(rm, 1);
                let res = self.alu((ext & 7) as u8, a, b, 1);
                if ext & 7 != 7 {
                    self.store_rm(rm, 1, res);
                }
            }
            0x81 => {
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch_imm_z(size);
                let a = self.load_rm(rm, size);
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x83 => {
                let (ext, rm) = self.modrm(rex);
                let b = self.fetch(1) as i8 as i64 as u64;
                let a = self.load_rm(rm, size);
                let res = self.alu((ext & 7) as u8, a, b, size);
                if ext & 7 != 7 {
                    self.store_rm(rm, size, res);
                }
            }
            0x84 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, 1), self.reg8(reg));
                self.flags_logic(a & b, 1);
            }
            0x85 => {
                let (reg, rm) = self.modrm(rex);
                let (a, b) = (self.load_rm(rm, size), self.r[reg] & Self::mask(size));
                self.flags_logic(a & b, size);
            }
            0x88 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.reg8(reg);
                self.store_rm(rm, 1, v);
            }
            0x89 => {
                let (reg, rm) = self.modrm(rex);
                let v = self.r[reg] & Self::mask(size);
                self.store_rm(rm, size, v);
            }
            0x8a => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 1);
                self.store_rm(Rm::Reg(reg), 1, v);
            }
            0x8b => {
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, size);
                self.store_rm(Rm::Reg(reg), size, v);
            }
            0x8c => {
                // MOV r/m16, Sreg — store a segment selector.
                let (reg, rm) = self.modrm(rex);
                let sel = u64::from(self.seg.get(reg).map_or(0, |s| s.selector));
                self.store_rm(rm, 2, sel);
            }
            0x8d => {
                let (reg, rm) = self.modrm(rex);
                if let Some(a) = self.rm_addr(rm) {
                    self.store_rm(Rm::Reg(reg), size, a & Self::mask(size));
                }
            }
            0x8e => {
                // MOV Sreg, r/m16 — load a segment selector. In long mode the
                // descriptor base/limit are ignored for DS/ES/SS/CS (flat); only
                // the selector is recorded (FS/GS bases come from their MSRs).
                let (reg, rm) = self.modrm(rex);
                let sel = self.load_rm(rm, 2) as u16;
                if let Some(s) = self.seg.get_mut(reg) {
                    s.selector = sel;
                }
            }
            0x90..=0x97 => {
                // XCHG rAX, reg. 0x90 (no REX.B) = NOP / PAUSE (F3); REX.B extends
                // the reg, so 0x90+REX.B is a real xchg with r8.
                let reg = (op - 0x90) as usize | (((rex & 1) as usize) << 3);
                if reg != 0 {
                    let m = Self::mask(size);
                    let (a, b) = (self.r[0] & m, self.r[reg] & m);
                    self.store_rm(Rm::Reg(0), size, b);
                    self.store_rm(Rm::Reg(reg), size, a);
                }
            }
            0xa8 => {
                let (a, b) = (self.r[0] & 0xff, self.fetch(1));
                self.flags_logic(a & b, 1);
            }
            0xa9 => {
                let a = self.r[0] & Self::mask(size);
                let b = self.fetch_imm_z(size);
                self.flags_logic(a & b, size);
            }
            0xb0..=0xb7 => {
                let r = (op - 0xb0) as usize | (((rex & 1) as usize) << 3);
                let imm = self.fetch(1);
                self.store_rm(Rm::Reg(r), 1, imm);
            }
            0xb8..=0xbf => {
                let r = (op - 0xb8) as usize | (((rex & 1) as usize) << 3);
                let imm = self.fetch(size); // imm16 / imm32 / imm64 by operand size
                self.store_rm(Rm::Reg(r), size, imm);
            }
            0x63 => {
                // MOVSXD r64, r/m32 — sign-extend a 32-bit operand into 64 bits.
                let (reg, rm) = self.modrm(rex);
                let v = self.load_rm(rm, 4) as u32 as i32 as i64 as u64;
                self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
            }
            0x69 => {
                // IMUL r, r/m, imm (imm16 under 0x66, else imm32 sign-extended).
                let (reg, rm) = self.modrm(rex);
                let imm = if size == 2 {
                    self.fetch(2) as i16 as i64
                } else {
                    self.fetch(4) as i32 as i64
                };
                let a = sign_extend(self.load_rm(rm, size), size);
                let full = i128::from(a) * i128::from(imm);
                let r = full as u64;
                self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
                self.set_imul_flags(full, r, size);
            }
            0x6b => {
                // IMUL r, r/m, imm8.
                let (reg, rm) = self.modrm(rex);
                let imm = self.fetch(1) as i8 as i64;
                let a = sign_extend(self.load_rm(rm, size), size);
                let full = i128::from(a) * i128::from(imm);
                let r = full as u64;
                self.store_rm(Rm::Reg(reg), size, r & Self::mask(size));
                self.set_imul_flags(full, r, size);
            }
            0x86 => {
                // XCHG r/m8, r8.
                let (reg, rm) = self.modrm(rex);
                let a = self.load_rm(rm, 1);
                let b = self.reg8(reg);
                self.store_rm(rm, 1, b);
                self.store_rm(Rm::Reg(reg), 1, a);
            }
            0x87 => {
                // XCHG r/m, r.
                let (reg, rm) = self.modrm(rex);
                let a = self.load_rm(rm, size);
                let b = self.r[reg] & Self::mask(size);
                self.store_rm(rm, size, b);
                self.store_rm(Rm::Reg(reg), size, a);
            }
            0x8f => {
                // POP r/m64. Per the SDM, when rSP is the base of the
                // destination's effective address, the address is computed
                // *after* rSP is incremented by the pop — so pop first, then
                // decode the ModRM address (e.g. `pop 0x20(%rsp)`).
                let v = self.pop();
                let (_e, rm) = self.modrm(rex);
                self.store_rm(rm, 8, v);
            }
            0x98 => {
                // CBW/CWDE/CDQE — sign-extend AL/AX/EAX to the operand size.
                let v = match size {
                    8 => self.r[RAX] as i32 as i64 as u64,
                    2 => (self.r[RAX] as i8 as i16 as u16) as u64,
                    _ => self.r[RAX] as i16 as i32 as u32 as u64,
                };
                self.store_rm(Rm::Reg(RAX), size, v);
            }
            0x99 => {
                // CWD/CDQ/CQO — sign-extend rax into rdx:rax.
                let neg = Self::sign(self.r[RAX], size);
                let ext = if neg { Self::mask(size) } else { 0 };
                self.store_rm(Rm::Reg(RDX), size, ext);
            }
            0x9c => {
                // PUSHFQ.
                self.push(self.rflags);
            }
            0x9d => {
                // POPFQ — restore the settable flags (CF/PF/AF/ZF/SF/TF/IF/DF/
                // OF/NT/AC/ID). The mask must include IF (bit 9, 0x200): the
                // kernel's `local_irq_restore` is `push; popfq`, so dropping IF
                // here would leave interrupts disabled across every
                // `local_irq_save/restore` region (the `irqs disabled` WARNs).
                // Same settable mask as IRETQ below.
                self.rflags = (self.pop() & 0x0024_4fd5) | 0x2;
            }
            0xa4 => self.string_op(StringOp::Movs, 1, rep, start),
            0xa5 => self.string_op(StringOp::Movs, size, rep, start),
            0xa6 => self.string_op(StringOp::Cmps, 1, rep, start),
            0xa7 => self.string_op(StringOp::Cmps, size, rep, start),
            0xaa => self.string_op(StringOp::Stos, 1, rep, start),
            0xab => self.string_op(StringOp::Stos, size, rep, start),
            0xac => self.string_op(StringOp::Lods, 1, rep, start),
            0xad => self.string_op(StringOp::Lods, size, rep, start),
            0xae => self.string_op(StringOp::Scas, 1, rep, start),
            0xaf => self.string_op(StringOp::Scas, size, rep, start),
            0xc0 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = self.fetch(1) as u8;
                self.shift_rotate((ext & 7) as u8, rm, 1, cnt);
            }
            0xc1 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = self.fetch(1) as u8;
                self.shift_rotate((ext & 7) as u8, rm, size, cnt);
            }
            0xd0 => {
                let (ext, rm) = self.modrm(rex);
                self.shift_rotate((ext & 7) as u8, rm, 1, 1);
            }
            0xd1 => {
                let (ext, rm) = self.modrm(rex);
                self.shift_rotate((ext & 7) as u8, rm, size, 1);
            }
            0xd2 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = (self.r[RCX] & 0xff) as u8;
                self.shift_rotate((ext & 7) as u8, rm, 1, cnt);
            }
            0xd3 => {
                let (ext, rm) = self.modrm(rex);
                let cnt = (self.r[RCX] & 0xff) as u8;
                self.shift_rotate((ext & 7) as u8, rm, size, cnt);
            }
            0xc2 => {
                // RET imm16 — pop the return address, then pop `imm16` arg bytes.
                let n = self.fetch(2);
                let v = self.pop();
                self.rip = v;
                self.r[RSP] = self.r[RSP].wrapping_add(n);
            }
            0xc9 => {
                // LEAVE: rsp = rbp; rbp = pop().
                self.r[RSP] = self.r[RBP];
                let v = self.pop();
                self.r[RBP] = v;
            }
            0xcf => {
                // IRETQ — return from an interrupt: pop RIP, CS, RFLAGS, RSP, SS.
                let rip = self.pop();
                let cs = self.pop();
                let rflags = self.pop();
                let rsp = self.pop();
                let ss = self.pop();
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.cpl = (cs & 3) as u8;
                // Restore the saved RFLAGS. The mask keeps the user-settable
                // status/control bits — crucially `IF` (bit 9): the timer/`virtio`
                // IRQ handlers `IRETQ` back into code that must run with interrupts
                // *enabled* (e.g. `calibrate_delay` waiting on `jiffies`), so
                // dropping `IF` here would wedge the boot in that wait. Bit 1 is the
                // architecturally-always-set reserved bit.
                self.rflags = (rflags & 0x0024_4fd5) | 0x2;
                self.r[RSP] = rsp;
                self.seg[SegId::Ss as usize].selector = ss as u16;
            }
            0xe3 => {
                // JRCXZ rel8.
                let rel = self.fetch(1) as i8 as i64;
                if self.r[RCX] == 0 {
                    self.rip = self.rip.wrapping_add(rel as u64);
                }
            }
            0xf5 => self.set(flag::CF, self.rflags & flag::CF == 0), // CMC
            0xf6 | 0xf7 => {
                let osz = if op == 0xf6 { 1 } else { size };
                self.group3(rex, osz, start)?;
            }
            0xf8 => self.set(flag::CF, false), // CLC
            0xf9 => self.set(flag::CF, true),  // STC
            0xfa => self.rflags &= !RFLAGS_IF, // CLI
            0xfb => self.rflags |= RFLAGS_IF,  // STI
            0xfc => self.rflags &= !RFLAGS_DF, // CLD
            0xfd => self.rflags |= RFLAGS_DF,  // STD
            0xc3 => {
                let v = self.pop();
                self.rip = v;
            }
            0xcb => {
                // LRET (far return): pop RIP then CS. The operand size (REX.W /
                // default) sets the slot width; the selector is the low 16 bits.
                let osz = if rex & 8 != 0 { 8 } else { 4 };
                let rip = self.pop_sized(osz);
                let cs = self.pop_sized(osz);
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.cpl = (cs & 3) as u8;
            }
            0xca => {
                // LRET imm16 (far return, freeing imm16 stack bytes).
                let n = self.fetch(2);
                let osz = if rex & 8 != 0 { 8 } else { 4 };
                let rip = self.pop_sized(osz);
                let cs = self.pop_sized(osz);
                self.rip = rip;
                self.seg[SegId::Cs as usize].selector = cs as u16;
                self.seg[SegId::Cs as usize].long = true;
                self.r[RSP] = self.r[RSP].wrapping_add(n);
            }
            0xc6 => {
                let (_e, rm) = self.modrm(rex);
                let imm = self.fetch(1);
                self.store_rm(rm, 1, imm);
            }
            0xc7 => {
                let (_e, rm) = self.modrm(rex);
                let v = self.fetch_imm_z(size);
                self.store_rm(rm, size, v);
            }
            0xe4 => {
                let port = self.fetch(1) as u16;
                let v = self.port_in(port);
                self.store_rm(Rm::Reg(0), 1, u64::from(v));
            }
            0xe6 => {
                let port = self.fetch(1) as u16;
                let v = (self.r[0] & 0xff) as u8;
                self.port_out(port, v);
            }
            0xe5 => {
                // IN eAX, imm8 — a word/dword port read (operand size).
                let port = self.fetch(1) as u16;
                let v = self.port_in_wide(port, size);
                self.store_rm(Rm::Reg(0), size, v);
            }
            0xe7 => {
                // OUT imm8, eAX — a word/dword port write.
                let port = self.fetch(1) as u16;
                let v = self.r[0] & Self::mask(size);
                self.port_out_wide(port, size, v);
            }
            0xed => {
                // IN eAX, dx — a word/dword port read.
                let port = (self.r[RDX] & 0xffff) as u16;
                let v = self.port_in_wide(port, size);
                self.store_rm(Rm::Reg(0), size, v);
            }
            0xef => {
                // OUT dx, eAX — a word/dword port write.
                let port = (self.r[RDX] & 0xffff) as u16;
                let v = self.r[0] & Self::mask(size);
                self.port_out_wide(port, size, v);
            }
            0xe8 => {
                let rel = self.fetch(4) as i32 as i64;
                let ret = self.rip;
                self.push(ret);
                let target = self.rip.wrapping_add(rel as u64);
                // Retpoline fast path. `call __x86_indirect_thunk_rXX` is the Spectre-v2
                // mitigation for an indirect `call *rXX`; with no speculation (this is an
                // emulator) it is *exactly* `call *rXX`. It is the #1 hot block on a real
                // boot (~13% of block entries) — recognise the thunk body and jump straight
                // to the register, skipping ~5 interpreted instructions per indirect call.
                // `push(ret)` above already supplied the outer return address; the thunk's
                // own push/mov/ret net to zero on rsp, so this is byte‑accurate.
                self.rip = match (fastpaths_on(), self.retpoline_reg(target)) {
                    (true, Some(reg)) => self.r[reg],
                    _ => target,
                };
            }
            0xe9 => {
                let rel = self.fetch(4) as i32 as i64;
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xeb => {
                let rel = self.fetch(1) as i8 as i64;
                self.rip = self.rip.wrapping_add(rel as u64);
            }
            0xec => {
                let port = (self.r[2] & 0xffff) as u16;
                let v = self.port_in(port);
                self.store_rm(Rm::Reg(0), 1, u64::from(v));
            }
            0xee => {
                let port = (self.r[2] & 0xffff) as u16;
                let v = (self.r[0] & 0xff) as u8;
                self.port_out(port, v);
            }
            0xf4 => {
                // HLT: with interrupts enabled the CPU idles until the next
                // interrupt — fast-forward the platform timers so a tick is
                // delivered rather than busy-spinning a step at a time. With
                // interrupts masked it is a true stop (the guest power-off).
                if self.rflags & RFLAGS_IF == 0 {
                    if let Some(sys) = self.sys.as_mut() {
                        sys.halted = true;
                    }
                    return Err(Halt::Halted);
                }
                // Leave `rip` *past* the HLT — the architectural state a wakeup
                // interrupt resumes from. The interrupt that ends the halt pushes
                // this address, so `IRET` returns to the instruction after HLT
                // (e.g. the `cli; ret` tail of `default_idle`), letting the idle
                // loop re-check `need_resched` and run a task woken by the timer
                // IRQ. Resetting `rip` back onto HLT would make `IRET` re-enter the
                // halt forever, stranding the woken task — the NO_HZ idle deadlock.
                self.idle_until_interrupt();
            }
            0xfe => {
                let (ext, rm) = self.modrm(rex);
                let a = self.load_rm(rm, 1);
                let sub = ext & 7 == 1;
                let r = if sub {
                    a.wrapping_sub(1)
                } else {
                    a.wrapping_add(1)
                };
                let cf = self.rflags & flag::CF;
                self.flags_arith(a, 1, r, 1, sub);
                self.rflags = (self.rflags & !flag::CF) | cf; // inc/dec preserve CF
                self.store_rm(rm, 1, r);
            }
            0xff => {
                let (ext, rm) = self.modrm(rex);
                match ext & 7 {
                    0 | 1 => {
                        let a = self.load_rm(rm, size);
                        let sub = ext & 7 == 1;
                        let r = if sub {
                            a.wrapping_sub(1)
                        } else {
                            a.wrapping_add(1)
                        };
                        let cf = self.rflags & flag::CF;
                        self.flags_arith(a, 1, r, size, sub);
                        self.rflags = (self.rflags & !flag::CF) | cf;
                        self.store_rm(rm, size, r);
                    }
                    2 => {
                        let t = self.load_rm(rm, 8);
                        let ret = self.rip;
                        self.push(ret);
                        self.rip = t;
                    }
                    4 => self.rip = self.load_rm(rm, 8),
                    6 => {
                        let v = self.load_rm(rm, 8);
                        self.push(v);
                    }
                    _ => return Err(Halt::Undefined(start)),
                }
            }
            0x0f => {
                let op2 = self.fetch_u8();
                #[cfg(feature = "std")]
                if OPHIST_ON.load(core::sync::atomic::Ordering::Relaxed) {
                    OPHIST_0F.with(|h| h.borrow_mut()[op2 as usize] += 1);
                }
                match op2 {
                    0x80..=0x8f => {
                        let rel = self.fetch(4) as i32 as i64;
                        if self.cond(op2 - 0x80) {
                            self.rip = self.rip.wrapping_add(rel as u64);
                        }
                    }
                    0x90..=0x9f => {
                        let cc = op2 - 0x90;
                        let (_e, rm) = self.modrm(rex);
                        let v = u64::from(self.cond(cc));
                        self.store_rm(rm, 1, v);
                    }
                    0xb6 => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1);
                        self.store_rm(Rm::Reg(reg), size, v);
                    }
                    0xb7 => {
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 2);
                        self.store_rm(Rm::Reg(reg), size, v);
                    }
                    0xbe => {
                        // MOVSX r, r/m8.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 1) as i8 as i64 as u64;
                        self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
                    }
                    0xbf => {
                        // MOVSX r, r/m16.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 2) as i16 as i64 as u64;
                        self.store_rm(Rm::Reg(reg), size, v & Self::mask(size));
                    }
                    0xaf => {
                        // IMUL r, r/m (signed) — set CF/OF on overflow of the size-byte result.
                        let (reg, rm) = self.modrm(rex);
                        let a = i128::from(sign_extend(self.r[reg] & Self::mask(size), size));
                        let b = i128::from(sign_extend(self.load_rm(rm, size), size));
                        let full = a * b;
                        let res = full as u64;
                        self.store_rm(Rm::Reg(reg), size, res & Self::mask(size));
                        self.set_imul_flags(full, res, size);
                    }
                    0x20 => {
                        // MOV r64, CRn (mod is ignored — rm is a register).
                        let (cr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.r[g] = self.cr(cr_idx);
                        }
                    }
                    0x22 => {
                        // MOV CRn, r64 — install paging (CR3), enable PG/PAE, etc.
                        let (cr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            let v = self.r[g];
                            self.set_cr(cr_idx, v);
                        }
                    }
                    0x21 => {
                        // MOV r64, DRn — read a debug register (cpu_init reads DR6/DR7).
                        let (dr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.r[g] = self.dr[dr_idx & 7];
                        }
                    }
                    0x23 => {
                        // MOV DRn, r64 — write a debug register (cpu_init clears them).
                        let (dr_idx, rm) = self.modrm(rex);
                        if let Rm::Reg(g) = rm {
                            self.dr[dr_idx & 7] = self.r[g];
                        }
                    }
                    0x30 => {
                        // WRMSR: MSR[ecx] = edx:eax.
                        let ecx = (self.r[RCX] & 0xffff_ffff) as u32;
                        let val = ((self.r[RDX] & 0xffff_ffff) << 32) | (self.r[RAX] & 0xffff_ffff);
                        self.wrmsr(ecx, val);
                    }
                    0x32 => {
                        // RDMSR: edx:eax = MSR[ecx].
                        let ecx = (self.r[RCX] & 0xffff_ffff) as u32;
                        let val = self.rdmsr(ecx);
                        self.r[RAX] = val & 0xffff_ffff;
                        self.r[RDX] = val >> 32;
                    }
                    0x05 => self.syscall_enter(),
                    0x07 => self.sysret(),
                    0x01 => self.group7(rex, start)?,
                    0x00 => {
                        // Group 6: LLDT(/2), LTR(/3), VERR/VERW — load the task
                        // register from the GDT descriptor `LTR` selects.
                        let (ext, rm) = self.modrm(rex);
                        let sel = self.load_rm(rm, 2);
                        if ext & 7 == 3 {
                            self.load_tr(sel as u16);
                        }
                    }
                    0xa2 => self.cpuid(),
                    0x31 => {
                        let tsc = self.read_tsc();
                        self.r[RAX] = tsc & 0xffff_ffff;
                        self.r[RDX] = tsc >> 32;
                    }
                    0x09 | 0x0d | 0x0e | 0x18..=0x1f | 0x77 => {
                        // WBINVD/PREFETCHW(3DNow)/FEMMS/NOP(prefetch/hint)/EMMS — no architectural
                        // effect the integer boot path observes. `0F 0D` (PREFETCHW) matters: on AMD
                        // the kernel's alternatives patch the SSE `prefetcht0` (`0F 18`) in SLUB's
                        // fast path into 3DNow `prefetchw` (`0F 0D`) once it detects the vendor.
                        if matches!(op2, 0x0d | 0x18..=0x1f) {
                            let _ = self.modrm(rex);
                        }
                    }
                    0xae => {
                        // 0F AE group — FXSAVE/FXRSTOR/XSAVE/XRSTOR/LDMXCSR/STMXCSR (memory forms) and
                        // LFENCE/MFENCE/SFENCE (register forms). CRITICAL: FXSAVE/FXRSTOR/XSAVE/XRSTOR
                        // are how the kernel saves+restores x87+XMM on a CONTEXT SWITCH. As NOPs, two
                        // threads clobber each other's FPU/XMM and float math corrupts (a real,
                        // timing-dependent multithreaded bug — the desktop hash-resize spin). Implement
                        // the legacy 512-byte save area (x87 ST0-7 @ +32, XMM0-15 @ +160); XSAVE adds a header.
                        let (reg, rm) = self.modrm(rex);
                        if let Rm::Reg(_) = rm {
                            // LFENCE/MFENCE/SFENCE — ordering on a single core is a no-op.
                        } else {
                            let addr = self.xmm_mem_addr(rm);
                            match reg & 7 {
                                ext @ (0 | 4) => {
                                    // FXSAVE (0) / XSAVE (4): write the legacy save area.
                                    self.wr(addr, 2, u64::from(self.fcw));
                                    let sw = (self.fsw & !0x3800) | (u16::from(self.ftop & 7) << 11);
                                    self.wr(addr.wrapping_add(2), 2, u64::from(sw));
                                    self.wr(addr.wrapping_add(4), 1, 0xff); // abridged FTW: all valid
                                    self.wr(addr.wrapping_add(24), 4, 0x1f80); // MXCSR default
                                    self.wr(addr.wrapping_add(28), 4, 0xffff); // MXCSR_MASK
                                    for i in 0..8 {
                                        let v = self.fpr[i];
                                        self.write_f80(addr.wrapping_add(32 + (i as u64) * 16), v);
                                    }
                                    for i in 0..16 {
                                        let o = addr.wrapping_add(160 + (i as u64) * 16);
                                        self.wr(o, 8, self.xmm[i] as u64);
                                        self.wr(o.wrapping_add(8), 8, (self.xmm[i] >> 64) as u64);
                                    }
                                    if ext == 4 {
                                        self.wr(addr.wrapping_add(512), 8, 0x3); // XSTATE_BV: x87|SSE
                                        self.wr(addr.wrapping_add(520), 8, 0);
                                    }
                                }
                                1 | 5 => {
                                    // FXRSTOR (1) / XRSTOR (5): reload x87 + XMM from the save area.
                                    self.fcw = self.rd(addr, 2) as u16;
                                    let sw = self.rd(addr.wrapping_add(2), 2) as u16;
                                    self.fsw = sw;
                                    self.ftop = ((sw >> 11) & 7) as u8;
                                    for i in 0..8 {
                                        self.fpr[i] = self.read_f80(addr.wrapping_add(32 + (i as u64) * 16));
                                    }
                                    for i in 0..16 {
                                        let o = addr.wrapping_add(160 + (i as u64) * 16);
                                        let lo = self.rd(o, 8);
                                        let hi = self.rd(o.wrapping_add(8), 8);
                                        self.xmm[i] = u128::from(lo) | (u128::from(hi) << 64);
                                    }
                                }
                                3 => { self.wr(addr, 4, 0x1f80); }  // STMXCSR
                                2 => { let _ = self.rd(addr, 4); }  // LDMXCSR (default rounding)
                                _ => {}                              // CLFLUSH etc.
                            }
                        }
                    }
                    0x0b => {
                        #[cfg(feature = "cc44-trace")]
                        {
                            use std::io::Write as _;
                            // Dump the bytes around the operands at a UD2 site
                            // (e.g. __text_poke's BUG() after a failed verify):
                            // rdi=dest, rsi=src, rdx=len.
                            let (dst, src, len) = (self.r[7], self.r[6], self.r[2]);
                            let mut dh = [0u8; 32];
                            let mut sh = [0u8; 32];
                            for i in 0..32u64 {
                                dh[i as usize] = self.rd_virt(dst.wrapping_add(i), 1) as u8;
                                sh[i as usize] = self.rd_virt(src.wrapping_add(i), 1) as u8;
                            }
                            let dpa = self.translate(dst);
                            let _ = writeln!(
                                std::io::stderr(),
                                "\n[cc44-trace] UD2 at {start:#x} rdi(dst)={dst:#x} -> pa={dpa:#x} \
                                 rsi(src)={src:#x} rdx(len)={len} cr3={:#x}\n  dst={dh:02x?}\n  src={sh:02x?}",
                                self.cr3,
                            );
                        }
                        // UD2 (`0F 0B`). On real x86-64 this raises `#UD`, which
                        // the kernel's `exc_invalid_op` → `handle_bug` decodes
                        // against `__bug_table`: a `WARN` prints and *resumes*
                        // (the handler advances past the `ud2`), a `BUG` panics.
                        // So vector `#UD` through the IDT (rip at the faulting
                        // `ud2`, a fault) rather than halting — the boot path hits
                        // `WARN`s (e.g. an initcall returning with IRQs off) that
                        // must be survivable. Only halt if no IDT is installed yet
                        // (the earliest boot, before the kernel's traps exist).
                        if self.sys.as_ref().is_some_and(|s| s.idtr.1 != 0) {
                            self.rip = start;
                            self.raise_exception(6, 0, false);
                            return Ok(());
                        }
                        return Err(Halt::Undefined(start)); // UD2, no handler yet
                    }
                    0x40..=0x4f => {
                        // CMOVcc r, r/m.
                        let cc = op2 - 0x40;
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size);
                        if self.cond(cc) {
                            self.store_rm(Rm::Reg(reg), size, v);
                        }
                    }
                    // ── SSE/SSE2 (XMM) data movement + bitwise logic ────────────────
                    // SSE2 is baseline on x86-64; `ld-musl` and every real userland use
                    // XMM from their first instructions. Prefix selects the variant:
                    // 0x66 = packed-double/integer, 0xF3 = scalar-single, 0xF2 =
                    // scalar-double, none = packed-single. Decoded into `opsz`/`rep`.
                    0x6e => {
                        // MOVD/MOVQ xmm, r/m (REX.W ⇒ 64-bit). Source is a GPR or memory.
                        let (reg, rm) = self.modrm(rex);
                        let sz = if rex & 8 != 0 { 8 } else { 4 };
                        let v = self.load_rm(rm, sz);
                        self.xmm[reg] = u128::from(v);
                    }
                    0x7e => {
                        let (reg, rm) = self.modrm(rex);
                        if matches!(rep, RepKind::Rep) {
                            // F3 0F 7E: MOVQ xmm, xmm/m64 — load 64, zero-extend.
                            let v = self.xmm_load64(rm);
                            self.xmm[reg] = u128::from(v);
                        } else {
                            // 66 0F 7E: MOVD/MOVQ r/m, xmm — store low 32/64.
                            let sz = if rex & 8 != 0 { 8 } else { 4 };
                            let v = (self.xmm[reg] as u64) & Self::mask(sz);
                            self.store_rm(rm, sz, v);
                        }
                    }
                    0xd6 => {
                        // 66 0F D6: MOVQ xmm/m64, xmm — low 64 (reg dest zero-extends).
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm[reg] as u64;
                        match rm {
                            Rm::Reg(i) => self.xmm[i] = u128::from(v),
                            _ => {
                                let a = self.xmm_mem_addr(rm);
                                self.wr(a, 8, v);
                            }
                        }
                    }
                    0x6f => {
                        // MOVDQA (66) / MOVDQU (F3) xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        self.xmm[reg] = self.xmm_load128(rm);
                    }
                    0x7f => {
                        // MOVDQA/MOVDQU xmm/m128, xmm.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm[reg];
                        self.xmm_store128(rm, v);
                    }
                    0x28 | 0x29 => {
                        // MOVAPS/MOVAPD: 0x28 load, 0x29 store (128-bit).
                        let (reg, rm) = self.modrm(rex);
                        if op2 == 0x28 {
                            self.xmm[reg] = self.xmm_load128(rm);
                        } else {
                            let v = self.xmm[reg];
                            self.xmm_store128(rm, v);
                        }
                    }
                    0x14 | 0x15 => {
                        // UNPCKLPS/UNPCKLPD (0x14) / UNPCKHPS/UNPCKHPD (0x15) — interleave float lanes.
                        let (reg, rm) = self.modrm(rex);
                        let (d, s) = (self.xmm[reg], self.xmm_load128(rm));
                        let high = op2 == 0x15;
                        self.xmm[reg] = if opsz {
                            if high { (d >> 64) | ((s >> 64) << 64) }
                            else { (d & u128::from(u64::MAX)) | ((s & u128::from(u64::MAX)) << 64) }
                        } else {
                            let base = if high { 2 } else { 0 };
                            let dl = |x: u128, i: u32| (x >> (32 * (base + i))) & u128::from(u32::MAX);
                            dl(d, 0) | (dl(s, 0) << 32) | (dl(d, 1) << 64) | (dl(s, 1) << 96)
                        };
                    }
                    0x2a => {
                        // CVTSI2SS (F3) / CVTSI2SD (F2) — signed integer r/m → scalar float,
                        // low lane; the rest of the dst xmm is preserved. REX.W picks i64 vs i32.
                        let (reg, rm) = self.modrm(rex);
                        let isz = if rex & 8 != 0 { 8 } else { 4 };
                        let raw = self.load_rm(rm, isz);
                        let ival: i64 = if isz == 8 { raw as i64 } else { raw as u32 as i32 as i64 };
                        if rep == RepKind::Rep {
                            let f = ival as f32;
                            self.xmm[reg] = (self.xmm[reg] & !u128::from(u32::MAX)) | u128::from(f.to_bits());
                        } else {
                            let f = ival as f64;
                            self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(f.to_bits());
                        }
                    }
                    0x2c | 0x2d => {
                        // CVT(T)SS2SI (F3) / CVT(T)SD2SI (F2) — scalar float → signed integer GPR.
                        // 0x2c truncates toward zero; 0x2d rounds to nearest-even (default MXCSR).
                        // Out-of-range / NaN → the architectural integer-indefinite (0x8000…).
                        let (reg, rm) = self.modrm(rex);
                        let osz = if rex & 8 != 0 { 8 } else { 4 };
                        let val: f64 = if rep == RepKind::Rep {
                            f64::from(f32::from_bits(self.xmm_load32(rm)))
                        } else {
                            f64::from_bits(self.xmm_load64(rm))
                        };
                        let r = if op2 == 0x2c { val.trunc() } else { val.round_ties_even() };
                        let int: i64 = if r.is_nan() {
                            i64::MIN
                        } else if osz == 8 {
                            if r >= 9.223_372_036_854_776e18 || r < -9.223_372_036_854_776e18 { i64::MIN } else { r as i64 }
                        } else if r >= 2_147_483_648.0 || r < -2_147_483_648.0 {
                            i64::from(i32::MIN)
                        } else {
                            i64::from(r as i32)
                        };
                        #[cfg(feature = "std")]
                        if self.cpl == 3 && int < 0 && std::env::var_os("HOLO_SYSTRACE").is_some() {
                            use core::sync::atomic::{AtomicU32, Ordering};
                            static N: AtomicU32 = AtomicU32::new(0);
                            if N.fetch_add(1, Ordering::Relaxed) < 8 {
                                std::eprintln!("[CVTT] op2={op2:#x} scalar_dbl={} in_bits={:#018x} val={val} -> {int}",
                                    rep == RepKind::Repne, val.to_bits());
                            }
                        }
                        self.store_rm(Rm::Reg(reg), osz, int as u64);
                    }
                    0x2e | 0x2f => {
                        // UCOMIS* (2e) / COMIS* (2f) — ordered scalar compare → ZF/PF/CF;
                        // OF/SF/AF cleared. opsz(66)=double, else single. NaN ⇒ unordered (all set).
                        let (reg, rm) = self.modrm(rex);
                        let (a, b): (f64, f64) = if opsz {
                            (f64::from_bits(self.xmm[reg] as u64), f64::from_bits(self.xmm_load64(rm)))
                        } else {
                            (f64::from(f32::from_bits(self.xmm[reg] as u32)),
                             f64::from(f32::from_bits(self.xmm_load32(rm))))
                        };
                        self.rflags &= !(flag::ZF | flag::PF | flag::CF | flag::OF | flag::SF | (1 << 4));
                        if a.is_nan() || b.is_nan() {
                            self.rflags |= flag::ZF | flag::PF | flag::CF;
                        } else if a < b {
                            self.rflags |= flag::CF;
                        } else if (a - b).abs() == 0.0 {
                            self.rflags |= flag::ZF;
                        }
                    }
                    0x51 => {
                        // SQRTPS/SQRTSS/SQRTPD/SQRTSD.
                        let (reg, rm) = self.modrm(rex);
                        match (rep, opsz) {
                            (RepKind::Rep, _) => {
                                let v = f32::from_bits(self.xmm_load32(rm)).sqrt();
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u32::MAX)) | u128::from(v.to_bits());
                            }
                            (RepKind::Repne, _) => {
                                let v = f64::from_bits(self.xmm_load64(rm)).sqrt();
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(v.to_bits());
                            }
                            (RepKind::None, true) => {
                                let s = self.xmm_load128(rm);
                                let lo = f64::from_bits(s as u64).sqrt();
                                let hi = f64::from_bits((s >> 64) as u64).sqrt();
                                self.xmm[reg] = u128::from(lo.to_bits()) | (u128::from(hi.to_bits()) << 64);
                            }
                            (RepKind::None, false) => {
                                let s = self.xmm_load128(rm);
                                let mut o = 0u128;
                                for i in 0..4 {
                                    let f = f32::from_bits((s >> (32 * i)) as u32).sqrt();
                                    o |= u128::from(f.to_bits()) << (32 * i);
                                }
                                self.xmm[reg] = o;
                            }
                        }
                    }
                    0x54 | 0x55 | 0x56 | 0x57 => {
                        // ANDPS(54)/ANDNPS(55)/ORPS(56)/XORPS(57) — 128-bit bitwise (PD form identical).
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] = match op2 {
                            0x54 => self.xmm[reg] & v,
                            0x55 => !self.xmm[reg] & v,
                            0x56 => self.xmm[reg] | v,
                            _ => self.xmm[reg] ^ v,
                        };
                    }
                    0x58 | 0x59 | 0x5c | 0x5d | 0x5e | 0x5f => {
                        // ADD(58) MUL(59) SUB(5c) MIN(5d) DIV(5e) MAX(5f) — PS/PD/SS/SD by prefix.
                        let (reg, rm) = self.modrm(rex);
                        let f64op = |a: f64, b: f64| -> f64 {
                            match op2 {
                                0x58 => a + b, 0x59 => a * b, 0x5c => a - b, 0x5e => a / b,
                                0x5d => if a < b { a } else { b }, _ => if a > b { a } else { b },
                            }
                        };
                        let f32op = |a: f32, b: f32| -> f32 {
                            match op2 {
                                0x58 => a + b, 0x59 => a * b, 0x5c => a - b, 0x5e => a / b,
                                0x5d => if a < b { a } else { b }, _ => if a > b { a } else { b },
                            }
                        };
                        match (rep, opsz) {
                            (RepKind::Repne, _) => {
                                let a = f64::from_bits(self.xmm[reg] as u64);
                                let b = f64::from_bits(self.xmm_load64(rm));
                                let r = f64op(a, b);
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(r.to_bits());
                            }
                            (RepKind::Rep, _) => {
                                let a = f32::from_bits(self.xmm[reg] as u32);
                                let b = f32::from_bits(self.xmm_load32(rm));
                                let r = f32op(a, b);
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u32::MAX)) | u128::from(r.to_bits());
                            }
                            (RepKind::None, true) => {
                                let (d, s) = (self.xmm[reg], self.xmm_load128(rm));
                                let lo = f64op(f64::from_bits(d as u64), f64::from_bits(s as u64));
                                let hi = f64op(f64::from_bits((d >> 64) as u64), f64::from_bits((s >> 64) as u64));
                                self.xmm[reg] = u128::from(lo.to_bits()) | (u128::from(hi.to_bits()) << 64);
                            }
                            (RepKind::None, false) => {
                                let (d, s) = (self.xmm[reg], self.xmm_load128(rm));
                                let mut o = 0u128;
                                for i in 0..4 {
                                    let r = f32op(f32::from_bits((d >> (32 * i)) as u32), f32::from_bits((s >> (32 * i)) as u32));
                                    o |= u128::from(r.to_bits()) << (32 * i);
                                }
                                self.xmm[reg] = o;
                            }
                        }
                    }
                    0x5a => {
                        // CVTSS2SD(F3) / CVTSD2SS(F2) / CVTPS2PD(none) / CVTPD2PS(66).
                        let (reg, rm) = self.modrm(rex);
                        match (rep, opsz) {
                            (RepKind::Rep, _) => {
                                let v = f64::from(f32::from_bits(self.xmm_load32(rm)));
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(v.to_bits());
                            }
                            (RepKind::Repne, _) => {
                                let v = f64::from_bits(self.xmm_load64(rm)) as f32;
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u32::MAX)) | u128::from(v.to_bits());
                            }
                            (RepKind::None, false) => {
                                let s = self.xmm_load128(rm);
                                let lo = f64::from(f32::from_bits(s as u32)).to_bits();
                                let hi = f64::from(f32::from_bits((s >> 32) as u32)).to_bits();
                                self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                            }
                            (RepKind::None, true) => {
                                let s = self.xmm_load128(rm);
                                let lo = (f64::from_bits(s as u64) as f32).to_bits();
                                let hi = (f64::from_bits((s >> 64) as u64) as f32).to_bits();
                                self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 32);
                            }
                        }
                    }
                    0x5b => {
                        // CVTDQ2PS(none) / CVTPS2DQ(66, round) / CVTTPS2DQ(F3, trunc).
                        let (reg, rm) = self.modrm(rex);
                        let s = self.xmm_load128(rm);
                        let mut o = 0u128;
                        match (rep, opsz) {
                            (RepKind::None, false) => {
                                for i in 0..4 {
                                    let f = ((s >> (32 * i)) as u32 as i32) as f32;
                                    o |= u128::from(f.to_bits()) << (32 * i);
                                }
                            }
                            (RepKind::None, true) | (RepKind::Rep, _) => {
                                let trunc = rep == RepKind::Rep;
                                for i in 0..4 {
                                    let f = f32::from_bits((s >> (32 * i)) as u32);
                                    let r = if trunc { f.trunc() } else { f.round_ties_even() };
                                    let v = if r.is_nan() || r >= 2_147_483_648.0 || r < -2_147_483_648.0 { i32::MIN } else { r as i32 };
                                    o |= u128::from(v as u32) << (32 * i);
                                }
                            }
                            _ => return Err(Halt::Undefined(start)),
                        }
                        self.xmm[reg] = o;
                    }
                    0xc2 => {
                        // CMPPS/CMPSS/CMPPD/CMPSD — compare with imm8 predicate → all-ones/zero.
                        let (reg, rm) = self.modrm(rex);
                        let imm = self.fetch_u8() & 7;
                        let c64 = |a: f64, b: f64| -> bool {
                            match imm { 0 => a == b, 1 => a < b, 2 => a <= b, 3 => a.is_nan() || b.is_nan(),
                                4 => a != b, 5 => !(a < b), 6 => !(a <= b), _ => !(a.is_nan() || b.is_nan()) }
                        };
                        let c32 = |a: f32, b: f32| -> bool {
                            match imm { 0 => a == b, 1 => a < b, 2 => a <= b, 3 => a.is_nan() || b.is_nan(),
                                4 => a != b, 5 => !(a < b), 6 => !(a <= b), _ => !(a.is_nan() || b.is_nan()) }
                        };
                        match (rep, opsz) {
                            (RepKind::Repne, _) => {
                                let r = c64(f64::from_bits(self.xmm[reg] as u64), f64::from_bits(self.xmm_load64(rm)));
                                let m = if r { u64::MAX } else { 0 };
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(m);
                            }
                            (RepKind::Rep, _) => {
                                let r = c32(f32::from_bits(self.xmm[reg] as u32), f32::from_bits(self.xmm_load32(rm)));
                                let m = if r { u32::MAX } else { 0 };
                                self.xmm[reg] = (self.xmm[reg] & !u128::from(u32::MAX)) | u128::from(m);
                            }
                            (RepKind::None, true) => {
                                let (d, s) = (self.xmm[reg], self.xmm_load128(rm));
                                let lo = if c64(f64::from_bits(d as u64), f64::from_bits(s as u64)) { u64::MAX } else { 0 };
                                let hi = if c64(f64::from_bits((d >> 64) as u64), f64::from_bits((s >> 64) as u64)) { u64::MAX } else { 0 };
                                self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                            }
                            (RepKind::None, false) => {
                                let (d, s) = (self.xmm[reg], self.xmm_load128(rm));
                                let mut o = 0u128;
                                for i in 0..4 {
                                    if c32(f32::from_bits((d >> (32 * i)) as u32), f32::from_bits((s >> (32 * i)) as u32)) {
                                        o |= u128::from(u32::MAX) << (32 * i);
                                    }
                                }
                                self.xmm[reg] = o;
                            }
                        }
                    }
                    0xc6 => {
                        // SHUFPS (none) / SHUFPD (66) — shuffle packed floats by imm8.
                        let (reg, rm) = self.modrm(rex);
                        let imm = self.fetch_u8();
                        let (d, s) = (self.xmm[reg], self.xmm_load128(rm));
                        if opsz {
                            let lo = if imm & 1 == 0 { d as u64 } else { (d >> 64) as u64 };
                            let hi = if (imm >> 1) & 1 == 0 { s as u64 } else { (s >> 64) as u64 };
                            self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                        } else {
                            let lane = |x: u128, sel: u32| (x >> (32 * sel)) & u128::from(u32::MAX);
                            let o = lane(d, u32::from(imm & 3))
                                | (lane(d, u32::from((imm >> 2) & 3)) << 32)
                                | (lane(s, u32::from((imm >> 4) & 3)) << 64)
                                | (lane(s, u32::from((imm >> 6) & 3)) << 96);
                            self.xmm[reg] = o;
                        }
                    }
                    0xe6 => {
                        // CVTDQ2PD (F3): 2 int32 (low 64) → 2 doubles. CVTPD2DQ (F2, round) /
                        // CVTTPD2DQ (66, trunc): 2 doubles → 2 int32 (low 64), high 64 zeroed.
                        let (reg, rm) = self.modrm(rex);
                        let s = self.xmm_load128(rm);
                        if rep == RepKind::Rep {
                            let lo = f64::from(s as u32 as i32).to_bits();
                            let hi = f64::from((s >> 32) as u32 as i32).to_bits();
                            self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                        } else {
                            let trunc = opsz; // 66 = CVTTPD2DQ (truncate); F2 = CVTPD2DQ (round)
                            let cvt = |v: f64| -> u32 {
                                let r = if trunc { v.trunc() } else { v.round_ties_even() };
                                if r.is_nan() || r >= 2_147_483_648.0 || r < -2_147_483_648.0 { i32::MIN as u32 } else { r as i32 as u32 }
                            };
                            let lo = cvt(f64::from_bits(s as u64));
                            let hi = cvt(f64::from_bits((s >> 64) as u64));
                            self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 32);
                        }
                    }
                    0x10 => {
                        // MOVUPS/MOVUPD (none/66) ; MOVSS (F3) ; MOVSD (F2) — load.
                        let (reg, rm) = self.modrm(rex);
                        match rep {
                            RepKind::Rep => match rm {
                                Rm::Reg(_) => {
                                    let s = self.xmm_load32(rm);
                                    self.xmm[reg] = (self.xmm[reg] & !u128::from(u32::MAX)) | u128::from(s);
                                }
                                _ => {
                                    let s = self.xmm_load32(rm);
                                    self.xmm[reg] = u128::from(s);
                                }
                            },
                            RepKind::Repne => match rm {
                                Rm::Reg(_) => {
                                    let s = self.xmm_load64(rm);
                                    self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(s);
                                }
                                _ => {
                                    let s = self.xmm_load64(rm);
                                    self.xmm[reg] = u128::from(s);
                                }
                            },
                            RepKind::None => self.xmm[reg] = self.xmm_load128(rm),
                        }
                    }
                    0x11 => {
                        // store form of 0x10.
                        let (reg, rm) = self.modrm(rex);
                        match rep {
                            RepKind::Rep => {
                                let v = self.xmm[reg] as u32;
                                match rm {
                                    Rm::Reg(i) => {
                                        self.xmm[i] = (self.xmm[i] & !u128::from(u32::MAX)) | u128::from(v);
                                    }
                                    _ => {
                                        let a = self.xmm_mem_addr(rm);
                                        self.wr(a, 4, u64::from(v));
                                    }
                                }
                            }
                            RepKind::Repne => {
                                let v = self.xmm[reg] as u64;
                                match rm {
                                    Rm::Reg(i) => {
                                        self.xmm[i] = (self.xmm[i] & !u128::from(u64::MAX)) | u128::from(v);
                                    }
                                    _ => {
                                        let a = self.xmm_mem_addr(rm);
                                        self.wr(a, 8, v);
                                    }
                                }
                            }
                            RepKind::None => {
                                let v = self.xmm[reg];
                                self.xmm_store128(rm, v);
                            }
                        }
                    }
                    0x12 => {
                        // MOVLPS/MOVLPD xmm, m64 (low←mem); reg-reg = MOVHLPS (src hi→dst lo).
                        let (reg, rm) = self.modrm(rex);
                        let lo = match rm {
                            Rm::Reg(i) => (self.xmm[i] >> 64) as u64,
                            _ => self.xmm_load64(rm),
                        };
                        self.xmm[reg] = (self.xmm[reg] & !u128::from(u64::MAX)) | u128::from(lo);
                    }
                    0x13 => {
                        // MOVLPS/MOVLPD m64, xmm — store low 64.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm[reg] as u64;
                        let a = self.xmm_mem_addr(rm);
                        self.wr(a, 8, v);
                    }
                    0x16 => {
                        // MOVHPS/MOVHPD xmm, m64 (high←mem); reg-reg = MOVLHPS (src lo→dst hi).
                        let (reg, rm) = self.modrm(rex);
                        let hi = match rm {
                            Rm::Reg(i) => self.xmm[i] as u64,
                            _ => self.xmm_load64(rm),
                        };
                        self.xmm[reg] = (self.xmm[reg] & u128::from(u64::MAX)) | (u128::from(hi) << 64);
                    }
                    0x17 => {
                        // MOVHPS/MOVHPD m64, xmm — store high 64.
                        let (reg, rm) = self.modrm(rex);
                        let v = (self.xmm[reg] >> 64) as u64;
                        let a = self.xmm_mem_addr(rm);
                        self.wr(a, 8, v);
                    }
                    0x6c => {
                        // PUNPCKLQDQ xmm, xmm/m128 — {dst.lo, src.lo}.
                        let (reg, rm) = self.modrm(rex);
                        let src = self.xmm_load128(rm);
                        let dst = self.xmm[reg];
                        self.xmm[reg] = (dst & u128::from(u64::MAX)) | ((src & u128::from(u64::MAX)) << 64);
                    }
                    0x6d => {
                        // PUNPCKHQDQ xmm, xmm/m128 — {dst.hi, src.hi}.
                        let (reg, rm) = self.modrm(rex);
                        let src = self.xmm_load128(rm);
                        let dst = self.xmm[reg];
                        self.xmm[reg] = (dst >> 64) | ((src >> 64) << 64);
                    }
                    0xef => {
                        // PXOR xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] ^= v;
                    }
                    // ── SSE2 integer ops (the musl/busybox string+memory routines, then the GUI
                    //    stack, lean on these from their first userspace instructions) ───────────
                    0x70 => {
                        // PSHUFD (66) / PSHUFHW (F3) / PSHUFLW (F2) xmm, xmm/m128, imm8.
                        let (reg, rm) = self.modrm(rex);
                        let src = self.xmm_load128(rm);
                        let imm = self.fetch_u8();
                        self.xmm[reg] = match rep {
                            RepKind::Rep => {
                                // PSHUFHW: low 64 verbatim; the 4 high words shuffled by imm pairs.
                                let hw = (src >> 64) as u64;
                                let mut hi = 0u128;
                                for i in 0..4 {
                                    let sel = (imm >> (2 * i)) & 3;
                                    let w = u128::from((hw >> (16 * sel)) & 0xffff);
                                    hi |= w << (16 * i);
                                }
                                (src & u128::from(u64::MAX)) | (hi << 64)
                            }
                            RepKind::Repne => {
                                // PSHUFLW: high 64 verbatim; the 4 low words shuffled.
                                let lw = src as u64;
                                let mut lo = 0u128;
                                for i in 0..4 {
                                    let sel = (imm >> (2 * i)) & 3;
                                    let w = u128::from((lw >> (16 * sel)) & 0xffff);
                                    lo |= w << (16 * i);
                                }
                                (src & (u128::from(u64::MAX) << 64)) | lo
                            }
                            RepKind::None => {
                                // PSHUFD: the 4 dwords shuffled by imm pairs.
                                let mut out = 0u128;
                                for i in 0..4 {
                                    let sel = (imm >> (2 * i)) & 3;
                                    let d = (src >> (32 * sel)) & u128::from(u32::MAX);
                                    out |= d << (32 * i);
                                }
                                out
                            }
                        };
                    }
                    0xc4 => {
                        // PINSRW xmm, r/m16, imm8 (66 0F C4) — insert a 16-bit word into lane imm&7.
                        // (pixman / GdkPixbuf pixel packing.) Source is a GPR (low 16) or memory.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, 4) as u16;
                        let imm = self.fetch_u8();
                        let lane = u32::from(imm & 7) * 16;
                        self.xmm[reg] =
                            (self.xmm[reg] & !(0xffffu128 << lane)) | (u128::from(v) << lane);
                    }
                    0xc5 => {
                        // PEXTRW r32, xmm, imm8 (66 0F C5) — extract a 16-bit word, ZERO-extended into
                        // the destination GP register (the pixman/GdkPixbuf word-extract path).
                        let (reg, rm) = self.modrm(rex);
                        let src = self.xmm_load128(rm);
                        let imm = self.fetch_u8();
                        let w = ((src >> (16 * (u32::from(imm) & 7))) & 0xffff) as u64;
                        self.r[reg] = w;
                    }
                    0x64 | 0x65 | 0x66 => {
                        // PCMPGTB/PCMPGTW/PCMPGTD — per-lane SIGNED greater-than → all-ones / zero.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let ls = match op2 { 0x64 => 1, 0x65 => 2, _ => 4 };
                        let lane = |s: &[u8]| -> i64 {
                            let mut v: u64 = 0;
                            for (k, &byte) in s.iter().enumerate() {
                                v |= (byte as u64) << (8 * k);
                            }
                            let shift = 64 - s.len() * 8;
                            ((v << shift) as i64) >> shift
                        };
                        let mut o = [0u8; 16];
                        let mut i = 0;
                        while i < 16 {
                            let gt = lane(&a[i..i + ls]) > lane(&b[i..i + ls]);
                            for k in 0..ls { o[i + k] = if gt { 0xff } else { 0 }; }
                            i += ls;
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0x71 | 0x72 | 0x73 => {
                        // SSE2 packed shift by IMM8 — PSRLW/PSRAW/PSLLW (71), PSRLD/PSRAD/PSLLD
                        // (72), PSRLQ/PSRLDQ/PSLLQ/PSLLDQ (73). modrm.reg = the operation; rm = the
                        // xmm operand (register form). Used heavily by pixman/cairo (Xorg).
                        let (ext, rm) = self.modrm(rex);
                        let imm = u32::from(self.fetch_u8());
                        let Rm::Reg(i) = rm else { return Err(Halt::Undefined(start)); };
                        let x = self.xmm[i];
                        let op = ext & 7;
                        let ls: u32 = match op2 { 0x71 => 2, 0x72 => 4, _ => 8 }; // word/dword/qword
                        self.xmm[i] = if op2 == 0x73 && op == 3 {
                            // PSRLDQ — shift the WHOLE 128-bit register right by `imm` BYTES.
                            if imm >= 16 { 0 } else { x >> (imm * 8) }
                        } else if op2 == 0x73 && op == 7 {
                            // PSLLDQ — left by `imm` bytes.
                            if imm >= 16 { 0 } else { x << (imm * 8) }
                        } else {
                            // Per-lane bit shifts: PSRL (/2), PSRA (/4, arithmetic), PSLL (/6).
                            let bits = ls * 8;
                            let lane_mask: u64 = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
                            let bytes = x.to_le_bytes();
                            let mut o = [0u8; 16];
                            let n = 16 / ls as usize;
                            for l in 0..n {
                                let base = l * ls as usize;
                                let mut v: u64 = 0;
                                for k in 0..ls as usize { v |= u64::from(bytes[base + k]) << (8 * k); }
                                let res: u64 = match op {
                                    6 => if imm >= bits { 0 } else { (v << imm) & lane_mask }, // PSLL
                                    2 => if imm >= bits { 0 } else { v >> imm },               // PSRL
                                    4 => {                                                      // PSRA
                                        let sh = 64 - bits;
                                        let sv = ((v << sh) as i64) >> sh;
                                        ((sv >> imm.min(bits - 1)) as u64) & lane_mask
                                    }
                                    _ => return Err(Halt::Undefined(start)),
                                };
                                for k in 0..ls as usize { o[base + k] = (res >> (8 * k)) as u8; }
                            }
                            u128::from_le_bytes(o)
                        };
                    }
                    0xd1 | 0xd2 | 0xd3 | 0xe1 | 0xe2 | 0xf1 | 0xf2 | 0xf3 => {
                        // Variable-count packed shifts — count = low 64 bits of src. PSRLW/D/Q
                        // (d1/d2/d3), PSRAW/D (e1/e2), PSLLW/D/Q (f1/f2/f3).
                        let (reg, rm) = self.modrm(rex);
                        let cnt = self.xmm_load128(rm) as u64;
                        let x = self.xmm[reg];
                        let ls: u32 = match op2 { 0xd1 | 0xe1 | 0xf1 => 2, 0xd2 | 0xe2 | 0xf2 => 4, _ => 8 };
                        let left = matches!(op2, 0xf1 | 0xf2 | 0xf3);
                        let arith = matches!(op2, 0xe1 | 0xe2);
                        let bits = u64::from(ls * 8);
                        let lane_mask: u64 = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
                        let bytes = x.to_le_bytes();
                        let mut o = [0u8; 16];
                        for l in 0..(16 / ls as usize) {
                            let base = l * ls as usize;
                            let mut v: u64 = 0;
                            for k in 0..ls as usize { v |= u64::from(bytes[base + k]) << (8 * k); }
                            let res: u64 = if left {
                                if cnt >= bits { 0 } else { (v << cnt) & lane_mask }
                            } else if arith {
                                let sh = 64 - bits;
                                let sv = ((v << sh) as i64) >> sh;
                                ((sv >> cnt.min(bits - 1)) as u64) & lane_mask
                            } else if cnt >= bits { 0 } else { v >> cnt };
                            for k in 0..ls as usize { o[base + k] = (res >> (8 * k)) as u8; }
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xd5 | 0xe5 | 0xe4 => {
                        // PMULLW (low 16) / PMULHW (signed high 16) / PMULHUW (unsigned high 16).
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let mut o = [0u8; 16];
                        for l in 0..8 {
                            let i = l * 2;
                            let aw = u16::from_le_bytes([a[i], a[i + 1]]);
                            let bw = u16::from_le_bytes([b[i], b[i + 1]]);
                            let r: u16 = match op2 {
                                0xd5 => aw.wrapping_mul(bw),
                                0xe5 => ((i32::from(aw as i16) * i32::from(bw as i16)) >> 16) as u16,
                                _ => ((u32::from(aw) * u32::from(bw)) >> 16) as u16,
                            };
                            o[i..i + 2].copy_from_slice(&r.to_le_bytes());
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xdc | 0xdd | 0xec | 0xed | 0xd8 | 0xd9 | 0xe8 | 0xe9 => {
                        // Saturating packed add/sub. PADDUSB/W (dc/dd), PADDSB/W (ec/ed),
                        // PSUBUSB/W (d8/d9), PSUBSB/W (e8/e9).
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let word = matches!(op2, 0xdd | 0xed | 0xd9 | 0xe9);
                        let signed = matches!(op2, 0xec | 0xed | 0xe8 | 0xe9);
                        let sub = matches!(op2, 0xd8 | 0xd9 | 0xe8 | 0xe9);
                        let ls = if word { 2 } else { 1 };
                        let mut o = [0u8; 16];
                        let mut i = 0;
                        while i < 16 {
                            let r: i64 = if signed {
                                let (sa, sb, lo, hi) = if word {
                                    (i64::from(i16::from_le_bytes([a[i], a[i + 1]])),
                                     i64::from(i16::from_le_bytes([b[i], b[i + 1]])),
                                     i64::from(i16::MIN), i64::from(i16::MAX))
                                } else {
                                    (i64::from(a[i] as i8), i64::from(b[i] as i8),
                                     i64::from(i8::MIN), i64::from(i8::MAX))
                                };
                                (if sub { sa - sb } else { sa + sb }).clamp(lo, hi)
                            } else {
                                let (ua, ub, hi) = if word {
                                    (i64::from(u16::from_le_bytes([a[i], a[i + 1]])),
                                     i64::from(u16::from_le_bytes([b[i], b[i + 1]])), 0xffff)
                                } else {
                                    (i64::from(a[i]), i64::from(b[i]), 0xff)
                                };
                                (if sub { ua - ub } else { ua + ub }).clamp(0, hi)
                            };
                            let ru = r as u64;
                            for k in 0..ls { o[i + k] = (ru >> (8 * k)) as u8; }
                            i += ls;
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xea | 0xee => {
                        // PMINSW / PMAXSW — signed word min/max.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let mut o = [0u8; 16];
                        for l in 0..8 {
                            let i = l * 2;
                            let aw = u16::from_le_bytes([a[i], a[i + 1]]) as i16;
                            let bw = u16::from_le_bytes([b[i], b[i + 1]]) as i16;
                            let r = if op2 == 0xea { aw.min(bw) } else { aw.max(bw) };
                            o[i..i + 2].copy_from_slice(&(r as u16).to_le_bytes());
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xe0 | 0xe3 => {
                        // PAVGB / PAVGW — rounding unsigned average: (a + b + 1) >> 1.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let mut o = [0u8; 16];
                        if op2 == 0xe0 {
                            for i in 0..16 { o[i] = ((u16::from(a[i]) + u16::from(b[i]) + 1) >> 1) as u8; }
                        } else {
                            for l in 0..8 {
                                let i = l * 2;
                                let aw = u32::from(u16::from_le_bytes([a[i], a[i + 1]]));
                                let bw = u32::from(u16::from_le_bytes([b[i], b[i + 1]]));
                                o[i..i + 2].copy_from_slice(&(((aw + bw + 1) >> 1) as u16).to_le_bytes());
                            }
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0x63 | 0x67 | 0x6b => {
                        // PACKSSWB (63) / PACKUSWB (67) words→bytes; PACKSSDW (6b) dwords→words.
                        // Result = saturate(dst lanes) then saturate(src lanes).
                        let (reg, rm) = self.modrm(rex);
                        let d = self.xmm[reg].to_le_bytes();
                        let s = self.xmm_load128(rm).to_le_bytes();
                        let mut o = [0u8; 16];
                        if op2 == 0x6b {
                            let mut oi = 0;
                            for src in [&d, &s] {
                                for l in 0..4 {
                                    let i = l * 4;
                                    let v = i32::from_le_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]);
                                    let w = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                                    o[oi..oi + 2].copy_from_slice(&(w as u16).to_le_bytes());
                                    oi += 2;
                                }
                            }
                        } else {
                            let signed = op2 == 0x63;
                            let mut oi = 0;
                            for src in [&d, &s] {
                                for l in 0..8 {
                                    let i = l * 2;
                                    let v = i32::from(i16::from_le_bytes([src[i], src[i + 1]]));
                                    o[oi] = if signed {
                                        v.clamp(i32::from(i8::MIN), i32::from(i8::MAX)) as i8 as u8
                                    } else {
                                        v.clamp(0, 255) as u8
                                    };
                                    oi += 1;
                                }
                            }
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xf5 => {
                        // PMADDWD — 8 signed words → 4 dwords: d[j] = a[2j]*b[2j] + a[2j+1]*b[2j+1].
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let w = |s: &[u8; 16], i: usize| i32::from(i16::from_le_bytes([s[i], s[i + 1]]));
                        let mut o = [0u8; 16];
                        for j in 0..4 {
                            let i = j * 4;
                            let r = w(&a, i).wrapping_mul(w(&b, i))
                                .wrapping_add(w(&a, i + 2).wrapping_mul(w(&b, i + 2)));
                            o[i..i + 4].copy_from_slice(&r.to_le_bytes());
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xf4 => {
                        // PMULUDQ — unsigned 32×32→64 of the low dword of each 64-bit lane.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg];
                        let b = self.xmm_load128(rm);
                        let lo = u64::from(a as u32) * u64::from(b as u32);
                        let hi = u64::from((a >> 64) as u32) * u64::from((b >> 64) as u32);
                        self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                    }
                    0xf6 => {
                        // PSADBW — sum of absolute byte diffs per 64-bit half → low word of each half.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let mut o = [0u8; 16];
                        for half in 0..2 {
                            let mut sum: u16 = 0;
                            for k in 0..8 {
                                let i = half * 8 + k;
                                sum += u16::from((i16::from(a[i]) - i16::from(b[i])).unsigned_abs());
                            }
                            o[half * 8..half * 8 + 2].copy_from_slice(&sum.to_le_bytes());
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0x74 | 0x75 | 0x76 => {
                        // PCMPEQB/PCMPEQW/PCMPEQD — per-lane equality → all-ones / zero.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let ls = match op2 { 0x74 => 1, 0x75 => 2, _ => 4 };
                        let mut o = [0u8; 16];
                        let mut i = 0;
                        while i < 16 {
                            let eq = a[i..i + ls] == b[i..i + ls];
                            for k in 0..ls { o[i + k] = if eq { 0xff } else { 0 }; }
                            i += ls;
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xd7 => {
                        // PMOVMSKB reg, xmm — the high bit of each of the 16 bytes → low 16 bits of a GPR.
                        let (reg, rm) = self.modrm(rex);
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let mut mask = 0u64;
                        for i in 0..16 { if b[i] & 0x80 != 0 { mask |= 1 << i; } }
                        self.r[reg] = mask;
                    }
                    0xeb => {
                        // POR xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] |= v;
                    }
                    0xdb => {
                        // PAND xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] &= v;
                    }
                    0xdf => {
                        // PANDN xmm, xmm/m128 — (NOT dst) AND src.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] = !self.xmm[reg] & v;
                    }
                    0xda | 0xde => {
                        // PMINUB / PMAXUB — per-byte unsigned min/max.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let mut o = [0u8; 16];
                        for i in 0..16 {
                            o[i] = if op2 == 0xda { a[i].min(b[i]) } else { a[i].max(b[i]) };
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xfc | 0xfd | 0xfe | 0xf8 | 0xf9 | 0xfa => {
                        // PADDB/W/D and PSUBB/W/D — per-lane wrapping add/sub (byte/word/dword).
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg].to_le_bytes();
                        let b = self.xmm_load128(rm).to_le_bytes();
                        let ls = match op2 { 0xfc | 0xf8 => 1, 0xfd | 0xf9 => 2, _ => 4 };
                        let add = matches!(op2, 0xfc | 0xfd | 0xfe);
                        let mut o = [0u8; 16];
                        let mut i = 0;
                        while i < 16 {
                            let (mut av, mut bv) = (0u64, 0u64);
                            for k in 0..ls {
                                av |= (a[i + k] as u64) << (8 * k);
                                bv |= (b[i + k] as u64) << (8 * k);
                            }
                            let r = if add { av.wrapping_add(bv) } else { av.wrapping_sub(bv) };
                            for k in 0..ls { o[i + k] = (r >> (8 * k)) as u8; }
                            i += ls;
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0xd4 | 0xfb => {
                        // PADDQ / PSUBQ — two 64-bit lanes, wrapping.
                        let (reg, rm) = self.modrm(rex);
                        let a = self.xmm[reg];
                        let b = self.xmm_load128(rm);
                        let (alo, ahi, blo, bhi) = (a as u64, (a >> 64) as u64, b as u64, (b >> 64) as u64);
                        let (lo, hi) = if op2 == 0xd4 {
                            (alo.wrapping_add(blo), ahi.wrapping_add(bhi))
                        } else {
                            (alo.wrapping_sub(blo), ahi.wrapping_sub(bhi))
                        };
                        self.xmm[reg] = u128::from(lo) | (u128::from(hi) << 64);
                    }
                    0x60 | 0x61 | 0x62 | 0x68 | 0x69 | 0x6a => {
                        // PUNPCKL/H BW/WD/DQ — interleave lanes from the low (or high) halves.
                        let (reg, rm) = self.modrm(rex);
                        let d = self.xmm[reg].to_le_bytes();
                        let s = self.xmm_load128(rm).to_le_bytes();
                        let ls = match op2 { 0x60 | 0x68 => 1, 0x61 | 0x69 => 2, _ => 4 };
                        let base = if matches!(op2, 0x68 | 0x69 | 0x6a) { 8 } else { 0 };
                        let mut o = [0u8; 16];
                        let (mut oi, lanes) = (0usize, 8 / ls);
                        for l in 0..lanes {
                            for k in 0..ls { o[oi] = d[base + l * ls + k]; oi += 1; }
                            for k in 0..ls { o[oi] = s[base + l * ls + k]; oi += 1; }
                        }
                        self.xmm[reg] = u128::from_le_bytes(o);
                    }
                    0x57 => {
                        // XORPS/XORPD xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] ^= v;
                    }
                    0x54 => {
                        // ANDPS/ANDPD xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] &= v;
                    }
                    0x55 => {
                        // ANDNPS/ANDNPD xmm, xmm/m128 — (~dst) & src.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] = !self.xmm[reg] & v;
                    }
                    0x56 => {
                        // ORPS/ORPD xmm, xmm/m128.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.xmm_load128(rm);
                        self.xmm[reg] |= v;
                    }
                    0xa3 | 0xab | 0xb3 | 0xbb => {
                        // BT / BTS / BTR / BTC r/m, r.
                        let (reg, rm) = self.modrm(rex);
                        let bit = self.r[reg] & Self::mask(size);
                        self.bit_test(rm, size, bit, (op2 >> 3) & 3);
                    }
                    0xba => {
                        // Group 8: BT/BTS/BTR/BTC r/m, imm8.
                        let (ext, rm) = self.modrm(rex);
                        let bit = u64::from(self.fetch(1) as u8);
                        self.bit_test(rm, size, bit, ((ext & 7).wrapping_sub(4) & 3) as u8);
                    }
                    0xbc => {
                        // BSF r, r/m — index of the lowest set bit; ZF if zero.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        self.set(flag::ZF, v == 0);
                        if v != 0 {
                            self.store_rm(Rm::Reg(reg), size, u64::from(v.trailing_zeros()));
                        }
                    }
                    0xbd => {
                        // BSR r, r/m — index of the highest set bit.
                        let (reg, rm) = self.modrm(rex);
                        let v = self.load_rm(rm, size) & Self::mask(size);
                        self.set(flag::ZF, v == 0);
                        if v != 0 {
                            let idx = 63 - v.leading_zeros();
                            self.store_rm(Rm::Reg(reg), size, u64::from(idx));
                        }
                    }
                    0xb0 => self.cmpxchg(rex, 1),
                    0xb1 => self.cmpxchg(rex, size),
                    0xc0 => self.xadd(rex, 1),
                    0xc1 => self.xadd(rex, size),
                    0xa4 | 0xac => {
                        // SHLD/SHRD r/m, r, imm8.
                        let (reg, rm) = self.modrm(rex);
                        let cnt = self.fetch(1) as u8;
                        self.shld_shrd(rm, reg, size, cnt, op2 == 0xac);
                    }
                    0xa5 | 0xad => {
                        // SHLD/SHRD r/m, r, CL.
                        let (reg, rm) = self.modrm(rex);
                        let cnt = (self.r[RCX] & 0xff) as u8;
                        self.shld_shrd(rm, reg, size, cnt, op2 == 0xad);
                    }
                    0xc7 => {
                        // Group 9. Memory form (/1) = CMPXCHG8B/16B; the
                        // register forms /6, /7 (mod==3) are RDRAND/RDSEED —
                        // the core's hardware RNG (advertised via CPUID).
                        let (ext, rm) = self.modrm(rex);
                        match (ext & 7, rm) {
                            (6, Rm::Reg(g)) | (7, Rm::Reg(g)) => self.rdrand(g, size),
                            (1, _) => self.cmpxchg16b(rm, rex & 8 != 0),
                            _ => {}
                        }
                    }
                    0xc8..=0xcf => {
                        // BSWAP r32/r64 — reverse the operand's byte order (the
                        // kernel's RNG/entropy + endian helpers use it). The
                        // register is the low 3 opcode bits, REX.B-extended.
                        let g = (op2 as usize - 0xc8) | (((rex & 1) as usize) << 3);
                        let v = self.r[g] & Self::mask(size);
                        let swapped = if size == 8 {
                            v.swap_bytes()
                        } else {
                            u64::from((v as u32).swap_bytes())
                        };
                        self.store_rm(Rm::Reg(g), size, swapped);
                    }
                    _ => return Err(Halt::Undefined(start)),
                }
            }
            0xcc => {
                // INT3 — the software breakpoint. The kernel's boot-time
                // self-test (`int3_selftest`) and the alternatives/kprobe machinery
                // execute a real `int3` and expect to vector through IDT[3]. `rip`
                // already points past the byte (the trap returns to the next insn).
                self.deliver_interrupt(3, 0, false);
            }
            0xcd => {
                // INT imm8 — software interrupt through IDT[imm8].
                let vec = self.fetch_u8();
                self.deliver_interrupt(vec, 0, false);
            }
            0x9b => {} // FWAIT/WAIT — no pending unmasked FP exception to service.
            0xd8..=0xdf => {
                // x87 FPU escape opcodes — a real stack machine (`fpr`/`ftop`). Required
                // by musl's `long double` number formatting on x86-64; see `Cpu::x87`.
                self.x87(op, rex);
            }
            _ => return Err(Halt::Undefined(start)),
        }
        // A page fault latched while executing this instruction: discard its
        // partial effects (restore the pre-instruction registers, `rip = start`),
        // set `CR2`, and vector `#PF` so the kernel's early page-fault handler maps
        // the page; the instruction re-runs on return (the real long-mode boot's
        // demand-paging of boot data through `early_top_pgt`).
        if let Some(pf) = self.fault.take() {
            self.restore_snapshot(snap);
            self.rip = start;
            self.cr2 = pf.addr;
            // SEGV PROBE (diagnostic): a userspace access faulting to the very top of the
            // stack (STACK_TOP = 0x7ffffffff000) is the Xorg crash we're chasing — record the
            // faulting instruction (its start rip + bytes) once, so the bad-pointer-producing
            // instruction can be found. Normal stack growth touches pages BELOW this one.
            #[cfg(feature = "std")]
            if self.cpl == 3 && (pf.addr & !0xfff) == 0x7fff_ffff_f000 {
                use std::sync::atomic::{AtomicBool, Ordering};
                static SEEN: AtomicBool = AtomicBool::new(false);
                if !SEEN.swap(true, Ordering::Relaxed) {
                    let code = self.peek_code(start, 16);
                    let hex: alloc::string::String = code.iter().map(|b| alloc::format!("{b:02x} ")).collect();
                    let lead = self.peek_code(start.wrapping_sub(24), 24);
                    let lhex: alloc::string::String = lead.iter().map(|b| alloc::format!("{b:02x} ")).collect();
                    std::eprintln!(
                        "\n[SEGV-PROBE] userspace fault addr={:#x} write={} rip={start:#x}\n  prev24: {}\n  here:   {}\n  rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsp={:#x} rbp={:#x} rsi={:#x} rdi={:#x}\n  r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}\n",
                        pf.addr, pf.error & PF_ERR_WRITE != 0,
                        lhex.trim_end(), hex.trim_end(),
                        self.r[0], self.r[1], self.r[2], self.r[3], self.r[4], self.r[5], self.r[6], self.r[7],
                        self.r[8], self.r[9], self.r[10], self.r[11], self.r[12], self.r[13], self.r[14], self.r[15],
                    );
                }
            }
            self.raise_exception(VEC_PAGE_FAULT, pf.error, true);
        }
        Ok(())
    }

    /// Capture the architectural register state restored on a mid-instruction
    /// `#PF` (general registers, `rip`, `rflags`, segments, `cpl`). RAM and the
    /// device/`sys` state are not snapshotted (see [`Cpu::step`]).
    fn reg_snapshot(&self) -> RegSnapshot {
        RegSnapshot {
            r: self.r,
            xmm: self.xmm,
            fpr: self.fpr,
            ftop: self.ftop,
            fcw: self.fcw,
            fsw: self.fsw,
            rip: self.rip,
            rflags: self.rflags,
            seg: self.seg,
            cpl: self.cpl,
        }
    }

    /// Restore a [`RegSnapshot`] taken at the start of an instruction. XMM + the x87
    /// stack are included because a read-modify SSE/x87 op (e.g. `MULSD xmm,[mem]`)
    /// whose memory operand `#PF`s would otherwise leave its destination corrupted by
    /// the faulting attempt's partial result — and the restart would consume that
    /// garbage (the destination is also a source). Restoring them makes the restart clean.
    fn restore_snapshot(&mut self, s: RegSnapshot) {
        self.r = s.r;
        self.xmm = s.xmm;
        self.fpr = s.fpr;
        self.ftop = s.ftop;
        self.fcw = s.fcw;
        self.fsw = s.fsw;
        self.rip = s.rip;
        self.rflags = s.rflags;
        self.seg = s.seg;
        self.cpl = s.cpl;
    }
}

/// The pre-instruction architectural state restored when a `#PF` is latched
/// mid-instruction, so the faulting instruction restarts cleanly after the
/// handler maps the page ([`Cpu::step`]).
#[derive(Clone, Copy)]
struct RegSnapshot {
    r: [u64; 16],
    xmm: [u128; 16],
    fpr: [f64; 8],
    ftop: u8,
    fcw: u16,
    fsw: u16,
    rip: u64,
    rflags: u64,
    seg: [Seg; 6],
    cpl: u8,
}

// The guest-physical layout the 64-bit boot protocol uses (a low region below
// the kernel's 16 MiB load address, mirroring the firecracker/kvmtool loaders).
const ZERO_PAGE: u64 = 0x7000; // struct boot_params (the "zero page")
const CMDLINE_ADDR: u64 = 0x20000; // the kernel command line
const BOOT_GDT: u64 = 0x500; // the boot GDT (3 flat descriptors)
const BOOT_PML4: u64 = 0x9000; // the loader's identity page tables
const BOOT_PDPT: u64 = 0xa000;

impl Cpu {
    /// Boot a real, unmodified x86-64 Linux `vmlinux` ELF to userspace via the
    /// 64-bit Linux boot protocol (`#12`, the x86-64 realization of `CC-36`).
    /// `kernel` is the *uncompressed* ELF; the loader lays out its `PT_LOAD`
    /// segments at their physical addresses, builds `boot_params` (the zero page,
    /// with an e820 map + the command line), a flat GDT, and identity page tables,
    /// enables long mode, and enters `startup_64` with `rsi` = the zero page —
    /// no real-mode setup, no in-guest decompressor. Drive it with [`Cpu::run`].
    #[must_use]
    pub fn boot_linux(ram_bytes: usize, kernel: &[u8], cmdline: &str) -> Self {
        Self::boot_linux_inner(ram_bytes, kernel, cmdline, None)
    }

    /// Boot like [`Cpu::boot_linux`], additionally attaching a **`virtio-blk`
    /// root filesystem** (`CC-45`): `rootfs` is the assembled image taken as
    /// κ-addressed content into the κ-disk (`CC-7`); the guest mounts it over
    /// `/dev/vda`. The shared `devbus` services the device — the same κ-disk the
    /// other cores boot. Use a `cmdline` with `root=/dev/vda`.
    #[must_use]
    pub fn boot_linux_disk(
        ram_bytes: usize,
        kernel: &[u8],
        rootfs: Vec<u8>,
        cmdline: &str,
    ) -> Self {
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::new(rootfs)),
        )
    }

    /// Boot like [`Cpu::boot_linux_disk`], but page the κ-disk from a supplied
    /// [`KappaStore`](hologram_substrate_core::KappaStore) by **streaming**
    /// `sector_count` sectors from `read` (no full image in RAM) — the x86-64
    /// analogue of [`aarch64::Cpu::boot_linux_disk_streamed`](super::aarch64::Cpu::boot_linux_disk_streamed):
    /// the browser peer reads each sector from the OPFS rootfs into the OPFS-backed
    /// store, so a real amd64 image boots without ever materializing the whole
    /// `Vec` (the paged κ-disk, the same `KappaBacking` every core uses, `CC-7`).
    #[must_use]
    pub fn boot_linux_disk_streamed<R: FnMut(u64, &mut [u8])>(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        store: alloc::boxed::Box<dyn hologram_substrate_core::KappaStore>,
        sector_count: u64,
        read: R,
    ) -> Self {
        let backing = super::KappaBacking::from_sectors(store, sector_count, read);
        Self::boot_linux_inner(
            ram_bytes,
            kernel,
            cmdline,
            Some(super::VirtioBlk::with_backing(backing)),
        )
    }

    fn boot_linux_inner(
        ram_bytes: usize,
        kernel: &[u8],
        cmdline: &str,
        disk: Option<super::VirtioBlk>,
    ) -> Self {
        // Validate the guest sizing up front so callers of the public
        // `boot_linux*` APIs get a clear message rather than a slice OOB / e820
        // underflow deep inside. The boot needs the legacy 1 MiB region plus room
        // for the NUL-terminated command line at CMDLINE_ADDR.
        let cl = cmdline.as_bytes();
        let cmdline_end = CMDLINE_ADDR as usize + cl.len() + 1;
        assert!(
            ram_bytes > 0x0010_0000 && ram_bytes >= cmdline_end,
            "boot_linux: ram_bytes={ram_bytes:#x} too small — need > 1 MiB and \
             ≥ {cmdline_end:#x} for the command line at {CMDLINE_ADDR:#x}",
        );

        let mut cpu = Cpu::new(ram_bytes);
        let entry = cpu.load_elf64(kernel);

        // The command line + the zero page (boot_params).
        cpu.ram[CMDLINE_ADDR as usize..CMDLINE_ADDR as usize + cl.len()].copy_from_slice(cl);
        cpu.ram[CMDLINE_ADDR as usize + cl.len()] = 0;
        cpu.build_boot_params(cmdline, ram_bytes as u64, disk.is_some());

        // The boot GDT: a null descriptor, a 64-bit code segment (__BOOT_CS,
        // selector 0x10), and a flat data segment (__BOOT_DS, selector 0x18) —
        // the segments the 64-bit boot protocol requires.
        cpu.build_boot_gdt();

        // Identity page tables: a single PML4 → PDPT mapping the low 512 GiB via
        // 1 GiB pages, so the entered kernel (and the zero page / command line /
        // its own code at 16 MiB) is reachable until startup_64 installs its own.
        cpu.build_boot_paging();

        // Enter long mode: PAE + PG + EFER.LME/LMA, CS = the long-mode code
        // segment, the data segments flat, rsi = the zero page, rip = startup_64.
        cpu.cr3 = BOOT_PML4;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = (1 << 8) | (1 << 10); // LME | LMA
        cpu.cr0 = (1 << 0) | (1 << 31); // PE | PG
        cpu.seg[SegId::Cs as usize] = Seg {
            selector: 0x10,
            base: 0,
            long: true,
        };
        for s in [SegId::Ds, SegId::Es, SegId::Ss, SegId::Fs, SegId::Gs] {
            cpu.seg[s as usize] = Seg {
                selector: 0x18,
                base: 0,
                long: false,
            };
        }
        cpu.sys_mut().gdtr = (BOOT_GDT, 0x17);
        cpu.r[RSI] = ZERO_PAGE;
        cpu.rip = entry;
        cpu.rflags = 0x2; // interrupts off, as on entry
        cpu.cpl = 0;
        cpu.sys_mut().virtio = disk;
        cpu
    }

    /// Load an ELF64 executable's `PT_LOAD` segments at their physical addresses
    /// (zeroing `bss` up to `p_memsz`) and return the entry point. The kernel's
    /// `vmlinux` is `ET_EXEC`; its segments load at the physical column of each
    /// program header.
    fn load_elf64(&mut self, elf: &[u8]) -> u64 {
        let rd16 = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]);
        let rd32 = |o: usize| u32::from_le_bytes([elf[o], elf[o + 1], elf[o + 2], elf[o + 3]]);
        let rd64 = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&elf[o..o + 8]);
            u64::from_le_bytes(b)
        };
        assert_eq!(&elf[0..4], b"\x7fELF", "vmlinux is an ELF");
        let entry = rd64(24);
        let phoff = rd64(32) as usize;
        let phentsize = rd16(54) as usize;
        let phnum = rd16(56) as usize;
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if rd32(ph) != 1 {
                continue; // PT_LOAD only
            }
            let offset = rd64(ph + 8) as usize;
            let paddr = rd64(ph + 24);
            let filesz = rd64(ph + 32) as usize;
            let memsz = rd64(ph + 40) as usize;
            let dst = paddr as usize;
            let n = filesz.min(elf.len().saturating_sub(offset));
            // Fail fast on a malformed/oversized image rather than silently
            // skipping the segment (which would leave a partially-loaded kernel
            // that executes into undefined behavior). This is a public boot API.
            assert!(
                dst.checked_add(memsz)
                    .is_some_and(|end| end <= self.ram.len()),
                "load_elf64: PT_LOAD segment [{dst:#x}, {:#x}) does not fit in {} bytes \
                 of guest RAM — increase ram_bytes or check the kernel image",
                dst.saturating_add(memsz),
                self.ram.len(),
            );
            assert!(
                offset + n <= elf.len(),
                "load_elf64: PT_LOAD file range exceeds the image"
            );
            self.ram[dst..dst + n].copy_from_slice(&elf[offset..offset + n]);
            for b in &mut self.ram[dst + n..dst + memsz] {
                *b = 0;
            }
        }
        entry
    }

    /// Build the boot GDT in low RAM: null, `__BOOT_CS` (0x10, 64-bit code), and
    /// `__BOOT_DS` (0x18, flat data) — the descriptors the entered kernel runs on
    /// until it reloads its own with `LGDT`.
    fn build_boot_gdt(&mut self) {
        let put = |ram: &mut [u8], sel: u64, desc: u64| {
            ram[BOOT_GDT as usize + sel as usize..BOOT_GDT as usize + sel as usize + 8]
                .copy_from_slice(&desc.to_le_bytes());
        };
        put(&mut self.ram, 0x00, 0);
        // 64-bit code: present, ring 0, code, executable/readable, L=1 (long).
        put(&mut self.ram, 0x10, 0x00af_9b00_0000_ffff);
        // Flat data: present, ring 0, data, read/write.
        put(&mut self.ram, 0x18, 0x00cf_9300_0000_ffff);
    }

    /// Build identity page tables: one PML4 entry → one PDPT whose first 512
    /// entries are 1 GiB pages mapping the low 512 GiB linearly. Enough that the
    /// entered kernel, the zero page, the command line, and the kernel's own load
    /// region are all reachable until `startup_64` installs its own tables.
    fn build_boot_paging(&mut self) {
        // PML4[0] → PDPT.
        self.ram[BOOT_PML4 as usize..BOOT_PML4 as usize + 8]
            .copy_from_slice(&(BOOT_PDPT | 0x3).to_le_bytes());
        for i in 0..512u64 {
            // 1 GiB pages: present | rw | page-size (bit 7).
            let e = (i << 30) | 0x83;
            let off = BOOT_PDPT as usize + i as usize * 8;
            self.ram[off..off + 8].copy_from_slice(&e.to_le_bytes());
        }
    }

    /// Build `struct boot_params` (the zero page): the setup-header fields the
    /// 64-bit protocol requires (`type_of_loader`, `loadflags`, `cmd_line_ptr`,
    /// the heap/ramdisk fields) and an **e820 memory map** so the kernel discovers
    /// RAM, the MMIO/APIC holes, and reserved low memory.
    fn build_boot_params(&mut self, _cmdline: &str, ram_bytes: u64, _has_disk: bool) {
        let zp = ZERO_PAGE as usize;
        let put32 = |ram: &mut [u8], off: usize, v: u32| {
            ram[zp + off..zp + off + 4].copy_from_slice(&v.to_le_bytes());
        };
        let put16 = |ram: &mut [u8], off: usize, v: u16| {
            ram[zp + off..zp + off + 2].copy_from_slice(&v.to_le_bytes());
        };
        let put8 = |ram: &mut [u8], off: usize, v: u8| ram[zp + off] = v;
        // struct setup_header (within boot_params at 0x1f1). The boot-protocol
        // signature + version the kernel checks before honouring cmd_line_ptr.
        put8(&mut self.ram, 0x1f1, 0); // setup_sects (unused on the 64-bit entry)
                                       // "HdrS" header magic at 0x202; boot-protocol version 2.15 at 0x206.
        self.ram[zp + 0x202..zp + 0x206].copy_from_slice(b"HdrS");
        put16(&mut self.ram, 0x206, 0x020f);
        // type_of_loader (0x210): 0xFF = undefined/unknown loader (accepted).
        put8(&mut self.ram, 0x210, 0xff);
        // loadflags (0x211): LOADED_HIGH (bit0) | CAN_USE_HEAP (bit7).
        put8(&mut self.ram, 0x211, 0x81);
        // cmd_line_ptr (0x228) — the 32-bit physical address of the command line.
        put32(&mut self.ram, 0x228, CMDLINE_ADDR as u32);
        // cmdline_size (0x238) — the maximum command-line length.
        put32(&mut self.ram, 0x238, 0x800);
        // ramdisk_image / ramdisk_size (0x218 / 0x21c): none (initramfs embedded).
        put32(&mut self.ram, 0x218, 0);
        put32(&mut self.ram, 0x21c, 0);

        // struct screen_info (boot_params offset 0) — advertise the passive linear framebuffer at the
        // top of RAM as an EFI framebuffer, so the kernel's `efifb`/`sysfb`→`simpledrm` + `fbcon` bind
        // to it (the x86 twin of the aarch64 `simple-framebuffer` DT node). x86 RAM is based at 0, so
        // the FB's guest-physical base is `ram_bytes - FB_SIZE`. Color layout = XRGB8888 (the standard
        // efifb format): blue@0 green@8 red@16 reserved@24, each 8 bits (the host may swap R/B when it
        // projects to the RGBA canvas — pin the order once pixels render).
        const VIDEO_TYPE_EFI: u8 = 0x70;
        let fb_base = (ram_bytes as usize).saturating_sub(Self::FB_SIZE) as u64;
        put8(&mut self.ram, 0x0f, VIDEO_TYPE_EFI); // orig_video_isVGA
        put16(&mut self.ram, 0x12, Self::FB_W as u16); // lfb_width
        put16(&mut self.ram, 0x14, Self::FB_H as u16); // lfb_height
        put16(&mut self.ram, 0x16, 32); // lfb_depth (bpp)
        put32(&mut self.ram, 0x18, fb_base as u32); // lfb_base (low 32)
        put32(&mut self.ram, 0x1c, Self::FB_SIZE as u32); // lfb_size
        put16(&mut self.ram, 0x24, (Self::FB_W * 4) as u16); // lfb_linelength (stride bytes)
        put8(&mut self.ram, 0x26, 8); // red_size
        put8(&mut self.ram, 0x27, 16); // red_pos
        put8(&mut self.ram, 0x28, 8); // green_size
        put8(&mut self.ram, 0x29, 8); // green_pos
        put8(&mut self.ram, 0x2a, 8); // blue_size
        put8(&mut self.ram, 0x2b, 0); // blue_pos
        put8(&mut self.ram, 0x2c, 8); // rsvd_size
        put8(&mut self.ram, 0x2d, 24); // rsvd_pos
        put32(&mut self.ram, 0x3a, (fb_base >> 32) as u32); // ext_lfb_base (high 32; 0 for <4 GiB)

        // The e820 map (boot_params.e820_entries at 0x1e8, e820_table at 0x2d0;
        // each entry is 20 bytes: u64 addr, u64 size, u32 type). type 1 = RAM,
        // type 2 = reserved.
        let mut entries: Vec<(u64, u64, u32)> = Vec::new();
        // Low RAM below the legacy 640 KiB / 1 MiB region: usable up to 0x9fc00,
        // reserved EBDA/BIOS to 1 MiB.
        entries.push((0x0000_0000, 0x0009_fc00, 1));
        entries.push((0x0009_fc00, 0x0000_0400, 2));
        entries.push((0x000f_0000, 0x0001_0000, 2));
        // Main RAM from 1 MiB up to the MMIO window (kept below 0xD000_0000), but RESERVE the
        // framebuffer at the top of RAM so the kernel/allocator never clobbers the scanout.
        let main_end = ram_bytes.min(VIRTIO_BLK_BASE);
        let ram_top = main_end.min(fb_base); // RAM ends where the FB begins
        // `saturating_sub`: `main_end ≥ 1 MiB` is guaranteed by the sizing assert
        // in `boot_linux_inner`, but stay underflow-safe regardless of caller.
        entries.push((0x0010_0000, ram_top.saturating_sub(0x0010_0000), 1));
        // The framebuffer region — reserved (owned RAM the guest scans out, not allocatable).
        if fb_base >= ram_top && fb_base + Self::FB_SIZE as u64 <= main_end {
            entries.push((fb_base, Self::FB_SIZE as u64, 2));
        }
        // The virtio-mmio + APIC windows are reserved (not RAM).
        entries.push((VIRTIO_BLK_BASE, LAPIC_END - VIRTIO_BLK_BASE, 2));
        // Any RAM above 4 GiB (if sized that large) is usable.
        if ram_bytes > 0x1_0000_0000 {
            entries.push((0x1_0000_0000, ram_bytes - 0x1_0000_0000, 1));
        }
        let n = entries.len().min(128) as u8;
        self.ram[zp + 0x1e8] = n;
        for (i, (addr, size, ty)) in entries.iter().take(128).enumerate() {
            let off = zp + 0x2d0 + i * 20;
            self.ram[off..off + 8].copy_from_slice(&addr.to_le_bytes());
            self.ram[off + 8..off + 16].copy_from_slice(&size.to_le_bytes());
            self.ram[off + 16..off + 20].copy_from_slice(&ty.to_le_bytes());
        }
    }

    // ── platform timers + interrupt delivery (the long-mode boot path; #12) ────

    /// Advance the architected timestamp counter and the platform timers one
    /// step; latch a timer interrupt (PIT IRQ 0 or the APIC-timer LVT vector)
    /// when one expires. The kernel calibrates against these and runs its
    /// periodic tick on them.
    ///
    /// The periodic IRQ sources (PIT channel-0, the APIC-timer LVT) count down
    /// every step *unconditionally* — the same model the riscv64 (`mtime`) and
    /// aarch64 (`counter`) cores use, where the timer advances regardless of the
    /// interrupt-acceptance state and only *delivery* of the latched interrupt is
    /// gated on `RFLAGS.IF`. The TSC and the channel-2 calibration one-shot
    /// advance at the same rate (one coherent time-base).
    fn sys_tick(&mut self) {
        let at_boundary = {
            let Some(sys) = self.sys.as_mut() else {
                return;
            };
            // The TSC advances every step (fine `sched_clock` resolution).
            sys.tsc = sys.tsc.wrapping_add(TSC_PER_STEP);
            // The PIT/APIC down-counters advance at ONE shared rate — once per
            // `TICK_DIV` steps. A single rate keeps the calibration reference (PIT
            // channel 2) and the tick sources (PIT channel 0, the APIC-timer) in
            // lockstep, so the LAPIC clockevent the kernel calibrates against the PIT
            // fires at exactly the rate the kernel expects — jiffies advance correctly
            // (no crawl). The period (`reload × TICK_DIV` steps) is far longer than the
            // handler (no storm) while channel 2 still expires inside
            // `quick_pit_calibrate`'s poll budget. One coherent time-base, like the
            // riscv64 (`mtime`) / aarch64 (`counter`) cores.
            sys.tdiv = sys.tdiv.wrapping_add(1);
            sys.tdiv % TICK_DIV == 0
        };
        if at_boundary {
            // The per-`TICK_DIV` PIT/APIC down-counter body.
            self.tick_down_counters();
        }
    }

    /// Advance the timers to the next armed deadline and latch its interrupt —
    /// called from `HLT` with interrupts enabled. The wake time is computed from
    /// the guest's *own* programmed timer counters (PIT channel-0, APIC-timer),
    /// so it scales to any guest/HZ with no fixed iteration cap — the same O(1)
    /// deadline-jump the riscv64 (`mtime = mtimecmp - 1`) and aarch64
    /// (`counter = deadline - 1`) WFI paths use. No loop, no arbitrary bound.
    fn idle_until_interrupt(&mut self) {
        // Debug gate (HOLO_NO_FASTFWD): skip the idle fast-forward so the run loop services a
        // pending wake (e.g. a serial RX IRQ that just made the shell runnable) and the kernel
        // reschedules it, instead of jumping past it to the next timer. Tests the resume-stability
        // hypothesis that fast-forward bypasses the reschedule of a woken userspace task.
        #[cfg(feature = "std")]
        if std::env::var_os("HOLO_NO_FASTFWD").is_some() {
            return;
        }
        let Some(sys) = self.sys.as_mut() else {
            return;
        };
        // Decrements remaining until each armed periodic source fires (each source
        // decrements once per `TICK_DIV` steps in `sys_tick`).
        let pit = (sys.pit.enabled && sys.pit.reload != 0).then(|| {
            u64::from(if sys.pit.counter == 0 {
                u32::from(sys.pit.reload)
            } else {
                sys.pit.counter
            })
        });
        let apic = (sys.lapic.enabled()
            && sys.lapic.initial_count != 0
            && sys.lapic.lvt_timer & (1 << 16) == 0)
            .then(|| {
                u64::from(if sys.lapic.current_count == 0 {
                    sys.lapic.initial_count
                } else {
                    sys.lapic.current_count
                })
            });
        // No armed timer → nothing to fast-forward to; leave the CPU halted (the
        // run loop keeps pumping devices/network and an external IRQ still wakes
        // it), exactly as hardware idles until an interrupt.
        let Some(ticks) = [pit, apic].into_iter().flatten().min() else {
            return;
        };
        // Jump to the nearest deadline: `ticks` down-counter decrements =
        // `ticks × TICK_DIV` steps. Advance the (every-step) TSC and the divisor
        // phase by the step count; the down-counters advance by `ticks`. Then fire
        // the source(s) that reached the deadline and advance the rest.
        let steps = ticks.saturating_mul(TICK_DIV);
        sys.tsc = sys.tsc.wrapping_add(steps.saturating_mul(TSC_PER_STEP));
        sys.tdiv = sys.tdiv.wrapping_add(steps);
        if sys.pit.ch2_gate && !sys.pit.ch2_out && sys.pit.ch2_counter > 0 {
            if u64::from(sys.pit.ch2_counter) <= ticks {
                sys.pit.ch2_counter = 0;
                sys.pit.ch2_out = true;
            } else {
                sys.pit.ch2_counter -= ticks as u32;
            }
        }
        if let Some(pt) = pit {
            if pt == ticks {
                sys.pic.raise(0);
                if sys.pit.ch0_periodic {
                    sys.pit.counter = u32::from(sys.pit.reload);
                } else {
                    sys.pit.counter = 0;
                    sys.pit.enabled = false;
                }
            } else {
                sys.pit.counter = (pt - ticks) as u32;
            }
        }
        if let Some(at) = apic {
            if at == ticks {
                sys.lapic.current_count = if sys.lapic.lvt_timer & (1 << 17) != 0 {
                    sys.lapic.initial_count
                } else {
                    0
                };
                sys.lapic.set_irr((sys.lapic.lvt_timer & 0xff) as u8);
            } else {
                sys.lapic.current_count = (at - ticks) as u32;
            }
        }
    }

    /// Deliver a pending external interrupt through the IDT if `RFLAGS.IF` is set
    /// and the kernel has installed an IDT. Returns `true` if an interrupt was
    /// taken (the caller then re-enters the run loop at the handler).
    fn take_pending_interrupt(&mut self) -> bool {
        if self.rflags & RFLAGS_IF == 0 {
            return false;
        }
        let (vector, irq) = {
            let Some(sys) = self.sys.as_ref() else {
                return false;
            };
            if sys.idtr.1 == 0 {
                return false;
            }
            if let Some(v) = sys.lapic.deliverable() {
                (v, None)
            } else if let Some((irq, vec)) = sys.pic.pending() {
                (vec, Some(irq))
            } else {
                return false;
            }
        };
        // Acknowledge the source.
        if let Some(sys) = self.sys.as_mut() {
            if let Some(irq) = irq {
                sys.pic.ack(irq);
            } else {
                sys.lapic.take(vector);
            }
        }
        self.deliver_interrupt(vector, 0, false);
        true
    }

    /// Vector through the 64-bit IDT: push the interrupt frame (`SS,RSP,RFLAGS,
    /// CS,RIP`, and the error code for the faults that carry one), switch to the
    /// gate's stack on a ring change, clear `IF`, and jump to the handler.
    fn deliver_interrupt(&mut self, vector: u8, error: u64, has_error: bool) {
        let (idt_base, _) = self.sys().idtr;
        let desc = idt_base + u64::from(vector) * 16;
        let lo = self.rd_virt(desc, 8);
        let hi = self.rd_virt(desc + 8, 8);
        let off = (lo & 0xffff) | ((lo >> 32) & 0xffff_0000) | ((hi & 0xffff_ffff) << 32);
        let ist = (lo >> 32) & 0x7;
        // Dev instrumentation: record CPU exceptions (< 32) into the bounded ring so a
        // wasm post-mortem can see the fault sequence — repeated identical `cr2` on
        // vector 14 = a non-progressing demand-paging loop (a stale translation across
        // the fault); a single `cr2` then a different fault = the handler took -EFAULT.
        #[cfg(feature = "std")]
        if vector < 32 {
            exc_trace_push(format!(
                "V={vector} err={error:#x} rip={:#x} cr2={:#x} handler={off:#x} cpl={} rsp={:#x}",
                self.rip,
                self.cr2,
                self.cpl,
                self.r[RSP],
            ));
        }
        // dev-only (`cc44-trace`): log CPU-exception vectors (< 32) as they vector
        // through the IDT — the fault type + faulting RIP + CR2 + target handler.
        // External IRQs (≥ 32) are omitted: they fire thousands of times per boot.
        #[cfg(feature = "cc44-trace")]
        if vector < 32 {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "\n[cc44-trace] VECTOR={vector} err={error:#x} from rip={:#x} cr2={:#x} -> handler={off:#x} ist={ist} rsp={:#x} if={}",
                self.rip,
                self.cr2,
                self.r[RSP],
                self.rflags & RFLAGS_IF != 0,
            );
        }
        let old_cs = self.seg[SegId::Cs as usize].selector;
        let old_ss = self.seg[SegId::Ss as usize].selector;
        let old_rsp = self.r[RSP];
        let old_cpl = self.cpl;

        // A ring transition (CPL 3 → 0) loads RSP0 from the TSS; an IST index
        // selects an IST stack. For the kernel's own interrupts (CPL 0) the stack
        // is unchanged unless an IST is set.
        if old_cpl != 0 || ist != 0 {
            let rsp0 = self.tss_stack(ist);
            if rsp0 != 0 {
                self.r[RSP] = rsp0;
            }
            self.cpl = 0;
            self.seg[SegId::Cs as usize].selector = 0x10;
            self.seg[SegId::Cs as usize].long = true;
            self.seg[SegId::Ss as usize].selector = 0x18;
        }
        let rflags = self.rflags;
        self.push(u64::from(old_ss));
        self.push(old_rsp);
        self.push(rflags);
        self.push(u64::from(old_cs));
        self.push(self.rip);
        if has_error {
            self.push(error);
        }
        self.rflags &= !RFLAGS_IF; // an interrupt gate clears IF
        self.rip = off;
    }

    /// The kernel stack for a ring transition: `RSP0` (IST 0) or the indexed IST
    /// entry from the TSS the kernel loaded with `LTR`.
    fn tss_stack(&self, ist: u64) -> u64 {
        let base = self.sys().tr_base;
        if base == 0 {
            return 0;
        }
        if ist == 0 {
            self.rd_virt(base + 4, 8) // RSP0 at TSS offset 4
        } else {
            self.rd_virt(base + 0x24 + (ist - 1) * 8, 8) // IST1.. at offset 0x24
        }
    }

    /// Deliver a CPU exception (a fault/abort) through the IDT — `#GP`, `#PF`
    /// (with `CR2` already set), `#UD`, etc. Mirrors the AArch64 core taking an
    /// EL1 exception instead of halting the flat core.
    fn raise_exception(&mut self, vector: u8, error: u64, has_error: bool) {
        if self.sys.as_ref().is_some_and(|s| s.idtr.1 != 0) {
            self.deliver_interrupt(vector, error, has_error);
        }
    }

    // ── the instruction tail the boot path hits (#12) ──────────────────────────

    /// Execute a string instruction (`MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS`) of
    /// element size `osz`, honouring a `REP`/`REPE`/`REPNE` prefix and the
    /// direction flag. `RSI`/`RDI` advance by ±`osz`; `RCX` counts the repeats.
    /// If `target` is an `__x86_indirect_thunk_rXX` retpoline (the Spectre‑v2 indirect‑
    /// call thunk), return the register `XX` it dispatches through. The body is fixed:
    /// `e8 01 00 00 00` (call +6) · `cc` (int3) · `48|4c 89 <modrm> 24`
    /// (`mov %rXX,(%rsp)`) · `e9` (jmp `__x86_return_thunk`). Matched by content (not a
    /// hardcoded address), so it is kernel‑version‑agnostic. Non‑faulting peek: an
    /// unmapped/garbage target simply doesn't match and the call proceeds normally.
    #[inline]
    fn retpoline_reg(&self, target: u64) -> Option<usize> {
        let b = |off: u64| *self.ram.get(self.translate(target.wrapping_add(off)) as usize).unwrap_or(&0);
        if b(0) == 0xe8
            && b(1) == 0x01
            && b(2) == 0
            && b(3) == 0
            && b(4) == 0
            && b(5) == 0xcc
            && (b(6) | 0x04) == 0x4c // REX.W (0x48) or REX.WR (0x4c)
            && b(7) == 0x89
            && (b(8) & 0xc7) == 0x04 // mov reg,(rsp): mod=00, rm=100 (SIB follows)
            && b(9) == 0x24 // SIB: base=rsp, index=none
            && b(10) == 0xe9
        {
            // base reg from ModRM.reg, extended by REX.R (bit 2 of the REX byte).
            Some(((b(8) >> 3) & 7) as usize | usize::from(b(6) & 0x04 != 0) << 3)
        } else {
            None
        }
    }

    // ── x87 FPU stack helpers ────────────────────────────────────────────────
    /// `st(i)` — the i-th register from the top of the x87 stack.
    #[inline]
    fn fst_pow2(k: i32) -> f64 {
        // Exact 2^k for k in the f64 normal-exponent range; clamps outside it. Used by
        // the 80-bit ↔ f64 conversions so no transcendental (`exp2`) is needed.
        if k >= 1024 {
            f64::INFINITY
        } else if k < -1022 {
            0.0
        } else {
            f64::from_bits(((k + 1023) as u64) << 52)
        }
    }
    #[inline]
    fn fst(&self, i: usize) -> f64 { self.fpr[((self.ftop as usize) + i) & 7] }
    /// Set `st(i)`.
    #[inline]
    fn fsetst(&mut self, i: usize, v: f64) { self.fpr[((self.ftop as usize) + i) & 7] = v; }
    /// Push: decrement TOP, store into the new `st(0)`.
    #[inline]
    fn fpush(&mut self, v: f64) { self.ftop = (self.ftop + 7) & 7; self.fpr[self.ftop as usize] = v; }
    /// Pop: discard `st(0)` by incrementing TOP.
    #[inline]
    fn fpop(&mut self) { self.ftop = (self.ftop + 1) & 7; }
    /// Round `st(0)` to an integer per the control-word rounding mode (RC, bits 10–11):
    /// 0=nearest-even, 1=down, 2=up, 3=truncate. Out-of-range/NaN → integer-indefinite.
    fn fist_round(&self, v: f64) -> i64 {
        let r = match (self.fcw >> 10) & 3 {
            0 => v.round_ties_even(),
            1 => v.floor(),
            2 => v.ceil(),
            _ => v.trunc(),
        };
        if r.is_nan() || r >= 9.223_372_036_854_776e18 || r < -9.223_372_036_854_776e18 {
            i64::MIN
        } else {
            r as i64
        }
    }
    /// `st(0) <op>= mem` for the D8/DC/DA/DE memory arithmetic group (`reg` selects the op;
    /// 2/3 are FCOM/FCOMP which compare instead of storing).
    fn farith_mem(&mut self, regf: u8, m: f64) {
        let a = self.fst(0);
        match regf {
            0 => self.fsetst(0, a + m),
            1 => self.fsetst(0, a * m),
            2 => self.fcom_sw(a, m),
            3 => { self.fcom_sw(a, m); self.fpop(); }
            4 => self.fsetst(0, a - m),
            5 => self.fsetst(0, m - a),
            6 => self.fsetst(0, a / m),
            _ => self.fsetst(0, m / a),
        }
    }
    /// FCOM-style compare → x87 status-word condition codes C3/C2/C0 (bits 14/10/8).
    fn fcom_sw(&mut self, a: f64, b: f64) {
        self.fsw &= !((1 << 14) | (1 << 10) | (1 << 8));
        if a.is_nan() || b.is_nan() {
            self.fsw |= (1 << 14) | (1 << 10) | (1 << 8); // unordered: C3=C2=C0=1
        } else if a < b {
            self.fsw |= 1 << 8; // C0
        } else if a == b {
            self.fsw |= 1 << 14; // C3
        }
    }
    /// FCOMI/FUCOMI-style compare → EFLAGS ZF/PF/CF (OF/SF/AF cleared), like UCOMISD.
    fn fcomi_flags(&mut self, a: f64, b: f64) {
        self.rflags &= !(flag::ZF | flag::PF | flag::CF | flag::OF | flag::SF | (1 << 4));
        if a.is_nan() || b.is_nan() {
            self.rflags |= flag::ZF | flag::PF | flag::CF;
        } else if a < b {
            self.rflags |= flag::CF;
        } else if a == b {
            self.rflags |= flag::ZF;
        }
    }
    /// Read an 80-bit extended-precision value from memory, as `f64`.
    fn read_f80(&mut self, addr: u64) -> f64 {
        let mant = self.rd(addr, 8);
        let se = self.rd(addr.wrapping_add(8), 2) as u16;
        let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
        let exp = (se & 0x7fff) as i32;
        if exp == 0 && mant == 0 {
            return sign * 0.0;
        }
        if exp == 0x7fff {
            return if mant << 1 == 0 { sign * f64::INFINITY } else { f64::NAN };
        }
        sign * (mant as f64) * Self::fst_pow2(exp - 16383 - 63)
    }
    /// Write an `f64` to memory as an 80-bit extended-precision value.
    fn write_f80(&mut self, addr: u64, v: f64) {
        let sign: u16 = if v.is_sign_negative() { 0x8000 } else { 0 };
        let a = v.abs();
        let (mant, biased): (u64, u16) = if a == 0.0 {
            (0, 0)
        } else if a.is_infinite() {
            (0x8000_0000_0000_0000, 0x7fff)
        } else if a.is_nan() {
            (0xC000_0000_0000_0000, 0x7fff)
        } else {
            let e = a.log2().floor() as i32;
            let m = (a * Self::fst_pow2(63 - e)).round() as u64;
            (m, (e + 16383) as u16)
        };
        self.wr(addr, 8, mant);
        self.wr(addr.wrapping_add(8), 2, u64::from(sign | (biased & 0x7fff)));
    }

    /// Execute one x87 escape (`op` ∈ `0xd8..=0xdf`). Decodes the ModRM byte itself
    /// (memory forms re-decode for SIB/disp; register forms encode the op in the
    /// modrm byte). A `#PF` on a memory access latches as usual and restarts the
    /// instruction. Unimplemented rare encodings are treated as no-ops (never a halt).
    fn x87(&mut self, op: u8, rex: u8) {
        let modrm = self.fetch_u8();
        let regf = (modrm >> 3) & 7;
        if modrm < 0xc0 {
            self.rip = self.rip.wrapping_sub(1);
            let (_r, rm) = self.modrm(rex);
            let addr = self.xmm_mem_addr(rm);
            match (op, regf) {
                // Loads (push onto the stack).
                (0xd9, 0) => { let v = f64::from(f32::from_bits(self.rd(addr, 4) as u32)); self.fpush(v); } // FLD m32
                (0xdd, 0) => { let v = f64::from_bits(self.rd(addr, 8)); self.fpush(v); }                    // FLD m64
                (0xdb, 5) => { let v = self.read_f80(addr); self.fpush(v); }                                 // FLD m80
                (0xdf, 0) => { let v = f64::from(self.rd(addr, 2) as u16 as i16); self.fpush(v); }           // FILD m16
                (0xdb, 0) => { let v = f64::from(self.rd(addr, 4) as u32 as i32); self.fpush(v); }           // FILD m32
                (0xdf, 5) => { let v = self.rd(addr, 8) as i64 as f64; self.fpush(v); }                      // FILD m64
                // Stores (optionally popping).
                (0xd9, 2) => { let v = self.fst(0); self.wr(addr, 4, u64::from((v as f32).to_bits())); }                       // FST m32
                (0xd9, 3) => { let v = self.fst(0); self.wr(addr, 4, u64::from((v as f32).to_bits())); self.fpop(); }          // FSTP m32
                (0xdd, 2) => { let v = self.fst(0); self.wr(addr, 8, v.to_bits()); }                                            // FST m64
                (0xdd, 3) => { let v = self.fst(0); self.wr(addr, 8, v.to_bits()); self.fpop(); }                               // FSTP m64
                (0xdb, 7) => { let v = self.fst(0); self.write_f80(addr, v); self.fpop(); }                                     // FSTP m80
                (0xdf, 2) => { let v = self.fist_round(self.fst(0)); self.wr(addr, 2, v as u16 as u64); }                       // FIST m16
                (0xdf, 3) => { let v = self.fist_round(self.fst(0)); self.wr(addr, 2, v as u16 as u64); self.fpop(); }          // FISTP m16
                (0xdb, 2) => { let v = self.fist_round(self.fst(0)); self.wr(addr, 4, v as u32 as u64); }                       // FIST m32
                (0xdb, 3) => { let v = self.fist_round(self.fst(0)); self.wr(addr, 4, v as u32 as u64); self.fpop(); }          // FISTP m32
                (0xdf, 7) => { let v = self.fist_round(self.fst(0)); self.wr(addr, 8, v as u64); self.fpop(); }                 // FISTP m64
                (0xdd, 1) | (0xdb, 1) | (0xdf, 1) | (0xd9, 1) => { let v = self.fist_round(self.fst(0)); let sz = if op == 0xdf {2} else {4}; self.wr(addr, sz, v as u64); self.fpop(); } // FISTTP (SSE3)
                // Arithmetic with a memory operand: D8=m32, DC=m64, DA=i32, DE=i16.
                (0xd8, _) => { let m = f64::from(f32::from_bits(self.rd(addr, 4) as u32)); self.farith_mem(regf, m); }
                (0xdc, _) => { let m = f64::from_bits(self.rd(addr, 8)); self.farith_mem(regf, m); }
                (0xda, _) => { let m = f64::from(self.rd(addr, 4) as u32 as i32); self.farith_mem(regf, m); }
                (0xde, _) => { let m = f64::from(self.rd(addr, 2) as u16 as i16); self.farith_mem(regf, m); }
                // Control / environment.
                (0xd9, 5) => { self.fcw = self.rd(addr, 2) as u16; }                 // FLDCW
                (0xd9, 7) => { self.wr(addr, 2, u64::from(self.fcw)); }              // FNSTCW
                (0xdd, 7) => { let sw = (self.fsw & !0x3800) | (u16::from(self.ftop & 7) << 11); self.wr(addr, 2, u64::from(sw)); } // FNSTSW m16
                _ => {} // FLDENV/FNSTENV/FRSTOR/FNSAVE etc. — ignore
            }
            return;
        }
        let i = (modrm & 7) as usize;
        match (op, modrm) {
            // ---- D9: load/const/unary ----
            (0xd9, 0xc0..=0xc7) => { let v = self.fst(i); self.fpush(v); }                         // FLD st(i)
            (0xd9, 0xc8..=0xcf) => { let (a, b) = (self.fst(0), self.fst(i)); self.fsetst(0, b); self.fsetst(i, a); } // FXCH
            (0xd9, 0xd0) => {}                                                                     // FNOP
            (0xd9, 0xe0) => { self.fsetst(0, -self.fst(0)); }                                      // FCHS
            (0xd9, 0xe1) => { self.fsetst(0, self.fst(0).abs()); }                                 // FABS
            (0xd9, 0xe4) => { let a = self.fst(0); self.fcom_sw(a, 0.0); }                         // FTST
            (0xd9, 0xe5) => {}                                                                     // FXAM (condition codes; rarely depended on)
            (0xd9, 0xe8) => self.fpush(1.0),                                                       // FLD1
            (0xd9, 0xe9) => self.fpush(3.321_928_094_887_362_3),                                   // FLDL2T
            (0xd9, 0xea) => self.fpush(1.442_695_040_888_963_4),                                   // FLDL2E
            (0xd9, 0xeb) => self.fpush(core::f64::consts::PI),                                     // FLDPI
            (0xd9, 0xec) => self.fpush(0.301_029_995_663_981_2),                                   // FLDLG2
            (0xd9, 0xed) => self.fpush(core::f64::consts::LN_2),                                   // FLDLN2
            (0xd9, 0xee) => self.fpush(0.0),                                                       // FLDZ
            (0xd9, 0xf0) => { self.fsetst(0, self.fst(0).exp2() - 1.0); }                          // F2XM1
            (0xd9, 0xf8) => { let (a, b) = (self.fst(0), self.fst(1)); self.fsetst(1, a % b); self.fpop(); } // FPREM (st1=rem; approx)
            (0xd9, 0xfa) => { self.fsetst(0, self.fst(0).sqrt()); }                                // FSQRT
            (0xd9, 0xfc) => { let v = self.fist_round(self.fst(0)) as f64; self.fsetst(0, v); }    // FRNDINT
            (0xd9, 0xfe) => { self.fsetst(0, self.fst(0).sin()); }                                 // FSIN
            (0xd9, 0xff) => { self.fsetst(0, self.fst(0).cos()); }                                 // FCOS
            // ---- D8: st0 op= st(i) ----
            (0xd8, 0xc0..=0xc7) => { let v = self.fst(0) + self.fst(i); self.fsetst(0, v); }
            (0xd8, 0xc8..=0xcf) => { let v = self.fst(0) * self.fst(i); self.fsetst(0, v); }
            (0xd8, 0xd0..=0xd7) => { let (a, b) = (self.fst(0), self.fst(i)); self.fcom_sw(a, b); }            // FCOM
            (0xd8, 0xd8..=0xdf) => { let (a, b) = (self.fst(0), self.fst(i)); self.fcom_sw(a, b); self.fpop(); } // FCOMP
            (0xd8, 0xe0..=0xe7) => { let v = self.fst(0) - self.fst(i); self.fsetst(0, v); }                  // FSUB
            (0xd8, 0xe8..=0xef) => { let v = self.fst(i) - self.fst(0); self.fsetst(0, v); }                  // FSUBR
            (0xd8, 0xf0..=0xf7) => { let v = self.fst(0) / self.fst(i); self.fsetst(0, v); }                  // FDIV
            (0xd8, 0xf8..=0xff) => { let v = self.fst(i) / self.fst(0); self.fsetst(0, v); }                  // FDIVR
            // ---- DC: st(i) op= st0 (reverse sense for SUB/DIV) ----
            (0xdc, 0xc0..=0xc7) => { let v = self.fst(i) + self.fst(0); self.fsetst(i, v); }
            (0xdc, 0xc8..=0xcf) => { let v = self.fst(i) * self.fst(0); self.fsetst(i, v); }
            (0xdc, 0xe0..=0xe7) => { let v = self.fst(0) - self.fst(i); self.fsetst(i, v); }                  // FSUBR st(i)
            (0xdc, 0xe8..=0xef) => { let v = self.fst(i) - self.fst(0); self.fsetst(i, v); }                  // FSUB st(i)
            (0xdc, 0xf0..=0xf7) => { let v = self.fst(0) / self.fst(i); self.fsetst(i, v); }                  // FDIVR st(i)
            (0xdc, 0xf8..=0xff) => { let v = self.fst(i) / self.fst(0); self.fsetst(i, v); }                  // FDIV st(i)
            // ---- DE: st(i) op= st0, then pop ----
            (0xde, 0xc0..=0xc7) => { let v = self.fst(i) + self.fst(0); self.fsetst(i, v); self.fpop(); }     // FADDP
            (0xde, 0xc8..=0xcf) => { let v = self.fst(i) * self.fst(0); self.fsetst(i, v); self.fpop(); }     // FMULP
            (0xde, 0xd9) => { let (a, b) = (self.fst(0), self.fst(1)); self.fcom_sw(a, b); self.fpop(); self.fpop(); } // FCOMPP
            (0xde, 0xe0..=0xe7) => { let v = self.fst(0) - self.fst(i); self.fsetst(i, v); self.fpop(); }     // FSUBRP
            (0xde, 0xe8..=0xef) => { let v = self.fst(i) - self.fst(0); self.fsetst(i, v); self.fpop(); }     // FSUBP
            (0xde, 0xf0..=0xf7) => { let v = self.fst(0) / self.fst(i); self.fsetst(i, v); self.fpop(); }     // FDIVRP
            (0xde, 0xf8..=0xff) => { let v = self.fst(i) / self.fst(0); self.fsetst(i, v); self.fpop(); }     // FDIVP
            // ---- DD: FFREE / FST(P) st(i) / FUCOM(P) ----
            (0xdd, 0xc0..=0xc7) => {}                                                                          // FFREE
            (0xdd, 0xd0..=0xd7) => { let v = self.fst(0); self.fsetst(i, v); }                                 // FST st(i)
            (0xdd, 0xd8..=0xdf) => { let v = self.fst(0); self.fsetst(i, v); self.fpop(); }                    // FSTP st(i)
            (0xdd, 0xe0..=0xe7) => { let (a, b) = (self.fst(0), self.fst(i)); self.fcom_sw(a, b); }            // FUCOM
            (0xdd, 0xe8..=0xef) => { let (a, b) = (self.fst(0), self.fst(i)); self.fcom_sw(a, b); self.fpop(); } // FUCOMP
            // ---- DB: FNINIT / FUCOMI / FCOMI ----
            (0xdb, 0xe3) => { self.ftop = 0; self.fcw = 0x037f; self.fsw = 0; }                                // FNINIT
            (0xdb, 0xe0..=0xe2) | (0xdb, 0xe4) => {}                                                           // FNENI/FNDISI/FNCLEX/FNSETPM
            (0xdb, 0xe8..=0xef) | (0xdb, 0xf0..=0xf7) => { let (a, b) = (self.fst(0), self.fst(i)); self.fcomi_flags(a, b); } // FUCOMI/FCOMI
            (0xda, 0xe9) => { let (a, b) = (self.fst(0), self.fst(1)); self.fcom_sw(a, b); self.fpop(); self.fpop(); } // FUCOMPP
            // ---- DF: FNSTSW AX / FUCOMIP / FCOMIP ----
            (0xdf, 0xe0) => { let sw = (self.fsw & !0x3800) | (u16::from(self.ftop & 7) << 11); self.r[RAX] = (self.r[RAX] & !0xffff) | u64::from(sw); } // FNSTSW AX
            (0xdf, 0xe8..=0xef) | (0xdf, 0xf0..=0xf7) => { let (a, b) = (self.fst(0), self.fst(i)); self.fcomi_flags(a, b); self.fpop(); } // FUCOMIP/FCOMIP
            // FCMOVcc (DA/DB C0..DF) and other rare encodings — approximate as no-ops.
            _ => {}
        }
    }

    fn string_op(&mut self, kind: StringOp, osz: u8, rep: RepKind, start: u64) {
        let step: i64 = if self.rflags & RFLAGS_DF != 0 {
            -i64::from(osz)
        } else {
            i64::from(osz)
        };
        // Bulk fast path — `clear_page` / small `__clear_user` (a top-2 hot operation on
        // a real boot: rep stos zeroing a page). A forward `REP STOS` whose whole
        // destination lies in one mapped RAM page is a single slice fill instead of N
        // per-element writes. Byte-identical to the loop below, and TSC-identical: we
        // replay the same per-cache-line dcache touches the per-element path would make
        // (the modelled L1 jitter feeds the TSC → RDRAND → ASLR, so it must not drift).
        // Falls through on anything subtle (DF=1, MMIO/device, cross-page, not-present).
        if fastpaths_on()
            && matches!(kind, StringOp::Stos)
            && rep != RepKind::None
            && self.rflags & RFLAGS_DF == 0
        {
            let count = self.r[RCX];
            let len = count.wrapping_mul(u64::from(osz));
            let dst = self.r[RDI];
            if count != 0 && (dst & 0xfff) + len <= 0x1000 {
                let user = self.cpl == 3;
                let pa = self.translate_acc(dst, true, user);
                let hi = pa.wrapping_add(len);
                if self.fault.is_none()
                    && !(VIRTIO_BLK_BASE..VIRTIO_NET_END).contains(&pa)
                    && !(LAPIC_BASE..LAPIC_END).contains(&pa)
                    && hi <= self.ram.len() as u64
                {
                    // Replay the per-element dcache touches (one miss per 64 B line) so the
                    // TSC trajectory is bit-identical to the per-element loop.
                    let mut line = pa & !0x3f;
                    while line < hi {
                        self.dcache_touch(line);
                        line += 64;
                    }
                    let val = self.r[RAX] & Self::mask(osz);
                    let o = (pa - RAM_BASE) as usize;
                    let n = len as usize;
                    if osz == 1 {
                        self.ram[o..o + n].fill(val as u8);
                    } else {
                        let bytes = val.to_le_bytes();
                        let mut p = o;
                        let end = o + n;
                        while p < end {
                            self.ram[p..p + osz as usize].copy_from_slice(&bytes[..osz as usize]);
                            p += osz as usize;
                        }
                    }
                    self.r[RDI] = dst.wrapping_add(len);
                    self.r[RCX] = 0;
                    return;
                }
                // A `#PF` latched on the probe (not-present page): the per-element loop
                // below sees `self.fault` on its first write and breaks, then `step`
                // restarts the whole REP after the handler maps the page — unchanged.
            }
        }
        // Bulk fast path — `copy_user` (the other top-hot op: rep movs). A forward
        // `REP MOVS` whose source and destination each lie within one mapped RAM page,
        // physically NON-OVERLAPPING, is a single slice copy instead of N read+write
        // pairs. Byte-identical (non-overlap → forward copy == `copy_within`) and
        // TSC-identical: we replay the per-element interleaved (src-read, dst-write)
        // dcache touches the loop would make. Falls through on overlap, page-cross,
        // MMIO, DF=1, or a `#PF`.
        if fastpaths_on()
            && matches!(kind, StringOp::Movs)
            && rep != RepKind::None
            && self.rflags & RFLAGS_DF == 0
        {
            let count = self.r[RCX];
            let len = count.wrapping_mul(u64::from(osz));
            let (src, dst) = (self.r[RSI], self.r[RDI]);
            if count != 0 && (src & 0xfff) + len <= 0x1000 && (dst & 0xfff) + len <= 0x1000 {
                let user = self.cpl == 3;
                let spa = self.translate_acc(src, false, user);
                let dpa = if self.fault.is_none() {
                    self.translate_acc(dst, true, user)
                } else {
                    0
                };
                let (shi, dhi) = (spa.wrapping_add(len), dpa.wrapping_add(len));
                let ram_ok = |lo: u64, hi: u64, ram: u64| {
                    !(VIRTIO_BLK_BASE..VIRTIO_NET_END).contains(&lo)
                        && !(LAPIC_BASE..LAPIC_END).contains(&lo)
                        && hi <= ram
                };
                let ramlen = self.ram.len() as u64;
                if self.fault.is_none()
                    && ram_ok(spa, shi, ramlen)
                    && ram_ok(dpa, dhi, ramlen)
                    && (dhi <= spa || shi <= dpa)
                // physically non-overlapping
                {
                    let osz64 = u64::from(osz);
                    for i in 0..count {
                        self.dcache_touch(spa + i * osz64);
                        self.dcache_touch(dpa + i * osz64);
                    }
                    let (soff, doff, n) =
                        ((spa - RAM_BASE) as usize, (dpa - RAM_BASE) as usize, len as usize);
                    self.ram.copy_within(soff..soff + n, doff);
                    self.r[RSI] = src.wrapping_add(len);
                    self.r[RDI] = dst.wrapping_add(len);
                    self.r[RCX] = 0;
                    return;
                }
                // A `#PF` latched on the probe: fall through; the per-element loop sees
                // it on the first access and breaks, then `step` restarts the REP.
            }
        }
        let mut count = if rep == RepKind::None { 1 } else { self.r[RCX] };
        while count != 0 {
            match kind {
                StringOp::Movs => {
                    let v = self.rd(self.r[RSI], osz);
                    self.wr(self.r[RDI], osz, v);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Stos => {
                    self.wr(self.r[RDI], osz, self.r[RAX] & Self::mask(osz));
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Lods => {
                    let v = self.rd(self.r[RSI], osz);
                    self.store_rm(Rm::Reg(RAX), osz, v);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                }
                StringOp::Scas => {
                    let a = self.r[RAX] & Self::mask(osz);
                    let b = self.rd(self.r[RDI], osz);
                    let r = a.wrapping_sub(b);
                    self.flags_arith(a, b, r, osz, true);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
                StringOp::Cmps => {
                    let a = self.rd(self.r[RSI], osz);
                    let b = self.rd(self.r[RDI], osz);
                    let r = a.wrapping_sub(b);
                    self.flags_arith(a, b, r, osz, true);
                    self.r[RSI] = self.r[RSI].wrapping_add(step as u64);
                    self.r[RDI] = self.r[RDI].wrapping_add(step as u64);
                }
            }
            // A `#PF` latched on this element's access (a demand-paged destination —
            // the kernel's `copy_to_user`/`__clear_user` on a not-present user page):
            // stop here so `step` discards the partial effects and restarts the whole
            // `REP` after the handler maps the page (x86 string ops are restartable).
            // Without this the loop spins the remaining `RCX` against the benign phys-0
            // scratch — wasted work, and a needless re-clobber of phys 0 every fault.
            if self.fault.is_some() {
                break;
            }
            if rep != RepKind::None {
                count -= 1;
                self.r[RCX] = count;
                // REPE/REPNE on SCAS/CMPS also terminate on the ZF condition.
                if matches!(kind, StringOp::Scas | StringOp::Cmps) {
                    let zf = self.rflags & flag::ZF != 0;
                    if (rep == RepKind::Rep && !zf) || (rep == RepKind::Repne && zf) {
                        break;
                    }
                }
                // INTERRUPTIBLE + step-BOUNDED: a `REP` with a huge/corrupted `RCX` must not spin a
                // single `ws.run` forever (a real hang we hit). `REP` is restartable — process this
                // element, then rewind `rip` to the prefix so the run loop re-enters it next step
                // (counting one step per element, servicing timers/interrupts between). The bulk
                // fast paths above still handle the common whole-page reps in one shot, so only the
                // cross-page/MMIO fallback pays the per-element re-decode.
                if count != 0 {
                    self.rip = start;
                    return;
                }
            } else {
                break;
            }
        }
    }

    /// Execute a shift/rotate (`ROL`/`ROR`/`RCL`/`RCR`/`SHL`/`SHR`/`SAR`) on a
    /// decoded operand, masking the count to 6 bits (5 for sub-64-bit) and setting
    /// the carry/zero/sign flags as the architecture defines.
    fn shift_rotate(&mut self, ext: u8, rm: Rm, size: u8, cnt: u8) {
        let bits = u32::from(size) * 8;
        let cnt = u32::from(cnt) & if size == 8 { 63 } else { 31 };
        if cnt == 0 {
            return;
        }
        let m = Self::mask(size);
        let a = self.load_rm(rm, size) & m;
        let (res, cf) = match ext {
            0 => {
                // ROL
                let r = ((a << cnt) | (a >> (bits - cnt))) & m;
                (r, r & 1)
            }
            1 => {
                // ROR
                let r = ((a >> cnt) | (a << (bits - cnt))) & m;
                (r, (r >> (bits - 1)) & 1)
            }
            4 | 6 => {
                // SHL / SAL
                let r = (a << cnt) & m;
                (r, (a >> (bits - cnt)) & 1)
            }
            5 => {
                // SHR
                let r = a >> cnt;
                (r, (a >> (cnt - 1)) & 1)
            }
            7 => {
                // SAR (arithmetic)
                let sa = (a as i64) << (64 - bits) >> (64 - bits); // sign-extend
                let r = ((sa >> cnt) as u64) & m;
                (r, (a >> (cnt - 1)) & 1)
            }
            2 | 3 => {
                // RCL / RCR through CF — uncommon on the boot path; approximate
                // with the corresponding rotate (the kernel does not depend on the
                // carry-through for correctness here).
                let r = if ext == 2 {
                    ((a << cnt) | (a >> (bits - cnt))) & m
                } else {
                    ((a >> cnt) | (a << (bits - cnt))) & m
                };
                (r, r & 1)
            }
            _ => (a, 0),
        };
        self.store_rm(rm, size, res);
        self.set(flag::CF, cf != 0);
        if ext >= 4 {
            // The logical/arithmetic shifts update SF/ZF/PF.
            self.set(flag::ZF, res == 0);
            self.set(flag::SF, Self::sign(res, size));
            self.set(flag::PF, (res as u8).count_ones().is_multiple_of(2));
        }
        // OF is defined only for a 1-bit shift/rotate (undefined for other counts → left as-is).
        if cnt == 1 {
            let bits = u32::from(size) * 8;
            let smsb = |v: u64| (v >> (bits - 1)) & 1;
            let of = match ext {
                0 | 4 | 6 => smsb(res) ^ cf,                // ROL / SHL / SAL: MSB(res) ^ CF
                1 => smsb(res) ^ ((res >> (bits - 2)) & 1), // ROR: MSB ^ (MSB-1)
                5 => smsb(a),                               // SHR: MSB of the original operand
                _ => 0,                                     // SAR: cleared
            };
            self.set(flag::OF, of != 0);
        }
    }

    /// `SHLD`/`SHRD` — a double-precision shift of `dst` with bits shifted in from
    /// `src`.
    fn shld_shrd(&mut self, rm: Rm, reg: usize, size: u8, cnt: u8, right: bool) {
        let bits = u32::from(size) * 8;
        let cnt = u32::from(cnt) & if size == 8 { 63 } else { 31 };
        if cnt == 0 {
            return;
        }
        let m = Self::mask(size);
        let dst = self.load_rm(rm, size) & m;
        let src = self.r[reg] & m;
        let res = if right {
            ((dst >> cnt) | (src << (bits - cnt))) & m
        } else {
            ((dst << cnt) | (src >> (bits - cnt))) & m
        };
        self.store_rm(rm, size, res);
        self.set(flag::ZF, res == 0);
        self.set(flag::SF, Self::sign(res, size));
        self.set(flag::PF, (res as u8).count_ones().is_multiple_of(2));
    }

    /// Group 3 (`0xf6`/`0xf7`): `TEST`/`NOT`/`NEG`/`MUL`/`IMUL`/`DIV`/`IDIV`.
    fn group3(&mut self, rex: u8, size: u8, start: u64) -> Result<(), Halt> {
        let (ext, rm) = self.modrm(rex);
        let m = Self::mask(size);
        match ext & 7 {
            0 | 1 => {
                // TEST r/m, imm. The immediate is fetched first so a RIP-relative
                // `rm` resolves against the instruction-end `rip`.
                let imm = if size == 1 {
                    self.fetch(1)
                } else {
                    self.fetch_imm_z(size)
                };
                let a = self.load_rm(rm, size);
                self.flags_logic(a & imm, size);
            }
            2 => {
                // NOT (no flags).
                let a = self.load_rm(rm, size);
                self.store_rm(rm, size, !a & m);
            }
            3 => {
                // NEG.
                let a = self.load_rm(rm, size) & m;
                let r = a.wrapping_neg() & m;
                self.flags_arith(0, a, r, size, true);
                self.set(flag::CF, a != 0);
                self.store_rm(rm, size, r);
            }
            4 => {
                // MUL (unsigned): rdx:rax = rax * r/m.
                let a = self.r[RAX] & m;
                let b = self.load_rm(rm, size) & m;
                let prod = u128::from(a) * u128::from(b);
                self.store_mul_result(prod, size);
            }
            5 => {
                // IMUL (signed): rdx:rax = rax * r/m. CF/OF set when the result doesn't fit in
                // `size` bytes signed (rdx is not the sign-extension of rax) — NOT `hi != 0`.
                let a = i128::from(sign_extend(self.r[RAX] & m, size));
                let b = i128::from(sign_extend(self.load_rm(rm, size) & m, size));
                let prod = a * b;
                let pu = prod as u128;
                if size == 1 {
                    self.r[RAX] = (self.r[RAX] & !0xffff) | (pu as u64 & 0xffff);
                } else {
                    self.store_rm(Rm::Reg(RAX), size, pu as u64 & m);
                    self.store_rm(Rm::Reg(RDX), size, (pu >> (u32::from(size) * 8)) as u64 & m);
                }
                let ov = prod != i128::from(sign_extend(pu as u64 & m, size));
                self.set(flag::CF, ov);
                self.set(flag::OF, ov);
            }
            6 => {
                // DIV (unsigned).
                let divisor = self.load_rm(rm, size) & m;
                if divisor == 0 {
                    self.raise_exception(0, 0, false); // #DE
                    return Ok(());
                }
                let dividend = self.dividend(size);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                self.store_div_result(q as u64, r as u64, size);
            }
            7 => {
                // IDIV (signed).
                let divisor = sign_extend(self.load_rm(rm, size) & m, size);
                if divisor == 0 {
                    self.raise_exception(0, 0, false);
                    return Ok(());
                }
                let dividend = self.dividend_signed(size);
                let q = dividend / i128::from(divisor);
                let r = dividend % i128::from(divisor);
                self.store_div_result(q as u64, r as u64, size);
            }
            _ => return Err(Halt::Undefined(start)),
        }
        Ok(())
    }

    fn store_mul_result(&mut self, prod: u128, size: u8) {
        let m = Self::mask(size);
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (prod as u64 & 0xffff);
        } else {
            self.store_rm(Rm::Reg(RAX), size, prod as u64 & m);
            self.store_rm(
                Rm::Reg(RDX),
                size,
                (prod >> (u32::from(size) * 8)) as u64 & m,
            );
        }
        let hi = prod >> (u32::from(size) * 8);
        let overflow = hi != 0;
        self.set(flag::CF, overflow);
        self.set(flag::OF, overflow);
    }

    /// Set `CF`/`OF` for an `IMUL` whose truncated `size`-byte result is `res`: both are set when
    /// the full signed product does not fit in `size` bytes (i.e. the discarded high bits are not a
    /// sign-extension of the kept result). This is what `__builtin_mul_overflow` / overflow-checked
    /// allocation sizing tests with `JO`/`JC`. `SF`/`ZF`/`PF`/`AF` are architecturally undefined.
    fn set_imul_flags(&mut self, full: i128, res: u64, size: u8) {
        let kept = i128::from(sign_extend(res & Self::mask(size), size));
        let ov = full != kept;
        self.set(flag::CF, ov);
        self.set(flag::OF, ov);
    }

    fn dividend(&self, size: u8) -> u128 {
        let m = Self::mask(size);
        if size == 1 {
            u128::from(self.r[RAX] & 0xffff)
        } else {
            (u128::from(self.r[RDX] & m) << (u32::from(size) * 8)) | u128::from(self.r[RAX] & m)
        }
    }

    fn dividend_signed(&self, size: u8) -> i128 {
        if size == 1 {
            i128::from(self.r[RAX] as i16)
        } else {
            let m = Self::mask(size);
            let hi = sign_extend(self.r[RDX] & m, size) as i128;
            let lo = (self.r[RAX] & m) as i128;
            (hi << (u32::from(size) * 8)) | lo
        }
    }

    fn store_div_result(&mut self, q: u64, r: u64, size: u8) {
        if size == 1 {
            self.r[RAX] = (self.r[RAX] & !0xffff) | (q & 0xff) | ((r & 0xff) << 8);
        } else {
            self.store_rm(Rm::Reg(RAX), size, q & Self::mask(size));
            self.store_rm(Rm::Reg(RDX), size, r & Self::mask(size));
        }
    }

    /// `BT`/`BTS`/`BTR`/`BTC` — test (and optionally set/reset/complement) bit
    /// `bit` of the operand; the prior bit value goes to `CF`. `op` selects the
    /// variant (0=BT, 1=BTS, 2=BTR, 3=BTC).
    fn bit_test(&mut self, rm: Rm, size: u8, bit: u64, op: u8) {
        let nbits = u64::from(size) * 8;
        // For a memory operand the bit index addresses beyond the operand; for a
        // register it wraps to the operand width.
        match rm {
            Rm::Reg(_) => {
                let b = bit % nbits;
                let v = self.load_rm(rm, size);
                self.set(flag::CF, (v >> b) & 1 != 0);
                let nv = match op {
                    1 => v | (1 << b),
                    2 => v & !(1 << b),
                    3 => v ^ (1 << b),
                    _ => v,
                };
                if op != 0 {
                    self.store_rm(rm, size, nv);
                }
            }
            _ => {
                let base = self.rm_addr(rm).unwrap();
                let byte = base.wrapping_add(bit / 8);
                let b = bit % 8;
                let v = self.rd(byte, 1);
                self.set(flag::CF, (v >> b) & 1 != 0);
                let nv = match op {
                    1 => v | (1 << b),
                    2 => v & !(1 << b),
                    3 => v ^ (1 << b),
                    _ => v,
                };
                if op != 0 {
                    self.wr(byte, 1, nv);
                }
            }
        }
    }

    /// `CMPXCHG`: compare `RAX` with the destination; if equal, store the source,
    /// else load the destination into `RAX`. Sets `ZF` on success.
    fn cmpxchg(&mut self, rex: u8, size: u8) {
        let (reg, rm) = self.modrm(rex);
        let dst = self.load_rm(rm, size) & Self::mask(size);
        let acc = self.r[RAX] & Self::mask(size);
        let r = acc.wrapping_sub(dst);
        self.flags_arith(acc, dst, r, size, true);
        if acc == dst {
            let src = self.r[reg] & Self::mask(size);
            self.store_rm(rm, size, src);
        } else {
            self.store_rm(Rm::Reg(RAX), size, dst);
        }
    }

    /// `CMPXCHG8B`/`CMPXCHG16B`: compare `EDX:EAX`/`RDX:RAX` with the 8/16-byte
    /// destination; swap in `ECX:EBX`/`RCX:RBX` on a match. Sets `ZF`.
    fn cmpxchg16b(&mut self, rm: Rm, wide: bool) {
        let Some(addr) = self.rm_addr(rm) else {
            return;
        };
        if wide {
            let lo = self.rd(addr, 8);
            let hi = self.rd(addr + 8, 8);
            if lo == self.r[RAX] && hi == self.r[RDX] {
                self.wr(addr, 8, self.r[3]); // RBX (index 3, not r[1] which is RCX)
                self.wr(addr + 8, 8, self.r[RCX]);
                self.set(flag::ZF, true);
            } else {
                self.r[RAX] = lo;
                self.r[RDX] = hi;
                self.set(flag::ZF, false);
            }
        } else {
            let lo = self.rd(addr, 4);
            let hi = self.rd(addr + 4, 4);
            if lo == (self.r[RAX] & 0xffff_ffff) && hi == (self.r[RDX] & 0xffff_ffff) {
                self.wr(addr, 4, self.r[3] & 0xffff_ffff); // EBX (RBX is index 3, not r[1]=RCX)
                self.wr(addr + 4, 4, self.r[RCX] & 0xffff_ffff);
                self.set(flag::ZF, true);
            } else {
                self.r[RAX] = lo;
                self.r[RDX] = hi;
                self.set(flag::ZF, false);
            }
        }
    }

    /// `RDRAND`/`RDSEED` (`0F C7 /6`, `/7`): write a fresh non-constant value to
    /// the destination register (size per the operand) and report success
    /// (`CF=1`, other arithmetic flags cleared). A deterministic SplitMix64
    /// stream, perturbed by the TSC, suffices for a boot witness — it only has
    /// to vary so the kernel's crng accepts hardware entropy instead of
    /// spinning on jitterentropy.
    fn rdrand(&mut self, reg: usize, size: u8) {
        let s = self.sys_mut();
        s.rng = s.rng.wrapping_add(0x9e37_79b9_7f4a_7c15 ^ s.tsc);
        let mut z = s.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        // 16-bit RDRAND zero-extends like an 8/16-bit GPR write; 32/64-bit
        // follow the usual write semantics (store_rm masks per `size`).
        self.store_rm(Rm::Reg(reg), size, z);
        self.set(flag::CF, true);
        self.set(flag::OF, false);
        self.set(flag::SF, false);
        self.set(flag::ZF, false);
        self.set(flag::PF, false);
        // AF (bit 4) — not in the `flag` module; clear it directly.
        self.rflags &= !(1u64 << 4);
    }

    /// The architected timestamp counter as `RDTSC`/`RDTSCP` observe it: the
    /// monotonic platform `tsc`, advancing at the fixed calibrated rate
    /// (the TSC:PIT lockstep in `sys_tick`). Monotonic and jitter-free.
    fn read_tsc(&self) -> u64 {
        self.sys().tsc
    }

    /// `XADD`: exchange-and-add — the destination and source swap, then their sum
    /// is stored to the destination, setting the arithmetic flags.
    fn xadd(&mut self, rex: u8, size: u8) {
        let (reg, rm) = self.modrm(rex);
        let dst = self.load_rm(rm, size) & Self::mask(size);
        let src = self.r[reg] & Self::mask(size);
        let sum = dst.wrapping_add(src);
        self.flags_arith(dst, src, sum, size, false);
        self.store_rm(Rm::Reg(reg), size, dst);
        self.store_rm(rm, size, sum & Self::mask(size));
    }

    /// `CPUID` — report the minimal feature set the kernel's early boot checks
    /// require: long mode, the standard feature bits, and a vendor string.
    fn cpuid(&mut self) {
        let leaf = (self.r[RAX] & 0xffff_ffff) as u32;
        let (a, b, c, d): (u32, u32, u32, u32) = match leaf {
            0 => (0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65), // max leaf, "AuthenticAMD"
            1 => {
                // Family/model in EAX; EDX features: FPU,TSC,MSR,PAE,CX8,APIC,
                // SEP,MTRR,PGE,CMOV,PAT,PSE36,CLFSH,MMX,FXSR,SSE,SSE2.
                // ECX bit 30 = RDRAND — the core's hardware RNG (the kernel's
                // crng/jitterentropy path needs it to avoid spinning on the
                // deterministic TSC entropy source). ECX bit 17 = PCID, so the
                // kernel tags address spaces and `CR3` switches (its ASID reuse and
                // text_poke's poking-mm) need not flush the (software) TLB.
                // EBX bits 15:8 = the CLFLUSH line size in 8-byte units; since EDX
                // advertises CLFSH (bit 19), this MUST be non-zero — the kernel sets
                // `x86_clflush_size = 8 * ((ebx>>8)&0xff)` and `cache_line_size()` returns it.
                // A zero here makes `cache_line_size()==0`, and `blk_mq`'s
                // `round_up(rq_size, cache_line_size())` then DIVIDES BY ZERO at boot
                // (#DE in blk_mq_alloc_map_and_rqs → kill init). 8 → a 64-byte line.
                (0x0000_0600, 0x0000_0800, (1 << 30) | (1 << 17), 0x078b_fbff)
            }
            // Leaf 7 sub-leaf 0: EBX bit 18 = RDSEED (the seeding RNG the
            // kernel pairs with RDRAND).
            7 => (0, 1 << 18, 0, 0),
            0x8000_0000 => (0x8000_0008, 0x6874_7541, 0x444d_4163, 0x6974_6e65),
            0x8000_0001 => {
                // EDX: SYSCALL (bit 11), NX (bit 20), Long Mode (bit 29), …
                (0, 0, 0x0000_0001, 0x2010_0800)
            }
            0x8000_0002..=0x8000_0004 => (0x6f6c_6f48, 0x6170_7373, 0x2d65_7363, 0x3436_7858),
            // AMD L1 cache/TLB (leaf 5) + L2/L3 (leaf 6). Vendor is "AuthenticAMD", so Linux reads the
            // cache LINE SIZE from here (`cpu_detect_cache_sizes`), NOT leaf 1's EBX. Bits 7:0 of the
            // L1d (ECX)/L1i (EDX) descriptors and the L2 (leaf-6 ECX) descriptor = the line size in
            // BYTES — MUST be non-zero (64) or `cache_line_size()` is 0 and `blk_mq` divides by zero.
            // ECX/EDX: line=64(b0-7) | lines/tag=1(b8-15) | assoc=8(b16-23) | sizeKB(b24-31).
            0x8000_0005 => (0x0000_0000, 0x0000_0000, 0x2008_0140, 0x2008_0140), // L1d/L1i 32 KiB, 64-byte line
            // Leaf 6 ECX: L2 line=64(b0-7) | lines/tag(b8-11) | assoc=8→0x6(b12-15) | sizeKB(b16-31).
            0x8000_0006 => (0x0000_0000, 0x0000_0000, 0x0200_6140, 0x0000_0000), // L2 512 KiB, 64-byte line
            0x8000_0008 => (0x0000_3028, 0, 0, 0), // 48-bit virt, 40-bit phys
            _ => (0, 0, 0, 0),
        };
        self.r[RAX] = u64::from(a);
        // RBX is GPR index 3 in the x86 encoding (RAX=0, RCX=1, RDX=2, RBX=3) — NOT index 1,
        // which is RCX. Writing EBX to r[1] put it in RCX, where the next line clobbered it with
        // ECX, leaving the real RBX unset. The kernel reads the CLFLUSH line size from CPUID(1).EBX
        // bits 15:8; an unset RBX (0) made cache_line_size()==0 → rq_size==round_up(_,0)==0 →
        // #DE in blk_mq_alloc_map_and_rqs on the disk-root mount path. Write to the real RBX.
        self.r[3] = u64::from(b); // RBX
        self.r[RCX] = u64::from(c);
        self.r[RDX] = u64::from(d);
    }

    /// Read a model-specific register the boot path uses.
    fn rdmsr(&self, ecx: u32) -> u64 {
        match ecx {
            MSR_EFER => self.efer,
            MSR_FS_BASE => self.seg[SegId::Fs as usize].base,
            MSR_GS_BASE => self.seg[SegId::Gs as usize].base,
            MSR_APIC_BASE => LAPIC_BASE | 0x900, // enabled, BSP
            _ => self.sys().msr.get(&ecx).copied().unwrap_or(0),
        }
    }

    /// Write a model-specific register, applying the architectural side effects
    /// (`EFER` long-mode bits, the `FS`/`GS` bases).
    fn wrmsr(&mut self, ecx: u32, val: u64) {
        match ecx {
            MSR_EFER => self.efer = val,
            MSR_FS_BASE => self.seg[SegId::Fs as usize].base = val,
            MSR_GS_BASE => self.seg[SegId::Gs as usize].base = val,
            _ => {
                self.sys_mut().msr.insert(ecx, val);
            }
        }
    }

    /// `SWAPGS` — exchange `GS.base` with the `KERNEL_GS_BASE` MSR (the kernel's
    /// per-CPU base swap on a ring transition).
    fn swapgs(&mut self) {
        let cur = self.seg[SegId::Gs as usize].base;
        let kern = self
            .sys()
            .msr
            .get(&MSR_KERNEL_GS_BASE)
            .copied()
            .unwrap_or(0);
        self.seg[SegId::Gs as usize].base = kern;
        self.sys_mut().msr.insert(MSR_KERNEL_GS_BASE, cur);
    }

    /// Group 7 (`0F 01`): `LGDT`/`LIDT`/`SGDT`/`SIDT`/`LMSW`/`SWAPGS`/`RDTSCP`.
    fn group7(&mut self, rex: u8, _start: u64) -> Result<(), Halt> {
        // Peek the ModRM to distinguish the memory forms (LGDT/LIDT, mod != 3)
        // from the register-encoded ones (SWAPGS = 0F 01 F8, etc.).
        let modrm = *self
            .ram
            .get(self.translate(self.rip) as usize)
            .unwrap_or(&0);
        let md = modrm >> 6;
        let ext = (modrm >> 3) & 7;
        if md == 3 {
            // Register forms keyed by the full ModRM byte.
            self.rip = self.rip.wrapping_add(1);
            match modrm {
                0xf8 => self.swapgs(),
                0xf9 => {
                    // RDTSCP — like RDTSC plus the CPU id in ECX.
                    let tsc = self.read_tsc();
                    self.r[RAX] = tsc & 0xffff_ffff;
                    self.r[RDX] = tsc >> 32;
                    self.r[RCX] = 0;
                }
                // SMAP: CLAC clears the AC flag, STAC sets it. The kernel's user-access
                // primitives (`copy_to_user`/`get_user`/…) wrap the access in `stac`/`clac`
                // so a `#PF` taken mid-access carries `AC=1` and is recognised as a
                // recoverable user fault rather than a fatal kernel access (`page_fault_oops`).
                0xca => self.rflags &= !RFLAGS_AC, // CLAC
                0xcb => self.rflags |= RFLAGS_AC,  // STAC
                _ => {} // MONITOR/MWAIT/etc. — no boot-path effect
            }
            return Ok(());
        }
        let (_e, rm) = self.modrm(rex);
        let addr = self.rm_addr(rm).unwrap_or(0);
        match ext {
            2 => {
                // LGDT: limit (u16) then base (u64).
                let limit = self.rd(addr, 2) as u16;
                let base = self.rd(addr + 2, 8);
                self.sys_mut().gdtr = (base, limit);
            }
            3 => {
                // LIDT.
                let limit = self.rd(addr, 2) as u16;
                let base = self.rd(addr + 2, 8);
                self.sys_mut().idtr = (base, limit);
            }
            0 => {
                // SGDT.
                let (base, limit) = self.sys().gdtr;
                self.wr(addr, 2, u64::from(limit));
                self.wr(addr + 2, 8, base);
            }
            1 => {
                // SIDT.
                let (base, limit) = self.sys().idtr;
                self.wr(addr, 2, u64::from(limit));
                self.wr(addr + 2, 8, base);
            }
            7 => {
                // INVLPG — invalidate the active address space's mapping for the
                // page (coarsened to the whole active PCID; a correct over-flush).
                let pcid = self.active_pcid();
                self.flush_pcid(pcid);
            }
            _ => {}
        }
        Ok(())
    }

    /// `LTR` — load the task register: read the 64-bit TSS descriptor `sel`
    /// selects from the GDT and latch the TSS base (for the ring-transition stack
    /// switch).
    fn load_tr(&mut self, sel: u16) {
        let (gdt, _) = self.sys().gdtr;
        let desc = gdt + u64::from(sel & 0xfff8);
        let lo = self.rd_virt(desc, 8);
        let hi = self.rd_virt(desc + 8, 8);
        let base =
            ((lo >> 16) & 0xff_ffff) | (((lo >> 56) & 0xff) << 24) | ((hi & 0xffff_ffff) << 32);
        self.sys_mut().tr_base = base;
    }

    /// `SYSCALL` — the fast system-call entry: save `RIP→RCX`, `RFLAGS→R11`, mask
    /// `RFLAGS` with `SFMASK`, load `RIP` from `LSTAR`, and switch `CS`/`SS` to the
    /// kernel selectors from `STAR`. The userspace PID-1 enters the kernel here.
    fn syscall_enter(&mut self) {
        SYS_PREV.store(self.r[RAX], core::sync::atomic::Ordering::Relaxed);
        // Thread-creation trace: clone(56)/clone3(435) with CLONE_VM (0x100) = a new THREAD sharing
        // the address space. Counts how many threads the spinning components actually spawn — the
        // over-full hash is most likely a concurrent-insert race (single-threaded resize works).
        #[cfg(feature = "std")]
        if std::env::var_os("HOLO_SYSTRACE").is_some() && (self.r[RAX] == 56 || self.r[RAX] == 435) {
            use core::sync::atomic::{AtomicU32, Ordering};
            static THREADS: AtomicU32 = AtomicU32::new(0);
            let flags = self.r[RDI];
            let is_thread = flags & 0x100 != 0; // CLONE_VM
            let n = THREADS.fetch_add(1, Ordering::Relaxed) + 1;
            std::eprintln!("[CLONE] #{n} nr={} flags={flags:#x} thread(CLONE_VM)={is_thread}", self.r[RAX]);
        }
        let star = self.sys().msr.get(&MSR_STAR).copied().unwrap_or(0);
        let lstar = self.sys().msr.get(&MSR_LSTAR).copied().unwrap_or(0);
        let sfmask = self.sys().msr.get(&MSR_SFMASK).copied().unwrap_or(0);
        self.r[RCX] = self.rip;
        self.r[11] = self.rflags;
        self.rflags &= !sfmask;
        self.rflags &= !RFLAGS_IF;
        let kcs = ((star >> 32) & 0xffff) as u16;
        self.seg[SegId::Cs as usize].selector = kcs;
        self.seg[SegId::Cs as usize].long = true;
        self.seg[SegId::Ss as usize].selector = kcs.wrapping_add(8);
        self.cpl = 0;
        self.rip = lstar;
    }

    /// `SYSRET` — the fast system-call return: restore `RIP←RCX`, `RFLAGS←R11`,
    /// and the userspace `CS`/`SS` from `STAR`, returning to CPL 3.
    fn sysret(&mut self) {
        let star = self.sys().msr.get(&MSR_STAR).copied().unwrap_or(0);
        self.rip = self.r[RCX];
        self.rflags = (self.r[11] & 0x0024_4dd5) | 0x2;
        let ucs = (((star >> 48) & 0xffff) as u16).wrapping_add(16) | 3;
        self.seg[SegId::Cs as usize].selector = ucs;
        self.seg[SegId::Cs as usize].long = true;
        self.seg[SegId::Ss as usize].selector =
            (((star >> 48) & 0xffff) as u16).wrapping_add(8) | 3;
        self.cpl = 3;
        #[cfg(feature = "std")]
        {
            let ret = self.r[RAX] as i64;
            // Trace syscalls returning exactly -1 (the 0xffffffff that wedges the bucket-size
            // loop). Gated by HOLO_SYSTRACE to keep the hot path clean otherwise.
            if ret == -1 && std::env::var_os("HOLO_SYSTRACE").is_some() {
                let nr = SYS_PREV.load(core::sync::atomic::Ordering::Relaxed);
                std::eprintln!("[SYS] nr={nr} -> -1  (return to userrip={:#x})", self.rip);
            }
        }
    }
}

/// Diagnostic: the syscall number of the most recent userspace `SYSCALL`, so `sysret` can
/// log the (number → return) pair when `HOLO_SYSTRACE` is set (tracing failing syscalls).
static SYS_PREV: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Sign-extend the low `size` bytes of `v` to a full `i64`.
fn sign_extend(v: u64, size: u8) -> i64 {
    match size {
        1 => v as u8 as i8 as i64,
        2 => v as u16 as i16 as i64,
        4 => v as u32 as i32 as i64,
        _ => v as i64,
    }
}

/// A decoded ModRM r/m operand: a register index, a resolved effective address,
/// or a RIP-relative operand (`disp`, `segment base`) resolved lazily once the
/// full instruction — including any trailing immediate — has decoded.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u64),
    RipRel(i64, u64),
}

/// A string-instruction repeat prefix (`REP`/`REPE` = `F3`, `REPNE` = `F2`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RepKind {
    None,
    Rep,
    Repne,
}

/// The string instruction family (`MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS`).
#[derive(Clone, Copy)]
enum StringOp {
    Movs,
    Stos,
    Lods,
    Scas,
    Cmps,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// G1: the linear framebuffer is reserved at the TOP of guest RAM — a pattern written via
    /// `write_framebuffer` round-trips through `read_framebuffer` and lives at `fb_phys_base()`.
    #[test]
    fn x64_framebuffer_reserved_at_top_of_ram() {
        let ram = 64 * 1024 * 1024; // 64 MiB ≫ the ~4 MiB framebuffer
        let mut cpu = Cpu::new(ram);
        assert_eq!(Cpu::FB_SIZE, Cpu::FB_W * Cpu::FB_H * 4);
        assert_eq!(cpu.fb_phys_base(), (ram - Cpu::FB_SIZE) as u64);
        // A recognizable per-pixel pattern (R,G,B,A bytes derived from the index).
        let mut pat = vec![0u8; Cpu::FB_SIZE];
        for (i, b) in pat.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        cpu.write_framebuffer(&pat);
        assert_eq!(cpu.read_framebuffer(), pat, "framebuffer round-trips");
        // It really lives at fb_phys_base in guest RAM (what screen_info.lfb_base advertises).
        let base = cpu.fb_phys_base() as usize;
        assert_eq!(&cpu.ram()[base..base + Cpu::FB_SIZE], &pat[..]);
    }

    /// G2: `build_boot_params` advertises the framebuffer to the kernel — `screen_info` describes an
    /// EFI linear framebuffer at `fb_phys_base()` with the right geometry, and the e820 map RESERVES
    /// the framebuffer region (type 2) so the kernel never allocates over the scanout.
    #[test]
    fn x64_boot_params_advertise_framebuffer() {
        let ram = 128 * 1024 * 1024u64;
        let mut cpu = Cpu::new(ram as usize);
        cpu.build_boot_params("console=ttyS0 console=tty0", ram, false);
        let zp = ZERO_PAGE as usize;
        let r = cpu.ram();
        let u16at = |o: usize| u16::from_le_bytes(r[zp + o..zp + o + 2].try_into().unwrap());
        let u32at = |o: usize| u32::from_le_bytes(r[zp + o..zp + o + 4].try_into().unwrap());
        let fb_base = (ram as usize - Cpu::FB_SIZE) as u32;
        assert_eq!(r[zp + 0x0f], 0x70, "orig_video_isVGA = VIDEO_TYPE_EFI");
        assert_eq!(u16at(0x12), Cpu::FB_W as u16, "lfb_width");
        assert_eq!(u16at(0x14), Cpu::FB_H as u16, "lfb_height");
        assert_eq!(u16at(0x16), 32, "lfb_depth");
        assert_eq!(u32at(0x18), fb_base, "lfb_base = fb_phys_base");
        assert_eq!(u16at(0x24), (Cpu::FB_W * 4) as u16, "lfb_linelength");
        // e820 must contain a type-2 (reserved) entry covering exactly the framebuffer.
        let n = r[zp + 0x1e8] as usize;
        let mut found_fb = false;
        let mut covers_fb_as_ram = false;
        for i in 0..n {
            let off = zp + 0x2d0 + i * 20;
            let addr = u64::from_le_bytes(r[off..off + 8].try_into().unwrap());
            let size = u64::from_le_bytes(r[off + 8..off + 16].try_into().unwrap());
            let ty = u32::from_le_bytes(r[off + 16..off + 20].try_into().unwrap());
            if ty == 2 && addr == u64::from(fb_base) && size == Cpu::FB_SIZE as u64 {
                found_fb = true;
            }
            if ty == 1 && addr <= u64::from(fb_base) && addr + size > u64::from(fb_base) {
                covers_fb_as_ram = true; // a RAM entry overlapping the FB → the kernel could clobber it
            }
        }
        assert!(found_fb, "e820 reserves the framebuffer region");
        assert!(!covers_fb_as_ram, "no usable-RAM e820 entry overlaps the framebuffer");
    }

    /// Load + run a flat x86-64 code blob; assert it stopped cleanly.
    fn run(code: &[u8]) -> Cpu {
        let mut cpu = Cpu::new(64 * 1024);
        cpu.load_at(0, code);
        let h = cpu.run(100_000);
        assert!(
            matches!(h, Halt::Halted | Halt::OutOfBudget),
            "unexpected halt: {h:?}"
        );
        cpu
    }

    /// κ-snapshot foundation: the core CPU state + RAM round-trip through `snapshot`/`restore`
    /// byte-for-byte (the serialization the κ-snapshot path content-addresses + streams). The
    /// device `Sys` state is the remaining piece for a full boot-resume.
    #[test]
    fn snapshot_restores_core_cpu_state_and_ram() {
        let mut a = Cpu::new(64 * 1024);
        for (i, r) in a.r.iter_mut().enumerate() {
            *r = 0x1111_0000 + i as u64;
        }
        a.rip = 0xdead_beef;
        a.rflags = 0x2 | (1 << 9);
        a.insns = 12345;
        a.cr0 = 0x8000_0011;
        a.cr2 = 0xcafe;
        a.cr3 = 0x1000;
        a.cr4 = 0x20;
        a.efer = 0x500;
        for (i, d) in a.dr.iter_mut().enumerate() {
            *d = 0xd0 + i as u64;
        }
        for (i, x) in a.xmm.iter_mut().enumerate() {
            *x = (0xfeed_0000_0000_0000_0000_0000_0000_0000u128) | (i as u128) << 64 | i as u128;
        }
        // x87 FPU state (musl formats long double via x87; FXSAVE/context-switch depends on it).
        a.fpr = [1.5, -2.25, 3.0, 0.0, 1e9, -1e-9, 42.0, 7.0];
        a.ftop = 5;
        a.fcw = 0x027f;
        a.fsw = 0x3800;
        a.ram[0x2000..0x2008].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        a.seg[1] = Seg { selector: 0x10, base: 0, long: true }; // CS
        a.cpl = 3;
        // non-trivial device state (the plain-data Sys: console, MSRs, timers, interrupts)
        let sys = a.sys.as_mut().unwrap();
        sys.tsc = 0x1234_5678;
        sys.msr.insert(0xC000_0080, 0x500); // IA32_EFER shadow
        sys.msr.insert(0xC000_0100, 0xffff_8000_0000_0000); // FS_BASE
        sys.uart.output.extend_from_slice(b"early boot log");
        sys.pic.request = 0x42;
        sys.lapic.svr = 0x1ff;
        sys.pit.counter = 0x9999;
        sys.dcache = vec![1, 2, 3, 4, 5];
        // A bound virtio-input device with live queue state — must survive resume (its queue
        // addresses live here, not in RAM, so the resumed guest's driver finds a live device).
        a.attach_virtio_input();
        {
            let d = a.sys.as_mut().unwrap().virtioinput.as_mut().unwrap();
            d.status = 0xf;
            d.device_features_sel = 1;
            d.driver_features_sel = 1;
            d.driver_features = [0, 1];
            d.queue_sel = 1;
            d.queue_num = [256, 64];
            d.queue_ready = [1, 1];
            d.desc_addr = [0x9000, 0xa000];
            d.avail_addr = [0x9100, 0xa100];
            d.used_addr = [0x9200, 0xa200];
            d.last_avail = [7, 3];
            d.interrupt_status = 1;
            d.cfg_select = 0x11;
            d.cfg_subsel = 0x01;
        }

        let snap = a.snapshot();
        // restore into a DIFFERENTLY-SIZED fresh machine — restore resizes RAM to match.
        let mut b = Cpu::new(4096);
        assert!(b.restore(&snap), "snapshot round-trips");

        assert_eq!(b.r, a.r, "GP registers");
        assert_eq!((b.rip, b.rflags, b.insns), (a.rip, a.rflags, a.insns));
        assert_eq!((b.cr0, b.cr2, b.cr3, b.cr4, b.efer), (a.cr0, a.cr2, a.cr3, a.cr4, a.efer));
        assert_eq!(b.dr, a.dr, "debug registers");
        assert_eq!(b.xmm, a.xmm, "SSE (xmm) registers — a mid-SSE machine resumes bit-exact");
        assert_eq!(b.fpr, a.fpr, "x87 FPU stack — a mid-float machine resumes bit-exact");
        assert_eq!((b.ftop, b.fcw, b.fsw), (a.ftop, a.fcw, a.fsw), "x87 TOP + control/status");
        assert_eq!(b.cpl, a.cpl, "CPL");
        // The virtio-input device survives resume with its negotiated queue state intact.
        let bi = b.sys.as_ref().unwrap().virtioinput.as_ref().expect("virtio-input survives resume");
        assert_eq!(
            (bi.queue_ready, bi.desc_addr, bi.last_avail, bi.cfg_select, bi.status),
            ([1, 1], [0x9000, 0xa000], [7, 3], 0x11, 0xf),
            "virtio-input queue registers round-trip",
        );
        assert_eq!(b.ram, a.ram, "RAM restored byte-for-byte (and resized)");
        // The decisive check: re-snapshotting the restored machine yields IDENTICAL bytes, so
        // EVERY field (core + segments + CPL + all devices + RAM) round-tripped — a missed or
        // misordered field would diverge here.
        assert_eq!(b.snapshot(), snap, "the full machine round-trips identically");
        assert!(!b.restore(&snap[..snap.len() - 1]), "a truncated snapshot is rejected");
    }

    /// κ-snapshot Step 2: RAM content-addresses into a per-page BLAKE3 manifest whose UNIQUE
    /// pages dedup in a `KappaStore`, and a κ-resume verifies every page (L5) + reconstructs RAM
    /// bit-exact. (The boot-scale 22.8× dedup + bit-exact κ-resume to userspace is gated by the
    /// integration test `kappa_snapshot_kappa_resume_to_userspace`.)
    #[test]
    fn kappa_snapshot_dedups_ram_and_restores_bit_exact() {
        use hologram_store_mem::MemKappaStore;
        let mut a = Cpu::new(16 * KAPPA_PAGE); // 16 pages = 64 KiB
        a.r[0] = 0x0abc;
        a.rip = 0x4000;
        a.cpl = 3;
        a.sys.as_mut().unwrap().tsc = 777;
        a.sys.as_mut().unwrap().uart.output.extend_from_slice(b"hi");
        // page 0: a unique pattern; pages 1..=14: zero (→ one κ); page 15: a copy of page 0 (→ its κ).
        a.ram[0..8].copy_from_slice(&0xdead_beef_cafe_babeu64.to_le_bytes());
        let p0 = a.ram[0..KAPPA_PAGE].to_vec();
        a.ram[15 * KAPPA_PAGE..16 * KAPPA_PAGE].copy_from_slice(&p0);

        let store = MemKappaStore::new();
        let snap = a.snapshot_kappa(&store).expect("snapshot_kappa");
        assert_eq!(snap.page_count(), 16, "16-page manifest");
        assert_eq!(
            store.approximate_count(),
            2,
            "16 RAM pages dedup to 2 unique κ (the nonzero pattern + the all-zero page)"
        );

        // Resume into a fresh, differently-sized core — every page fetched-by-κ + verified (L5).
        let mut b = Cpu::new(KAPPA_PAGE);
        assert!(b.restore_kappa(&snap, &store), "κ-resume verifies + reconstructs");
        assert_eq!(b.ram, a.ram, "RAM reconstructed byte-for-byte from the κ pages");
        assert_eq!(b.snapshot(), a.snapshot(), "full machine identical after a κ-resume");

        // A missing page is refused, not silently zero-filled (verify-before-use has nothing to
        // serve) — resuming against an empty store fails.
        let mut c = Cpu::new(KAPPA_PAGE);
        assert!(
            !c.restore_kappa(&snap, &MemKappaStore::new()),
            "a κ-resume against a store missing the pages is refused"
        );
    }

    /// κ-snapshot Step 3a: the whole machine seals into ONE content κ, and an adopter holding only
    /// that κ + the store resumes bit-exact. The manifest (state + page-κ list) round-trips, and a
    /// κ whose blob isn't a valid manifest — or one absent from the store — is refused (L5).
    #[test]
    fn kappa_snapshot_seal_and_resume_from_one_kappa() {
        use hologram_store_mem::MemKappaStore;
        let mut a = Cpu::new(16 * KAPPA_PAGE);
        a.r[3] = 0xfeed;
        a.rip = 0x8000;
        a.cpl = 3;
        a.sys.as_mut().unwrap().tsc = 4242;
        a.ram[0..8].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        a.ram[15 * KAPPA_PAGE..15 * KAPPA_PAGE + 8].copy_from_slice(&0x99u64.to_le_bytes());

        let store = MemKappaStore::new();

        // The manifest serialization round-trips deterministically.
        let snap = a.snapshot_kappa(&store).unwrap();
        let manifest = snap.to_manifest_bytes();
        let reparsed = KappaSnapshot::from_manifest_bytes(&manifest).expect("manifest parses");
        assert_eq!(reparsed.to_manifest_bytes(), manifest, "manifest round-trips");
        assert_eq!(reparsed.page_count(), snap.page_count());

        // Seal → one κ; resume from ONLY that κ + the store.
        let kappa = a.seal_kappa(&store).expect("seal");
        let mut b = Cpu::new(KAPPA_PAGE);
        assert!(b.resume_kappa(&kappa, &store), "resume from the sealed κ");
        assert_eq!(b.ram, a.ram, "RAM bit-exact via the sealed κ");
        assert_eq!(b.snapshot(), a.snapshot(), "full machine identical after a sealed-κ resume");

        // L5: a κ whose blob is real but ISN'T a manifest is refused (verify passes, parse fails).
        let mut c = Cpu::new(KAPPA_PAGE);
        let not_a_manifest = store.put("blake3", b"not a manifest").unwrap();
        assert!(!c.resume_kappa(&not_a_manifest, &store), "a non-manifest κ is refused");

        // A sealed κ absent from the store is refused (nothing to fetch).
        let mut d = Cpu::new(KAPPA_PAGE);
        assert!(!d.resume_kappa(&kappa, &MemKappaStore::new()), "a κ missing from the store is refused");
    }

    /// κ-snapshot Step 3b/3c: a machine sealed on peer A resumes BIT-EXACT on a *second* peer B
    /// that starts with an empty store and pulls the manifest + unique pages over `content_net`,
    /// verifying every byte on receipt — and a FORGING peer (serving attacker-chosen bytes for any
    /// κ) is refused on receipt (L5), so the adversary cannot corrupt the resume. Boot once on A,
    /// resume on B.
    #[test]
    fn kappa_snapshot_resumes_on_a_second_peer_over_content_net() {
        use crate::content_net::{drive_fetch, forging_peer, peer, PacketLink};
        use alloc::sync::Arc;
        use hologram_store_mem::MemKappaStore;
        const MTU: u32 = 64 * 1024;

        // Origin machine A with non-trivial state + RAM (page 0 unique, page 7 unique, rest zero).
        let mut a = Cpu::new(16 * KAPPA_PAGE);
        a.r[5] = 0x00c0_ffee;
        a.rip = 0x1234;
        a.sys.as_mut().unwrap().tsc = 1000;
        a.ram[0..8].copy_from_slice(&0xabad_1dea_dead_c0deu64.to_le_bytes());
        a.ram[7 * KAPPA_PAGE..7 * KAPPA_PAGE + 4].copy_from_slice(&[1, 2, 3, 4]);

        // A seals into its store → the whole machine is ONE κ.
        let a_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
        let snapshot_kappa = a.seal_kappa(&*a_store).expect("seal");

        // Adopter B: empty store + a content-net link to A.
        let b_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
        let (b_link, a_link) = PacketLink::loopback_pair(MTU);
        let server = peer(a_link, a_store.clone());
        let fetcher = peer(b_link, b_store.clone());

        // B pulls the manifest by κ (verify-on-receipt), then each UNIQUE page κ — into its store.
        let manifest = drive_fetch(&fetcher, &server, &snapshot_kappa).expect("fetch manifest");
        b_store.put("blake3", &manifest).unwrap();
        let snap = KappaSnapshot::from_manifest_bytes(&manifest).expect("manifest");
        let mut fetched = 0usize;
        for k in snap.page_kappas() {
            if b_store.contains(k) {
                continue; // dedup — fetch each unique page once
            }
            let page = drive_fetch(&fetcher, &server, k).expect("fetch page");
            b_store.put("blake3", &page).unwrap();
            fetched += 1;
        }

        // B resumes from its now-populated store — bit-exact, having pulled only the unique pages.
        let mut b = Cpu::new(KAPPA_PAGE);
        assert!(b.resume_kappa(&snapshot_kappa, &*b_store), "B resumes from the adopted κ");
        assert_eq!(b.ram, a.ram, "B's RAM is byte-identical to A");
        assert_eq!(b.snapshot(), a.snapshot(), "B ≡ A after a content-net resume");
        assert!(
            fetched < snap.page_count(),
            "deduped over the wire: {fetched} unique pages fetched < {} total",
            snap.page_count()
        );

        // 3c — a FORGING peer answers every fetch with attacker bytes; the fetcher re-derives the
        // κ and REJECTS them, so B cannot even obtain a verifiable manifest (the resume is refused,
        // not silently corrupted).
        let (g_link, f_link) = PacketLink::loopback_pair(MTU);
        let forger = forging_peer(f_link, b"forged-bytes".to_vec());
        let victim_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
        let victim = peer(g_link, victim_store);
        assert!(
            drive_fetch(&victim, &forger, &snapshot_kappa).is_none(),
            "a forged response is rejected on receipt (L5) — the adversary cannot serve the snapshot"
        );
    }

    /// κ-snapshot Step-4 Phase 1: the self-contained, deduplicated κ-blob is smaller than the flat
    /// snapshot (zero/duplicate pages collapse), resumes RAM bit-exact, and a tampered or truncated
    /// blob is refused (L5). This is what the browser persists to OPFS so a fresh tab resumes from
    /// only the unique pages.
    #[test]
    fn kappa_snapshot_blob_dedups_and_restores_bit_exact() {
        let mut a = Cpu::new(16 * KAPPA_PAGE);
        a.r[2] = 0xbeef;
        a.rip = 0x5000;
        a.sys.as_mut().unwrap().tsc = 55;
        a.ram[0..8].copy_from_slice(&0xfeed_face_dead_beefu64.to_le_bytes());
        a.ram[9 * KAPPA_PAGE..9 * KAPPA_PAGE + 8].copy_from_slice(&0x42u64.to_le_bytes());

        let blob = a.snapshot_kappa_blob();
        let flat = a.snapshot();
        assert!(
            blob.len() < flat.len(),
            "the deduped κ-blob ({}) is smaller than the flat snapshot ({})",
            blob.len(),
            flat.len()
        );

        // Resume into a fresh, differently-sized core — bit-exact.
        let mut b = Cpu::new(KAPPA_PAGE);
        assert!(b.restore_kappa_blob(&blob), "κ-blob resumes");
        assert_eq!(b.ram, a.ram, "RAM reconstructed byte-for-byte from the κ-blob");
        assert_eq!(b.snapshot(), flat, "full machine identical after a κ-blob resume");

        // L5: flipping a byte in a bundled page makes it fail to re-derive → refused.
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(
            !Cpu::new(KAPPA_PAGE).restore_kappa_blob(&tampered),
            "a tampered κ-blob is refused (page no longer re-derives to its κ)"
        );
        assert!(
            !Cpu::new(KAPPA_PAGE).restore_kappa_blob(&blob[..blob.len() / 2]),
            "a truncated κ-blob is refused"
        );
    }

    /// DIFFERENTIAL (atomics): CMPXCHG/XADD/XCHG — the primitives behind futexes, glib atomic
    /// refcounts and lock-free updates. A subtle bug here corrupts shared state in the MULTITHREADED
    /// X components (GTK/GDBus worker threads) while single-threaded CLIs stay fine — matching the
    /// observed "components spin, CLIs pass" split. LOCK is a no-op for a single core (atomicity only),
    /// so the bare op result+flags is what matters.
    #[test]
    fn atomic_ops_match_reference() {
        const ZF: u64 = 1 << 6;
        // run `op` with eax/ebx/ecx preset → (eax, ebx, ecx, zf)
        let run = |bytes: &[u8], eax: u64, ebx: u64, ecx: u64| -> (u64, u64, u64, bool) {
            let mut cpu = Cpu::new(0x10000);
            cpu.ram[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
            cpu.rip = 0x1000;
            cpu.rflags = 0x2;
            cpu.r[RAX] = eax; cpu.r[3] = ebx; cpu.r[RCX] = ecx;
            let _ = cpu.step();
            (cpu.r[RAX] & 0xffff_ffff, cpu.r[3] & 0xffff_ffff, cpu.r[RCX] & 0xffff_ffff, cpu.rflags & ZF != 0)
        };
        let mut fails: Vec<String> = Vec::new();
        for (eax, ebx, ecx) in [(5u64,5u64,9u64),(5,7,9),(0,0,1),(0xffffffff,0xffffffff,1),(0x80000000,0x80000000,2),(1,2,3)] {
            // CMPXCHG ebx, ecx (0f b1 cb): temp=ebx; if eax==ebx {ZF=1; ebx=ecx} else {ZF=0; eax=ebx}
            let (ra, rb, _rc, zf) = run(&[0x0f, 0xb1, 0xcb], eax, ebx, ecx);
            let (wa, wb, wzf) = if eax == ebx { (eax, ecx, true) } else { (ebx, ebx, false) };
            if ra != wa || rb != wb || zf != wzf {
                fails.push(format!("cmpxchg eax={eax:#x} ebx={ebx:#x} ecx={ecx:#x}: emu(eax={ra:#x},ebx={rb:#x},zf={zf}) ref(eax={wa:#x},ebx={wb:#x},zf={wzf})"));
            }
            // XADD ebx, ecx (0f c1 cb): temp=ebx; ebx=ebx+ecx; ecx=temp
            let (_ra, rb, rc, _zf) = run(&[0x0f, 0xc1, 0xcb], eax, ebx, ecx);
            let (wb, wc) = ((ebx + ecx) & 0xffff_ffff, ebx);
            if rb != wb || rc != wc {
                fails.push(format!("xadd ebx={ebx:#x} ecx={ecx:#x}: emu(ebx={rb:#x},ecx={rc:#x}) ref(ebx={wb:#x},ecx={wc:#x})"));
            }
            // XCHG ebx, ecx (87 cb): swap
            let (_ra, rb, rc, _zf) = run(&[0x87, 0xcb], eax, ebx, ecx);
            if rb != ecx || rc != ebx {
                fails.push(format!("xchg ebx={ebx:#x} ecx={ecx:#x}: emu(ebx={rb:#x},ecx={rc:#x})"));
            }
        }
        assert!(fails.is_empty(), "{} atomic mismatches:\n{}", fails.len(), fails.join("\n"));
    }

    /// DIFFERENTIAL (SSE2 integer/SIMD): the packed-integer ops added blind for Xorg/pixman bring-up,
    /// compared lane-by-lane to an independent Rust reference over edge-case vectors. The desktop
    /// blocker is in the X/rendering path (which uses these heavily) while bare glib/GTK CLIs pass —
    /// so a lane-wrong SIMD op is the prime suspect. `op xmm0, xmm1` = `66 0f <op> c1`.
    #[test]
    fn simd_ops_match_reference() {
        fn emu(bytes: &[u8], a: u128, b: u128) -> u128 {
            let mut cpu = Cpu::new(0x10000);
            cpu.ram[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
            cpu.rip = 0x1000;
            cpu.rflags = 0x2;
            cpu.xmm[0] = a;
            cpu.xmm[1] = b;
            let _ = cpu.step();
            cpu.xmm[0]
        }
        // lane split/combine for element size `sz` bytes
        let lanes = |v: u128, sz: usize| -> Vec<u64> {
            (0..16 / sz).map(|i| {
                let mut x = 0u64;
                for k in 0..sz { x |= (((v >> (8 * (i * sz + k))) & 0xff) as u64) << (8 * k); }
                x
            }).collect()
        };
        let pack = |ls: &[u64], sz: usize| -> u128 {
            let mut v = 0u128;
            for (i, &x) in ls.iter().enumerate() {
                for k in 0..sz { v |= (u128::from((x >> (8 * k)) & 0xff)) << (8 * (i * sz + k)); }
            }
            v
        };
        // sign-extend an sz-byte lane to i64
        let sx = |x: u64, sz: usize| -> i64 { let s = 64 - sz * 8; ((x << s) as i64) >> s };
        let binlane = |a: u128, b: u128, sz: usize, f: &dyn Fn(u64, u64) -> u64| -> u128 {
            let (la, lb) = (lanes(a, sz), lanes(b, sz));
            pack(&la.iter().zip(&lb).map(|(&x, &y)| f(x, y) & ((1u128 << (8 * sz)) - 1) as u64).collect::<Vec<_>>(), sz)
        };
        let mask = |sz: usize| -> u64 { if sz == 8 { u64::MAX } else { (1u64 << (8 * sz)) - 1 } };

        let vecs: [u128; 8] = [
            0,
            u128::MAX,
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            0x8080_8080_8080_8080_8080_8080_8080_8080,
            0x7fff_7fff_7fff_7fff_7fff_7fff_7fff_7fff,
            0xffff_0001_8000_7fff_0000_ffff_1234_abcd,
            0xdead_beef_cafe_babe_0bad_f00d_1337_c0de,
            0x00ff_00ff_00ff_00ff_00ff_00ff_00ff_00ff,
        ];
        let mut fails: Vec<String> = Vec::new();
        let mut chk = |name: &str, bytes: &[u8], a: u128, b: u128, want: u128| {
            let got = emu(bytes, a, b);
            if got != want {
                fails.push(format!("{name} a={a:032x} b={b:032x}: emu={got:032x} ref={want:032x}"));
            }
        };

        for &a in &vecs {
            for &b in &vecs {
                // PADD/PSUB B/W/D/Q
                for (nm, op, sz) in [("paddb",0xfc,1),("paddw",0xfd,2),("paddd",0xfe,4),("paddq",0xd4,8)] {
                    chk(nm, &[0x66,0x0f,op,0xc1], a, b, binlane(a,b,sz,&|x,y| x.wrapping_add(y)));
                }
                for (nm, op, sz) in [("psubb",0xf8,1),("psubw",0xf9,2),("psubd",0xfa,4),("psubq",0xfb,8)] {
                    chk(nm, &[0x66,0x0f,op,0xc1], a, b, binlane(a,b,sz,&|x,y| x.wrapping_sub(y)));
                }
                // PCMPEQ B/W/D
                for (nm, op, sz) in [("pcmpeqb",0x74,1),("pcmpeqw",0x75,2),("pcmpeqd",0x76,4)] {
                    let m = mask(sz);
                    chk(nm, &[0x66,0x0f,op,0xc1], a, b, binlane(a,b,sz,&|x,y| if x==y {m} else {0}));
                }
                // PCMPGT B/W/D (signed)
                for (nm, op, sz) in [("pcmpgtb",0x64,1),("pcmpgtw",0x65,2),("pcmpgtd",0x66,4)] {
                    let m = mask(sz);
                    chk(nm, &[0x66,0x0f,op,0xc1], a, b, binlane(a,b,sz,&|x,y| if sx(x,sz) > sx(y,sz) {m} else {0}));
                }
                // logic
                chk("pand", &[0x66,0x0f,0xdb,0xc1], a, b, a & b);
                chk("pandn", &[0x66,0x0f,0xdf,0xc1], a, b, !a & b);
                chk("por", &[0x66,0x0f,0xeb,0xc1], a, b, a | b);
                chk("pxor", &[0x66,0x0f,0xef,0xc1], a, b, a ^ b);
                // PMULLW / PMULHW / PMULHUW (words)
                chk("pmullw", &[0x66,0x0f,0xd5,0xc1], a, b, binlane(a,b,2,&|x,y| (x.wrapping_mul(y)) & 0xffff));
                chk("pmulhw", &[0x66,0x0f,0xe5,0xc1], a, b, binlane(a,b,2,&|x,y| ((sx(x,2)*sx(y,2)) >> 16) as u64 & 0xffff));
                chk("pmulhuw", &[0x66,0x0f,0xe4,0xc1], a, b, binlane(a,b,2,&|x,y| ((x*y) >> 16) & 0xffff));
                // PMIN/MAX UB / SW
                chk("pminub", &[0x66,0x0f,0xda,0xc1], a, b, binlane(a,b,1,&|x,y| x.min(y)));
                chk("pmaxub", &[0x66,0x0f,0xde,0xc1], a, b, binlane(a,b,1,&|x,y| x.max(y)));
                chk("pminsw", &[0x66,0x0f,0xea,0xc1], a, b, binlane(a,b,2,&|x,y| (sx(x,2).min(sx(y,2))) as u64 & 0xffff));
                chk("pmaxsw", &[0x66,0x0f,0xee,0xc1], a, b, binlane(a,b,2,&|x,y| (sx(x,2).max(sx(y,2))) as u64 & 0xffff));
                // saturating add/sub
                chk("paddusb", &[0x66,0x0f,0xdc,0xc1], a, b, binlane(a,b,1,&|x,y| (x+y).min(0xff)));
                chk("paddusw", &[0x66,0x0f,0xdd,0xc1], a, b, binlane(a,b,2,&|x,y| (x+y).min(0xffff)));
                chk("psubusb", &[0x66,0x0f,0xd8,0xc1], a, b, binlane(a,b,1,&|x,y| x.saturating_sub(y)));
                chk("psubusw", &[0x66,0x0f,0xd9,0xc1], a, b, binlane(a,b,2,&|x,y| x.saturating_sub(y)));
                chk("paddsb", &[0x66,0x0f,0xec,0xc1], a, b, binlane(a,b,1,&|x,y| (sx(x,1)+sx(y,1)).clamp(-128,127) as u64 & 0xff));
                chk("paddsw", &[0x66,0x0f,0xed,0xc1], a, b, binlane(a,b,2,&|x,y| (sx(x,2)+sx(y,2)).clamp(-32768,32767) as u64 & 0xffff));
                chk("psubsb", &[0x66,0x0f,0xe8,0xc1], a, b, binlane(a,b,1,&|x,y| (sx(x,1)-sx(y,1)).clamp(-128,127) as u64 & 0xff));
                chk("psubsw", &[0x66,0x0f,0xe9,0xc1], a, b, binlane(a,b,2,&|x,y| (sx(x,2)-sx(y,2)).clamp(-32768,32767) as u64 & 0xffff));
                // PAVGB / PAVGW (rounding average)
                chk("pavgb", &[0x66,0x0f,0xe0,0xc1], a, b, binlane(a,b,1,&|x,y| (x+y+1) >> 1));
                chk("pavgw", &[0x66,0x0f,0xe3,0xc1], a, b, binlane(a,b,2,&|x,y| (x+y+1) >> 1));
                // PUNPCK low/high B/W/D/Q
                {
                    let (lb_, hb_) = (lanes(a,1), lanes(b,1));
                    let lo: Vec<u64> = (0..8).flat_map(|i| [lb_[i], hb_[i]]).collect();
                    chk("punpcklbw", &[0x66,0x0f,0x60,0xc1], a, b, pack(&lo,1));
                    let hi: Vec<u64> = (8..16).flat_map(|i| [lb_[i], hb_[i]]).collect();
                    chk("punpckhbw", &[0x66,0x0f,0x68,0xc1], a, b, pack(&hi,1));
                    let (lw, hw) = (lanes(a,2), lanes(b,2));
                    let lo: Vec<u64> = (0..4).flat_map(|i| [lw[i], hw[i]]).collect();
                    chk("punpcklwd", &[0x66,0x0f,0x61,0xc1], a, b, pack(&lo,2));
                    let (ld, hd) = (lanes(a,4), lanes(b,4));
                    let lo: Vec<u64> = (0..2).flat_map(|i| [ld[i], hd[i]]).collect();
                    chk("punpckldq", &[0x66,0x0f,0x62,0xc1], a, b, pack(&lo,4));
                }
                // PACKUSWB / PACKSSWB / PACKSSDW (dst lanes then src lanes)
                {
                    let usb = |w: i64| w.clamp(0,255) as u64;
                    let mut o: Vec<u64> = lanes(a,2).iter().map(|&x| usb(sx(x,2))).collect();
                    o.extend(lanes(b,2).iter().map(|&y| usb(sx(y,2))));
                    chk("packuswb", &[0x66,0x0f,0x67,0xc1], a, b, pack(&o,1));
                    let ssb = |w: i64| w.clamp(-128,127) as u64 & 0xff;
                    let mut o: Vec<u64> = lanes(a,2).iter().map(|&x| ssb(sx(x,2))).collect();
                    o.extend(lanes(b,2).iter().map(|&y| ssb(sx(y,2))));
                    chk("packsswb", &[0x66,0x0f,0x63,0xc1], a, b, pack(&o,1));
                    let ssw = |d: i64| d.clamp(-32768,32767) as u64 & 0xffff;
                    let mut o: Vec<u64> = lanes(a,4).iter().map(|&x| ssw(sx(x,4))).collect();
                    o.extend(lanes(b,4).iter().map(|&y| ssw(sx(y,4))));
                    chk("packssdw", &[0x66,0x0f,0x6b,0xc1], a, b, pack(&o,2));
                }
                // PMADDWD: 4 dwords, each a[2j]*b[2j] + a[2j+1]*b[2j+1] (signed words)
                {
                    let (la, lb) = (lanes(a,2), lanes(b,2));
                    let mut o = [0u64; 4];
                    for j in 0..4 { o[j] = ((sx(la[2*j],2)*sx(lb[2*j],2) + sx(la[2*j+1],2)*sx(lb[2*j+1],2)) as u64) & 0xffff_ffff; }
                    chk("pmaddwd", &[0x66,0x0f,0xf5,0xc1], a, b, pack(&o,4));
                }
                // PSADBW: |a-b| per byte, summed per 64-bit half → low word of each half
                {
                    let (la, lb) = (lanes(a,1), lanes(b,1));
                    let mut o = [0u64; 2];
                    for h in 0..2 { let mut s=0u64; for k in 0..8 { s += (la[h*8+k] as i64 - lb[h*8+k] as i64).unsigned_abs(); } o[h]=s; }
                    chk("psadbw", &[0x66,0x0f,0xf6,0xc1], a, b, pack(&o,8));
                }
                // PSHUFD imm=0x1b (reverse dwords) — shuffles the SOURCE (xmm1 = b) into dst.
                {
                    let ld = lanes(b,4);
                    let imm = 0x1bu8;
                    let o: Vec<u64> = (0..4).map(|i| ld[((imm >> (2*i)) & 3) as usize]).collect();
                    chk("pshufd", &[0x66,0x0f,0x70,0xc1,imm], a, b, pack(&o,4));
                }
            }
        }
        // PMOVMSKB (separate loop so `chk`'s borrow of `fails` has ended). pmovmskb eax,xmm1 = 66 0f d7 c1
        for &a in &vecs {
            let mut cpu = Cpu::new(0x10000);
            cpu.ram[0x1000..0x1004].copy_from_slice(&[0x66,0x0f,0xd7,0xc1]);
            cpu.rip = 0x1000; cpu.rflags = 0x2; cpu.xmm[1] = a;
            let _ = cpu.step();
            let mut want = 0u64;
            for i in 0..16 { if (a >> (8*i+7)) & 1 == 1 { want |= 1 << i; } }
            if cpu.r[RAX] != want { fails.push(format!("pmovmskb a={a:032x}: emu={:#x} ref={want:#x}", cpu.r[RAX])); }
        }
        assert!(fails.is_empty(), "{} SIMD mismatches:\n{}", fails.len(), fails.join("\n"));
    }

    /// DIFFERENTIAL: execute each integer instruction in the emulator and compare its result +
    /// flags to an INDEPENDENT reference model (computed from the ISA spec in safe Rust — the crate
    /// forbids `unsafe`, so no host-CPU asm oracle), over edge-case operands. Catches subtle
    /// flag/result bugs that wedge real software (e.g. a glib hash-table resize compare that
    /// mis-branches on a wrong flag). Reports every mismatch, then asserts none.
    #[test]
    fn integer_ops_match_reference() {
        const CF: u64 = 1 << 0;
        const PF: u64 = 1 << 2;
        const AF: u64 = 1 << 4;
        const ZF: u64 = 1 << 6;
        const SF: u64 = 1 << 7;
        const OF: u64 = 1 << 11;
        // AF (auxiliary carry) is BCD-only, not implemented by the core, and never gates real
        // control flow — exclude it. Compare the branch-relevant flags: CF, PF, ZF, SF, OF.
        const ALL: u64 = CF | PF | ZF | SF | OF;
        let _ = AF;
        let msb = 0x8000_0000u32;
        let par = |r: u32| (r as u8).count_ones() % 2 == 0;
        // Assemble a flags word from the defined bits + the base reserved bit.
        let mk = |r: u32, cf: bool, af: bool, of: bool| -> u64 {
            u64::from(cf) | (u64::from(par(r)) << 2) | (u64::from(af) << 4)
                | (u64::from(r == 0) << 6) | (u64::from(r >> 31) << 7) | (u64::from(of) << 11)
        };

        // Execute one reg-reg instruction in the emulator: rax=a, rcx=b, rflags=0x2 → (rax, rflags).
        fn emu(bytes: &[u8], a: u64, b: u64) -> (u64, u64) {
            let mut cpu = Cpu::new(0x10000);
            cpu.ram[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
            cpu.rip = 0x1000;
            cpu.rflags = 0x2;
            cpu.r[RAX] = a;
            cpu.r[RCX] = b;
            let _ = cpu.step();
            (cpu.r[RAX] & 0xffff_ffff, cpu.rflags)
        }

        let vals: [u32; 16] = [
            0, 1, 2, 7, 8, 0x0f, 0x10, 0x55, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0xdead_beef,
            0x0cfd_63ac, 0x1234_5678, 0x8000_0001, 0xaaaa_aaaa,
        ];
        let mut fails: Vec<String> = Vec::new();
        let mut check = |name: &str, a: u32, b: u32, bytes: &[u8], rref: u32, fref: u64, defined: u64, cmp_res: bool| {
            let (er, ef) = emu(bytes, u64::from(a), u64::from(b));
            let res_bad = cmp_res && er != u64::from(rref);
            let flag_bad = (ef ^ fref) & defined != 0;
            if res_bad || flag_bad {
                fails.push(format!(
                    "{name} a={a:#010x} b={b:#010x}: emu=({er:#010x},f {:#05x}) ref=({rref:#010x},f {:#05x}) def={defined:#05x}{}{}",
                    ef & defined, fref & defined,
                    if res_bad { " RESULT" } else { "" }, if flag_bad { " FLAGS" } else { "" },
                ));
            }
        };

        for &a in &vals {
            for &b in &vals {
                // add / sub / cmp
                let r = a.wrapping_add(b);
                let cf = u64::from(a) + u64::from(b) > 0xffff_ffff;
                let af = (a & 0xf) + (b & 0xf) > 0xf;
                let of = (a ^ r) & (b ^ r) & msb != 0;
                check("add", a, b, &[0x01, 0xc8], r, mk(r, cf, af, of), ALL, true);
                let r = a.wrapping_sub(b);
                let cf = a < b;
                let af = (a & 0xf) < (b & 0xf);
                let of = (a ^ b) & (a ^ r) & msb != 0;
                check("sub", a, b, &[0x29, 0xc8], r, mk(r, cf, af, of), ALL, true);
                check("cmp", a, b, &[0x39, 0xc8], r, mk(r, cf, af, of), ALL, false);
                // logic: CF=OF=0, AF undefined
                let r = a & b;
                check("and", a, b, &[0x21, 0xc8], r, mk(r, false, false, false), ALL & !AF, true);
                check("test", a, b, &[0x85, 0xc8], r, mk(r, false, false, false), ALL & !AF, false);
                let r = a | b;
                check("or", a, b, &[0x09, 0xc8], r, mk(r, false, false, false), ALL & !AF, true);
                let r = a ^ b;
                check("xor", a, b, &[0x31, 0xc8], r, mk(r, false, false, false), ALL & !AF, true);
                // imul r32,r32: CF=OF set if signed product overflows 32 bits; SF/ZF/PF/AF undefined
                let full = i64::from(a as i32) * i64::from(b as i32);
                let r = full as u32;
                let ov = full != i64::from(r as i32);
                check("imul", a, b, &[0x0f, 0xaf, 0xc1], r, mk(r, ov, false, ov), CF | OF, true);
                // bsr/bsf scan ecx(=b); ZF=(b==0), result valid only if b!=0
                let rr = if b == 0 { 0 } else { 31 - b.leading_zeros() };
                check("bsr", a, b, &[0x0f, 0xbd, 0xc1], rr, u64::from(b == 0) << 6, ZF, b != 0);
                let rr = if b == 0 { 0 } else { b.trailing_zeros() };
                check("bsf", a, b, &[0x0f, 0xbc, 0xc1], rr, u64::from(b == 0) << 6, ZF, b != 0);
                // bt eax,ecx: CF = bit (b%32) of a
                let cf = (a >> (b & 31)) & 1 == 1;
                check("bt", a, b, &[0x0f, 0xa3, 0xc8], a, u64::from(cf), CF, false);
                // shifts by cl
                let c = b & 0x1f;
                let shl = if c == 0 { a } else { a.wrapping_shl(c) };
                let shl_cf = c != 0 && (a >> (32 - c)) & 1 == 1;
                let shl_of = ((shl >> 31) & 1 == 1) ^ shl_cf;
                let shr = a >> c;
                let shr_cf = c != 0 && (a >> (c - 1).min(31)) & 1 == 1;
                let shr_of = (a >> 31) & 1 == 1;
                let sar = ((a as i32) >> c) as u32;
                let sar_cf = c != 0 && (a >> (c - 1).min(31)) & 1 == 1;
                let sh_def = if c == 0 { 0 } else if c == 1 { ALL & !AF } else { ZF | SF | PF };
                check("shl", a, b, &[0xd3, 0xe0], shl, mk(shl, shl_cf, false, shl_of), sh_def, true);
                check("shr", a, b, &[0xd3, 0xe8], shr, mk(shr, shr_cf, false, shr_of), sh_def, true);
                check("sar", a, b, &[0xd3, 0xf8], sar, mk(sar, sar_cf, false, false), sh_def, true);
                // rotates by cl: only CF (c!=0) and OF (c==1) defined; SF/ZF/PF/AF unchanged → not compared
                let rol = a.rotate_left(c);
                let rol_cf = rol & 1 == 1;
                let rol_of = ((rol >> 31) & 1 == 1) ^ rol_cf;
                let ror = a.rotate_right(c);
                let ror_cf = (ror >> 31) & 1 == 1;
                let ror_of = ((ror >> 31) & 1) ^ ((ror >> 30) & 1) == 1;
                let rot_def = if c == 0 { 0 } else if c == 1 { CF | OF } else { CF };
                check("rol", a, b, &[0xd3, 0xc0], rol, mk(rol, rol_cf, false, rol_of), rot_def, true);
                check("ror", a, b, &[0xd3, 0xc8], ror, mk(ror, ror_cf, false, ror_of), rot_def, true);
            }
            // unary (CF unchanged for inc/dec)
            let r = a.wrapping_add(1);
            check("inc", a, 0, &[0xff, 0xc0], r, mk(r, false, (a & 0xf) == 0xf, a == 0x7fff_ffff), ALL & !CF, true);
            let r = a.wrapping_sub(1);
            check("dec", a, 0, &[0xff, 0xc8], r, mk(r, false, (a & 0xf) == 0, a == 0x8000_0000), ALL & !CF, true);
            let r = 0u32.wrapping_sub(a);
            check("neg", a, 0, &[0xf7, 0xd8], r, mk(r, a != 0, (a & 0xf) != 0, a == 0x8000_0000), ALL, true);
        }

        // ADC/SBB (carry-in dependent), LEA (the hash spread uses `key*5`,`key*11` via LEA),
        // MOVZX/MOVSX (string hashing reads bytes via these). `check`'s borrow of `fails` has ended.
        let emu_f = |bytes: &[u8], a: u64, b: u64, fin: u64| -> (u64, u64) {
            let mut cpu = Cpu::new(0x10000);
            cpu.ram[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
            cpu.rip = 0x1000;
            cpu.rflags = fin;
            cpu.r[RAX] = a;
            cpu.r[RCX] = b;
            let _ = cpu.step();
            (cpu.r[RAX] & 0xffff_ffff, cpu.rflags)
        };
        for &a in &vals {
            for &b in &vals {
                for ci in 0u64..2 {
                    let fin = 0x2 | ci;
                    // adc eax,ecx = 11 c8
                    let r = a.wrapping_add(b).wrapping_add(ci as u32);
                    let cf = u64::from(a) + u64::from(b) + ci > 0xffff_ffff;
                    let of = (a ^ r) & (b ^ r) & msb != 0;
                    let (er, ef) = emu_f(&[0x11, 0xc8], u64::from(a), u64::from(b), fin);
                    if er != u64::from(r) || (ef ^ mk(r, cf, false, of)) & ALL != 0 {
                        fails.push(format!("adc ci={ci} a={a:#010x} b={b:#010x}: emu=({er:#010x},f {:#05x}) ref=({r:#010x},f {:#05x})", ef & ALL, mk(r, cf, false, of) & ALL));
                    }
                    // sbb eax,ecx = 19 c8
                    let r = a.wrapping_sub(b).wrapping_sub(ci as u32);
                    let cf = u64::from(a) < u64::from(b) + ci;
                    let of = (a ^ b) & (a ^ r) & msb != 0;
                    let (er, ef) = emu_f(&[0x19, 0xc8], u64::from(a), u64::from(b), fin);
                    if er != u64::from(r) || (ef ^ mk(r, cf, false, of)) & ALL != 0 {
                        fails.push(format!("sbb ci={ci} a={a:#010x} b={b:#010x}: emu=({er:#010x},f {:#05x}) ref=({r:#010x},f {:#05x})", ef & ALL, mk(r, cf, false, of) & ALL));
                    }
                }
                let chk = |name: &str, bytes: &[u8], want: u32| {
                    let (er, _) = emu(bytes, u64::from(a), u64::from(b));
                    if er != u64::from(want) { Some(format!("{name} a={a:#010x} b={b:#010x}: emu={er:#010x} ref={want:#010x}")) } else { None }
                };
                if let Some(m) = chk("lea*5", &[0x8d, 0x04, 0x89], b.wrapping_mul(5)) { fails.push(m); }
                if let Some(m) = chk("movzx8", &[0x0f, 0xb6, 0xc1], b & 0xff) { fails.push(m); }
                if let Some(m) = chk("movzx16", &[0x0f, 0xb7, 0xc1], b & 0xffff) { fails.push(m); }
                if let Some(m) = chk("movsx8", &[0x0f, 0xbe, 0xc1], b as u8 as i8 as i32 as u32) { fails.push(m); }
                if let Some(m) = chk("movsx16", &[0x0f, 0xbf, 0xc1], b as u16 as i16 as i32 as u32) { fails.push(m); }
            }
        }

        // Flag PRESERVATION: these instructions must not alter any arithmetic flag. A spurious
        // clobber silently breaks `cmp …; <op>; jcc` sequences (a prime mis-branch / skipped-resize
        // source the value-only checks above can't see).
        for (name, bytes) in [
            ("mov", &[0x89u8, 0xc8][..]),
            ("lea", &[0x8d, 0x04, 0x89]),
            ("movzx8", &[0x0f, 0xb6, 0xc1]),
            ("movzx16", &[0x0f, 0xb7, 0xc1]),
            ("movsx8", &[0x0f, 0xbe, 0xc1]),
            ("movsx16", &[0x0f, 0xbf, 0xc1]),
            ("movsxd", &[0x48, 0x63, 0xc1]),
            ("push", &[0x50]),
            ("nop", &[0x90]),
        ] {
            let (_, ef) = emu_f(bytes, 0x1234_5678, 0x9abc_def0, 0x2 | ALL);
            if ef & ALL != ALL {
                fails.push(format!("{name} clobbered flags: got {:#05x} want {:#05x}", ef & ALL, ALL));
            }
        }

        // SETcc across all 16 conditions and all 32 (CF,PF,ZF,SF,OF) flag combinations — this is the
        // condition-code evaluator that EVERY conditional branch (Jcc), CMOVcc and SETcc share. A
        // wrong condition mis-branches even when the flags are correct (e.g. a skipped hash resize).
        for combo in 0u32..32 {
            let cf = combo & 1 != 0;
            let pf = combo & 2 != 0;
            let zf = combo & 4 != 0;
            let sf = combo & 8 != 0;
            let of = combo & 16 != 0;
            let rflags = 0x2 | u64::from(cf) | (u64::from(pf) << 2) | (u64::from(zf) << 6)
                | (u64::from(sf) << 7) | (u64::from(of) << 11);
            for cc in 0u8..16 {
                let want = match cc {
                    0x0 => of, 0x1 => !of, 0x2 => cf, 0x3 => !cf, 0x4 => zf, 0x5 => !zf,
                    0x6 => cf || zf, 0x7 => !(cf || zf), 0x8 => sf, 0x9 => !sf, 0xa => pf, 0xb => !pf,
                    0xc => sf != of, 0xd => sf == of, 0xe => zf || (sf != of), _ => !(zf || (sf != of)),
                };
                let mut cpu = Cpu::new(0x10000);
                cpu.ram[0x1000..0x1003].copy_from_slice(&[0x0f, 0x90 + cc, 0xc0]); // SETcc al
                cpu.rip = 0x1000;
                cpu.rflags = rflags;
                let _ = cpu.step();
                let got = cpu.r[RAX] & 1 == 1;
                if got != want {
                    fails.push(format!("setcc cc={cc:#x} flags={rflags:#05x}: emu={got} ref={want}"));
                }
            }
        }
        assert!(fails.is_empty(), "{} reference-vs-emulator mismatches:\n{}", fails.len(), fails.join("\n"));
    }

    /// DIFFERENTIAL (rep string ops): `rep stos`/`rep movs` must fill/copy EXACTLY `rcx` elements,
    /// leave `rcx`=0, and advance `rdi`/`rsi` — including counts that cross pages (the slow,
    /// interruptible per-element path). glib's GHashTable resize zeroes the new bucket array via a
    /// `rep stos`/memset; a short or stale fill leaves non-zero "empty" slots ⇒ the table reads full
    /// ⇒ the over-full spin even after a resize. This validates the interruptible-rep change.
    #[test]
    fn rep_string_ops_match_reference() {
        // Run a single (possibly REP-prefixed) instruction to completion: the interruptible REP does
        // one element per `step` and rewinds rip, so loop until rip advances past it.
        fn run_to_done(bytes: &[u8], setup: impl FnOnce(&mut Cpu)) -> Cpu {
            let mut cpu = Cpu::new(0x20000);
            cpu.ram[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
            cpu.rip = 0x100;
            cpu.rflags = 0x2; // DF=0
            setup(&mut cpu);
            for _ in 0..2_000_000 {
                if cpu.rip != 0x100 { break; }
                let _ = cpu.step();
            }
            cpu
        }
        let mut fails: Vec<String> = Vec::new();
        for &n in &[0u64, 1, 5, 255, 256, 4096, 5000] {
            for &dst in &[0x8000u64, 0x8001, 0x8ffe, 0x9ffd] { // incl. page-crossing dst (slow path)
                // rep stosb: f3 aa  (store AL to [RDI], RCX times)
                let cpu = run_to_done(&[0xf3, 0xaa], |c| { c.r[7] = dst; c.r[RCX] = n; c.r[RAX] = 0xcd; });
                let bad = (0..n as usize).any(|i| cpu.ram[dst as usize + i] != 0xcd);
                if bad || cpu.r[RCX] != 0 || cpu.r[7] != dst + n {
                    fails.push(format!("rep stosb n={n} dst={dst:#x}: fill_bad={bad} rcx={:#x} rdi={:#x} (want rcx=0 rdi={:#x})", cpu.r[RCX], cpu.r[7], dst + n));
                }
                // guard byte just past the fill must be untouched (0)
                if cpu.ram[dst as usize + n as usize] != 0 {
                    fails.push(format!("rep stosb n={n} dst={dst:#x}: overran into [{:#x}]={:#04x}", dst + n, cpu.ram[dst as usize + n as usize]));
                }
                // rep stosq: f3 48 ab  (store RAX 8 bytes, RCX times)
                let cpu = run_to_done(&[0xf3, 0x48, 0xab], |c| { c.r[7] = dst; c.r[RCX] = n; c.r[RAX] = 0x1122_3344_5566_7788; });
                let qbad = (0..n as usize).any(|i| {
                    let o = dst as usize + i * 8;
                    cpu.ram[o..o + 8] != 0x1122_3344_5566_7788u64.to_le_bytes()
                });
                if qbad || cpu.r[RCX] != 0 || cpu.r[7] != dst + n * 8 {
                    fails.push(format!("rep stosq n={n} dst={dst:#x}: fill_bad={qbad} rcx={:#x} rdi={:#x}", cpu.r[RCX], cpu.r[7]));
                }
            }
        }
        // rep movsb: f3 a4  (copy [RSI]→[RDI], RCX times). Non-overlapping src/dst.
        for &n in &[0u64, 1, 5, 256, 5000] {
            let (src, dst) = (0x4000u64, 0xc000u64);
            let cpu = run_to_done(&[0xf3, 0xa4], |c| {
                for i in 0..n as usize { c.ram[src as usize + i] = (i as u8).wrapping_mul(7).wrapping_add(1); }
                c.r[6] = src; c.r[7] = dst; c.r[RCX] = n;
            });
            let bad = (0..n as usize).any(|i| cpu.ram[dst as usize + i] != (i as u8).wrapping_mul(7).wrapping_add(1));
            if bad || cpu.r[RCX] != 0 || cpu.r[7] != dst + n || cpu.r[6] != src + n {
                fails.push(format!("rep movsb n={n}: copy_bad={bad} rcx={:#x} rdi={:#x} rsi={:#x}", cpu.r[RCX], cpu.r[7], cpu.r[6]));
            }
        }
        assert!(fails.is_empty(), "{} rep mismatches:\n{}", fails.len(), fails.join("\n"));
    }

    /// DIFFERENTIAL (TLB coherency): after a PTE is changed, the new mapping must take effect on
    /// `INVLPG` (that page) and on a CR3 reload (full flush). A stale software-TLB entry would alias
    /// a store/load to the OLD physical page — silent memory corruption in the live kernel (COW,
    /// mmap, demand paging), a strong candidate for the over-full-hash spin. VA 0x400000 starts
    /// mapped to phys 0x40000 (value 0x11); phys 0x50000 holds 0x22.
    #[test]
    fn tlb_invalidation_is_coherent() {
        fn pte(ram: &mut [u8], table: usize, idx: usize, e: u64) {
            ram[table + idx * 8..table + idx * 8 + 8].copy_from_slice(&e.to_le_bytes());
        }
        let mut cpu = Cpu::new(0x100000);
        pte(&mut cpu.ram, 0x10000, 0, 0x11000 | 0x3);
        pte(&mut cpu.ram, 0x11000, 0, 0x12000 | 0x3);
        pte(&mut cpu.ram, 0x12000, 2, 0x13000 | 0x3);
        pte(&mut cpu.ram, 0x13000, 0, 0x40000 | 0x3); // VA 0x400000 → phys 0x40000
        pte(&mut cpu.ram, 0x13000, 2, 0x14000 | 0x3); // VA 0x402000 → phys 0x14000 (code)
        cpu.ram[0x40000] = 0x11;
        cpu.ram[0x50000] = 0x22;
        // code @ VA 0x402000: load al; load ah; invlpg[rbx]; load cl; load dl   (rip steps through)
        //   8a 03         mov al,[rbx]
        //   8a 23         mov ah,[rbx]
        //   0f 01 3b      invlpg [rbx]
        //   8a 0b         mov cl,[rbx]
        //   8a 13         mov dl,[rbx]
        cpu.ram[0x14000..0x14000 + 11].copy_from_slice(&[0x8a, 0x03, 0x8a, 0x23, 0x0f, 0x01, 0x3b, 0x8a, 0x0b, 0x8a, 0x13]);
        cpu.cr3 = 0x10000;
        cpu.cr4 = 1 << 5;
        cpu.efer = (1 << 8) | (1 << 10);
        cpu.cr0 = (1 << 0) | (1 << 31);
        cpu.seg[SegId::Cs as usize] = Seg { selector: 0x10, base: 0, long: true };
        cpu.rip = 0x402000;
        cpu.rflags = 0x2;
        cpu.r[3] = 0x400000; // rbx

        let mut fails: Vec<String> = Vec::new();
        let _ = cpu.step(); // mov al,[rbx] → caches the TLB entry; al should be 0x11
        if cpu.r[RAX] & 0xff != 0x11 { fails.push(format!("initial load al={:#04x} want 0x11", cpu.r[RAX] & 0xff)); }
        // Kernel remaps VA 0x400000 → phys 0x50000 (a PTE write) but does NOT invalidate yet.
        pte(&mut cpu.ram, 0x13000, 0, 0x50000 | 0x3);
        let _ = cpu.step(); // mov ah,[rbx] — still the OLD mapping (stale TLB is architecturally allowed until INVLPG)
        let _ = cpu.step(); // invlpg [rbx] — flush this page
        let _ = cpu.step(); // mov cl,[rbx] — MUST now see the new mapping → 0x22
        if cpu.r[RCX] & 0xff != 0x22 {
            fails.push(format!("after INVLPG cl={:#04x} want 0x22 (TLB not invalidated)", cpu.r[RCX] & 0xff));
        }
        // Remap back to 0x40000 and reload CR3 (full flush) — next access must see 0x11.
        pte(&mut cpu.ram, 0x13000, 0, 0x40000 | 0x3);
        cpu.cr3 = cpu.cr3; // not a flush by itself; emulate the kernel's `mov cr3` via the real path:
        // execute `mov rax,cr3; mov cr3,rax` would flush; simplest: call the same routine the core uses.
        cpu.rip = 0x402000 + 9; // point at the last `mov dl,[rbx]`
        // Force a full TLB flush the way a CR3 write does, then read.
        cpu.flush_tlb();
        let _ = cpu.step(); // mov dl,[rbx] → 0x11
        if cpu.r[RDX] & 0xff != 0x11 {
            fails.push(format!("after CR3-flush dl={:#04x} want 0x11", cpu.r[RDX] & 0xff));
        }
        assert!(fails.is_empty(), "TLB coherency:\n{}", fails.join("\n"));
    }

    /// DIFFERENTIAL (paged, cross-page): a multi-byte store/load that crosses a 4 KiB page boundary
    /// into a NON-CONTIGUOUS physical page must split correctly per the page tables. This is the one
    /// memory mechanism the non-paged test can't see, and a split bug would corrupt a hash slot
    /// (stale bytes ⇒ never "empty" ⇒ the over-full GHashTable spin). Maps VA 0x400000→phys 0x40000
    /// and VA 0x401000→phys 0x50000 (non-adjacent), code at VA 0x402000→phys 0x14000.
    #[test]
    fn paged_cross_page_memory_is_correct() {
        fn pte(ram: &mut [u8], table: usize, idx: usize, e: u64) {
            ram[table + idx * 8..table + idx * 8 + 8].copy_from_slice(&e.to_le_bytes());
        }
        fn paged(code: &[u8], setup: impl FnOnce(&mut Cpu)) -> Cpu {
            let mut cpu = Cpu::new(0x100000); // 1 MiB phys
            pte(&mut cpu.ram, 0x10000, 0, 0x11000 | 0x3); // PML4[0] → PDPT
            pte(&mut cpu.ram, 0x11000, 0, 0x12000 | 0x3); // PDPT[0] → PD
            pte(&mut cpu.ram, 0x12000, 2, 0x13000 | 0x3); // PD[2] → PT  (covers 0x400000..)
            pte(&mut cpu.ram, 0x13000, 0, 0x40000 | 0x3); // VA 0x400000 → phys 0x40000
            pte(&mut cpu.ram, 0x13000, 1, 0x50000 | 0x3); // VA 0x401000 → phys 0x50000 (non-adjacent!)
            pte(&mut cpu.ram, 0x13000, 2, 0x14000 | 0x3); // VA 0x402000 → phys 0x14000 (code)
            cpu.ram[0x14000..0x14000 + code.len()].copy_from_slice(code);
            cpu.cr3 = 0x10000;
            cpu.cr4 = 1 << 5; // PAE
            cpu.efer = (1 << 8) | (1 << 10); // LME|LMA
            cpu.cr0 = (1 << 0) | (1 << 31); // PE|PG
            cpu.seg[SegId::Cs as usize] = Seg { selector: 0x10, base: 0, long: true };
            cpu.rip = 0x402000;
            cpu.rflags = 0x2;
            setup(&mut cpu);
            let _ = cpu.step();
            cpu
        }
        let mut fails: Vec<String> = Vec::new();
        let val: u64 = 0x1122_3344_5566_7788;
        // STORE rax (8 bytes) at VA 0x400ffe → 2 bytes in phys 0x40ffe, 6 bytes in phys 0x50000.
        let cpu = paged(&[0x48, 0x89, 0x03], |c| {
            for b in &mut c.ram[0x40000..0x51000] { *b = 0xAA; }
            c.r[3] = 0x400ffe;
            c.r[RAX] = val;
        });
        let lo = &cpu.ram[0x40ffe..0x41000]; // first page tail
        let hi = &cpu.ram[0x50000..0x50006]; // second page head
        let wb = val.to_le_bytes();
        if lo != &wb[0..2] {
            fails.push(format!("cross-store low(phys 0x40ffe)={lo:02x?} want {:02x?}", &wb[0..2]));
        }
        if hi != &wb[2..8] {
            fails.push(format!("cross-store high(phys 0x50000)={hi:02x?} want {:02x?}", &wb[2..8]));
        }
        // The byte just past the first page in PHYS (0x41000) must be UNTOUCHED (it's a different VA).
        if cpu.ram[0x41000] != 0xAA {
            fails.push(format!("cross-store bled into contiguous phys 0x41000={:#04x} (split bug)", cpu.ram[0x41000]));
        }
        // LOAD rax (8 bytes) at VA 0x400ffe ← from the two non-adjacent phys pages.
        let cpu = paged(&[0x48, 0x8b, 0x03], |c| {
            c.ram[0x40ffe] = 0xde; c.ram[0x40fff] = 0xc0;
            c.ram[0x50000..0x50006].copy_from_slice(&[0xad, 0x0b, 0xfe, 0xca, 0xef, 0xbe]);
            c.r[3] = 0x400ffe;
        });
        let want = u64::from_le_bytes([0xde, 0xc0, 0xad, 0x0b, 0xfe, 0xca, 0xef, 0xbe]);
        if cpu.r[RAX] != want {
            fails.push(format!("cross-load rax={:#018x} want {want:#018x}", cpu.r[RAX]));
        }
        assert!(fails.is_empty(), "{} cross-page mismatches:\n{}", fails.len(), fails.join("\n"));
    }

    /// DIFFERENTIAL (memory): stores must write EXACTLY their byte-width and leave neighbours
    /// untouched; loads must zero/sign-extend correctly — across aligned, unaligned, and
    /// page-boundary-crossing addresses. A store that leaves stale bytes is the prime suspect for a
    /// hash slot that never reads as empty (the over-full GHashTable spin). Paging off (VA=PA).
    #[test]
    fn memory_store_load_match_reference() {
        fn run(setup: impl FnOnce(&mut Cpu), bytes: &[u8]) -> Cpu {
            let mut cpu = Cpu::new(0x10000);
            cpu.ram[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
            cpu.rip = 0x1000;
            cpu.rflags = 0x2;
            setup(&mut cpu);
            let _ = cpu.step();
            cpu
        }
        let mut fails: Vec<String> = Vec::new();
        let val: u64 = 0x1122_3344_5566_7788;
        // (name, bytes [dst=[rbx], src=a-reg], width)
        let stores: &[(&str, &[u8], usize)] = &[
            ("mov[rbx],al", &[0x88, 0x03], 1),
            ("mov[rbx],ax", &[0x66, 0x89, 0x03], 2),
            ("mov[rbx],eax", &[0x89, 0x03], 4),
            ("mov[rbx],rax", &[0x48, 0x89, 0x03], 8),
        ];
        // Addresses incl. unaligned + crossing the 0x3000 page boundary.
        for &addr in &[0x3000u64, 0x3001, 0x3003, 0x3007, 0x2ffd, 0x2ffe, 0x2fff] {
            for &(name, bytes, n) in stores {
                let cpu = run(|c| {
                    for b in &mut c.ram[0x2fe0..0x3030] { *b = 0xAA; }
                    c.r[3] = addr; // rbx
                    c.r[RAX] = val;
                }, bytes);
                let want = &val.to_le_bytes()[..n];
                let got = &cpu.ram[addr as usize..addr as usize + n];
                if got != want {
                    fails.push(format!("{name}@{addr:#x}: wrote {got:02x?} want {want:02x?}"));
                }
                if cpu.ram[addr as usize - 1] != 0xAA || cpu.ram[addr as usize + n] != 0xAA {
                    fails.push(format!("{name}@{addr:#x}: clobbered neighbour prev={:#04x} next={:#04x}",
                        cpu.ram[addr as usize - 1], cpu.ram[addr as usize + n]));
                }
            }
        }
        // Loads: prefill a known high-bit pattern, verify zero/sign extension.
        let pat: [u8; 8] = [0x90, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07];
        let loads: &[(&str, &[u8], u64)] = &[
            ("movzx8", &[0x0f, 0xb6, 0x03], u64::from(pat[0])),
            ("movsx8", &[0x0f, 0xbe, 0x03], u64::from(pat[0] as i8 as i32 as u32)),
            ("movzx16", &[0x0f, 0xb7, 0x03], u64::from(u16::from_le_bytes([pat[0], pat[1]]))),
            ("movsx16", &[0x0f, 0xbf, 0x03], u64::from(i16::from_le_bytes([pat[0], pat[1]]) as i32 as u32)),
            ("mov32", &[0x8b, 0x03], u64::from(u32::from_le_bytes([pat[0], pat[1], pat[2], pat[3]]))),
            ("mov64", &[0x48, 0x8b, 0x03], u64::from_le_bytes(pat)),
        ];
        for &addr in &[0x3000u64, 0x3001, 0x3003, 0x2ffe, 0x2fff] {
            for &(name, bytes, want) in loads {
                let cpu = run(|c| {
                    c.ram[addr as usize..addr as usize + 8].copy_from_slice(&pat);
                    c.r[3] = addr;
                    c.r[RAX] = 0xdead_beef_dead_beef;
                }, bytes);
                // 32-bit dests zero the upper 32; 64-bit fills all.
                let mask = if name == "mov64" { u64::MAX } else { 0xffff_ffff };
                if cpu.r[RAX] & mask != want {
                    fails.push(format!("{name}@{addr:#x}: loaded {:#018x} want {want:#018x}", cpu.r[RAX] & mask));
                }
            }
        }
        assert!(fails.is_empty(), "{} memory mismatches:\n{}", fails.len(), fails.join("\n"));
    }

    /// κ-snapshot Step-4 Phase 4 (streaming): an adopter resumes from a manifest by fetching each
    /// page ONE-BY-κ through a transport closure, verifying each on receipt (L5) and deduping —
    /// the exact seam a browser tab uses over `content_net`. A forging transport (attacker bytes
    /// for every κ) and a missing page are both refused.
    #[test]
    fn kappa_streaming_resume_fetches_by_kappa_and_refuses_forgery() {
        use hologram_store_mem::MemKappaStore;
        let mut a = Cpu::new(16 * KAPPA_PAGE);
        a.r[1] = 0xcafe;
        a.rip = 0x6000;
        a.sys.as_mut().unwrap().tsc = 7;
        a.ram[0..8].copy_from_slice(&0x1111_2222_3333_4444u64.to_le_bytes());
        a.ram[5 * KAPPA_PAGE..5 * KAPPA_PAGE + 4].copy_from_slice(&[9, 8, 7, 6]);

        // Publisher: a manifest (small) + a store holding only the UNIQUE pages.
        let store = MemKappaStore::new();
        let snap = a.snapshot_kappa(&store).unwrap();
        let manifest = a.snapshot_kappa_manifest();
        assert_eq!(manifest, snap.to_manifest_bytes(), "manifest matches the seal");

        // Adopter: stream each page by κ from the store (verify-on-receipt), counting fetches.
        let mut fetched = 0usize;
        let mut b = Cpu::new(KAPPA_PAGE);
        let ok = b.restore_kappa_streaming(&manifest, |k| {
            fetched += 1;
            store.get(k).ok().flatten().map(|x| x.to_vec())
        });
        assert!(ok, "streaming resume reconstructs");
        assert_eq!(b.ram, a.ram, "RAM bit-exact via streamed pages");
        assert_eq!(b.snapshot(), a.snapshot(), "full machine identical after a streamed resume");
        assert!(
            fetched < a.ram.len() / KAPPA_PAGE,
            "deduped over the wire: {fetched} unique fetches < 16 pages"
        );

        // A FORGING transport (attacker bytes for every κ) is refused on receipt (L5).
        let mut c = Cpu::new(KAPPA_PAGE);
        assert!(
            !c.restore_kappa_streaming(&manifest, |_k| Some(alloc::vec![0xaau8; KAPPA_PAGE])),
            "forged pages are refused (verify-on-receipt)"
        );
        // A missing page is refused (the transport can't serve it).
        let mut d = Cpu::new(KAPPA_PAGE);
        assert!(
            !d.restore_kappa_streaming(&manifest, |_k| None),
            "a missing page is refused"
        );
    }

    #[test]
    fn adds_two_registers() {
        // mov rax,5 ; mov rbx,7 ; add rax,rbx ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x05, 0, 0, 0, // mov rax, 5
            0x48, 0xc7, 0xc3, 0x07, 0, 0, 0, // mov rbx, 7
            0x48, 0x01, 0xd8, // add rax, rbx
            0xf4, // hlt
        ];
        assert_eq!(run(&code).reg(0), 12);
    }

    /// The exact instructions busybox `printf`'s escape conversion (`bb_process_escape_sequence`)
    /// executes for `printf 'x\n'` (extracted via a qemu-user differential). In 64-bit mode a
    /// 32-bit destination operand MUST zero the upper 32 bits of the register; a core that leaves
    /// them dirty silently corrupts the conversion → the shell exits → init dies (the crash this
    /// repro chases). Each case asserts the SDM-correct zero-extended result.
    #[test]
    fn printf_escape_path_instructions_zero_extend_to_64() {
        // lea esi, [rdx-0x57]  (8d 72 a9) — 32-bit LEA: low32 of (rdx-0x57), upper cleared.
        let c = [
            0x48, 0xba, 0x60, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, // movabs rdx, 0xffffffff_00000060
            0x8d, 0x72, 0xa9, // lea esi, [rdx-0x57]
            0xf4,
        ];
        assert_eq!(run(&c).reg(6), 0x09, "32-bit LEA must zero-extend rsi");

        // movzx ecx, al  (0f b6 c8) — zero-extend a byte into a 32-bit dest → upper 56 bits clear.
        let c = [
            0x48, 0xb8, 0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, // movabs rax, 0xffffffff_00000080
            0x0f, 0xb6, 0xc8, // movzx ecx, al
            0xf4,
        ];
        assert_eq!(run(&c).reg(1), 0x80, "MOVZX must zero-extend the byte into a clean rcx");

        // sub esi, 0x30  (83 ee 30) — '0'-subtract; 32-bit result zero-extends.
        let c = [
            0x48, 0xbe, 0x35, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, // movabs rsi, 0xffffffff_00000035
            0x83, 0xee, 0x30, // sub esi, 0x30
            0xf4,
        ];
        assert_eq!(run(&c).reg(6), 0x05, "SUB r32,imm8 must zero-extend rsi");

        // or edx, 0x20  (83 ca 20) — lowercase bit; 32-bit result zero-extends.
        let c = [
            0x48, 0xba, 0x41, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, // movabs rdx, 0xffffffff_00000041
            0x83, 0xca, 0x20, // or edx, 0x20
            0xf4,
        ];
        assert_eq!(run(&c).reg(2), 0x61, "OR r32,imm8 must zero-extend rdx");

        // mov r8d, edx  (44 89 c2 is mov edx,r8d; use 41 89 d0 = mov r8d, edx) — 32-bit reg move clears upper.
        let c = [
            0x49, 0xb8, 0x99, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, // movabs r8, 0xffffffff_00000099
            0x44, 0x89, 0xc2, // mov edx, r8d  → edx=0x99, rdx upper clear
            0xf4,
        ];
        assert_eq!(run(&c).reg(2), 0x99, "32-bit reg→reg mov must zero-extend rdx");

        // dec qword [0x400]  (48 ff 0c 25 ..) — memory RMW; result correct AND CF preserved
        // (INC/DEC must NOT touch CF, unlike SUB). The escape loop decrements a counter.
        let c = [
            0xf9, // stc → CF=1
            0x48, 0xc7, 0xc0, 0x05, 0, 0, 0, // mov rax, 5
            0x48, 0x89, 0x04, 0x25, 0x00, 0x04, 0, 0, // mov [0x400], rax
            0x48, 0xff, 0x0c, 0x25, 0x00, 0x04, 0, 0, // dec qword [0x400]
            0x48, 0x8b, 0x1c, 0x25, 0x00, 0x04, 0, 0, // mov rbx, [0x400]
            0xf4,
        ];
        let cpu = run(&c);
        assert_eq!(cpu.reg(3), 4, "DEC qword[mem] decremented in memory");
        assert!(cpu.rflags() & flag::CF != 0, "DEC must NOT clear CF (only SUB does)");

        // cmp dl, 0x60  (80 fa 60) with dl='n'(0x6e): 0x6e-0x60=0x0e → CF=0, ZF=0, SF=0.
        let c = [
            0x48, 0xc7, 0xc2, 0x6e, 0, 0, 0, // mov rdx, 0x6e ('n')
            0x80, 0xfa, 0x60, // cmp dl, 0x60
            0xf4,
        ];
        let f = run(&c).rflags();
        assert_eq!(f & flag::CF, 0, "cmp 0x6e,0x60: no borrow → CF=0");
        assert_eq!(f & flag::ZF, 0, "cmp 0x6e,0x60: not equal → ZF=0");

        // cmp dl, 0x60 with dl=0x40 ('@'): 0x40-0x60 borrows → CF=1.
        let c = [
            0x48, 0xc7, 0xc2, 0x40, 0, 0, 0, // mov rdx, 0x40
            0x80, 0xfa, 0x60, // cmp dl, 0x60
            0xf4,
        ];
        assert!(run(&c).rflags() & flag::CF != 0, "cmp 0x40,0x60: borrow → CF=1");
    }

    /// musl's SSE `strlen`/`strchr`/`memchr` (which printf calls to scan its format string)
    /// reduce a 16-byte `pcmpeqb` vector to a 16-bit GPR mask with **PMOVMSKB**; a wrong mask
    /// mis-locates the terminator → printf misparses → returns error 2 → init dies. Verify
    /// PMOVMSKB against the SDM: bit i of the result = the sign bit of byte i.
    #[test]
    fn pmovmskb_extracts_the_per_byte_sign_mask() {
        // xmm0 = 0x8000_8000_..._8000 (high bit set on odd byte lanes) → mask 0xAAAA.
        let c = [
            0x48, 0xb8, 0x00, 0x80, 0x00, 0x80, 0x00, 0x80, 0x00, 0x80, // movabs rax, 0x8000800080008000
            0x66, 0x48, 0x0f, 0x6e, 0xc0, // movq xmm0, rax       (low 64)
            0x66, 0x0f, 0x6c, 0xc0, // punpcklqdq xmm0, xmm0       (low → high)
            0x66, 0x0f, 0xd7, 0xc0, // pmovmskb eax, xmm0
            0xf4,
        ];
        assert_eq!(run(&c).reg(0), 0xAAAA, "PMOVMSKB: bit i = sign bit of byte i");

        // All-high (0x80 every byte) → 0xFFFF; all-low (0x7f) → 0x0000.
        let c = [
            0x48, 0xb8, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, // movabs rax, 0x8080808080808080
            0x66, 0x48, 0x0f, 0x6e, 0xc0,
            0x66, 0x0f, 0x6c, 0xc0,
            0x66, 0x0f, 0xd7, 0xc0,
            0xf4,
        ];
        assert_eq!(run(&c).reg(0), 0xFFFF, "PMOVMSKB all sign bits → 0xFFFF");
    }

    /// The SSE *load* that brings the format string into xmm (musl `strlen`/`strchr`) must read the
    /// exact 16 bytes at the address — aligned (`movdqa`) and unaligned (`movdqu`). A wrong load
    /// feeds the (correct) `pcmpeqb`/`pmovmskb` garbage → misparse. Stage 16 known bytes in RAM,
    /// `movdqu`/`movdqa` them in, extract the low 64 with `movq` and assert it equals the staged bytes.
    #[test]
    fn sse_loads_read_the_exact_bytes() {
        // Stage 01..10 at 0x200 (16-byte aligned) and at 0x207 (unaligned), then load + extract.
        // mov rax,imm64 ; mov [addr],rax ; mov rax,imm64 ; mov [addr+8],rax
        let stage = |addr: u32| -> Vec<u8> {
            let mut v = vec![
                0x48, 0xb8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // movabs rax, 0x0807060504030201
                0x48, 0x89, 0x04, 0x25,
            ];
            v.extend_from_slice(&addr.to_le_bytes()); // mov [addr], rax
            v.extend_from_slice(&[
                0x48, 0xb8, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, // movabs rax, 0x100f0e0d0c0b0a09
                0x48, 0x89, 0x04, 0x25,
            ]);
            v.extend_from_slice(&(addr + 8).to_le_bytes()); // mov [addr+8], rax
            v
        };
        // movdqu xmm0,[addr] = F3 0F 6F 04 25 <disp32> ; movq rbx,xmm0 = 66 48 0F 7E C3
        let load_extract = |addr: u32, aligned: bool| -> u64 {
            let mut c = stage(addr);
            if aligned {
                c.extend_from_slice(&[0x66, 0x0f, 0x6f, 0x04, 0x25]); // movdqa xmm0,[addr]
            } else {
                c.extend_from_slice(&[0xf3, 0x0f, 0x6f, 0x04, 0x25]); // movdqu xmm0,[addr]
            }
            c.extend_from_slice(&addr.to_le_bytes());
            c.extend_from_slice(&[0x66, 0x48, 0x0f, 0x7e, 0xc3, 0xf4]); // movq rbx,xmm0 ; hlt
            run(&c).reg(3)
        };
        assert_eq!(load_extract(0x200, true), 0x0807_0605_0403_0201, "movdqa aligned load");
        assert_eq!(load_extract(0x207, false), 0x0807_0605_0403_0201, "movdqu unaligned load");
    }

    #[test]
    fn countdown_loop_sets_the_zero_flag() {
        // mov rcx,3 ; (dec rcx ; jnz -5) ; hlt  — a real branch loop
        let code = [
            0x48, 0xc7, 0xc1, 0x03, 0, 0, 0, // mov rcx, 3
            0x48, 0xff, 0xc9, // dec rcx
            0x75, 0xfb, // jnz -5 (back to dec)
            0xf4, // hlt
        ];
        let cpu = run(&code);
        assert_eq!(cpu.reg(1), 0, "the loop ran to zero");
        assert!(cpu.rflags() & flag::ZF != 0, "ZF set at zero");
    }

    #[test]
    fn writes_to_the_serial_console() {
        // mov dx,0x3f8 ; mov al,'h' ; out dx,al ; mov al,'i' ; out dx,al ; hlt
        let code = [
            0x66, 0xba, 0xf8, 0x03, // mov dx, 0x3f8 (the 16550 THR port)
            0xb0, b'h', 0xee, // mov al,'h' ; out dx,al
            0xb0, b'i', 0xee, // mov al,'i' ; out dx,al
            0xf4, // hlt
        ];
        assert_eq!(run(&code).console(), b"hi");
    }

    #[test]
    fn long_mode_paging_translates_through_four_levels() {
        // Build a 4 KiB-page mapping VA 0 → PA 0x5000 through PML4→PDPT→PD→PT
        // (entry 0 of each table, the tables at 0x1000/0x2000/0x3000/0x4000), then
        // enable long-mode paging and check translation honours it.
        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        put(&mut cpu, 0x1000, 0x2000 | 1); // PML4[0] → PDPT, present
        put(&mut cpu, 0x2000, 0x3000 | 1); // PDPT[0] → PD
        put(&mut cpu, 0x3000, 0x4000 | 1); // PD[0]   → PT
        put(&mut cpu, 0x4000, 0x5000 | 1); // PT[0]   → frame 0x5000
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = 1 << 8; // LME
        assert_eq!(cpu.translate(0x123), 0x123, "paging off → identity");
        cpu.cr0 = 1 << 31; // PG → paging on
        assert_eq!(cpu.translate(0x123), 0x5123, "VA 0x123 maps to PA 0x5123");
        // A write through the VA lands at the physical frame (rd/wr translate).
        cpu.wr(0x40, 4, 0xdead_beef);
        assert_eq!(
            cpu.rd_phys(0x5040, 4),
            0xdead_beef,
            "the write hit frame 0x5000"
        );
    }

    #[test]
    fn mov_cr_and_wrmsr_set_up_long_mode() {
        // mov rax, 0x1000 ; mov cr3, rax ; mov rax, 0x20 ; mov cr4, rax ;
        // mov ecx,0xC0000080 ; mov eax,0x100 ; xor edx,edx ; wrmsr ; rdmsr ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x00, 0x10, 0, 0, // mov rax, 0x1000
            0x0f, 0x22, 0xd8, // mov cr3, rax
            0xb9, 0x80, 0x00, 0x00, 0xc0, // mov ecx, 0xC0000080
            0xb8, 0x00, 0x01, 0x00, 0x00, // mov eax, 0x100 (LME)
            0x31, 0xd2, // xor edx, edx
            0x0f, 0x30, // wrmsr → EFER = 0x100
            0x0f, 0x32, // rdmsr → eax = EFER low
            0xf4, // hlt
        ];
        let cpu = run(&code);
        assert_eq!(cpu.cr3, 0x1000, "mov cr3 installed the PML4 base");
        assert_eq!(cpu.efer, 0x100, "wrmsr set EFER.LME");
        assert_eq!(
            cpu.reg(0) & 0xffff_ffff,
            0x100,
            "rdmsr read EFER back into eax"
        );
    }

    /// A `REP MOVSB` whose destination crosses into a not-present page (exactly the
    /// kernel's `copy_to_user` of argv/envp onto a freshly demand-paged user stack at
    /// `execve` — the fault that blocked the x64 browser boot). The string op must
    /// fault on the boundary byte, let `step` restore the pre-instruction registers
    /// and set `CR2`, and — once the page is mapped — restart and complete the copy
    /// byte-perfect. The fault must also *stop the REP* (not spin the remaining count
    /// against the phys-0 scratch): the single faulting element writes phys 0 once.
    #[test]
    fn rep_movsb_faults_on_a_page_boundary_then_restarts_and_completes() {
        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        // 4-level tables at 0x1000/0x2000/0x3000; the PT at 0x4000 maps VA→PA identity
        // for the frames we use, EXCEPT VA 0x8000 (destination's second page) is absent.
        put(&mut cpu, 0x1000, 0x2000 | 0b11); // PML4[0] → PDPT  (present|write)
        put(&mut cpu, 0x2000, 0x3000 | 0b11); // PDPT[0] → PD
        put(&mut cpu, 0x3000, 0x4000 | 0b11); // PD[0]   → PT
        let map = |cpu: &mut Cpu, va: u64, pa: u64| {
            put(cpu, 0x4000 + ((va >> 12) as usize) * 8, pa | 0b11);
        };
        map(&mut cpu, 0x5000, 0x5000); // code
        map(&mut cpu, 0x6000, 0x6000); // source page
        map(&mut cpu, 0x7000, 0x7000); // destination page 1 (present)
        // VA 0x8000 (destination page 2) deliberately left not-present.
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = 1 << 8; // LME
        cpu.cr0 = 1 << 31; // PG

        // Eight distinct, non-zero source bytes so a stray phys-0 scratch is visible.
        let src: [u8; 8] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        cpu.ram[0x6000..0x6008].copy_from_slice(&src);
        // A sentinel at phys 0 — clobbered at most once (the faulting element) with the
        // fix; clobbered repeatedly (ending in src[7]) without it.
        cpu.ram[0] = 0xAB;

        // Code: `rep movsb` (F3 A4) then `hlt`.
        cpu.ram[0x5000..0x5003].copy_from_slice(&[0xF3, 0xA4, 0xF4]);
        cpu.rip = 0x5000;
        cpu.r[RCX] = 8;
        cpu.r[RSI] = 0x6000;
        cpu.r[RDI] = 0x7FFE; // 2 bytes fit in page 1, then crosses into absent 0x8000
        cpu.rflags &= !RFLAGS_DF; // forward

        // One step decodes + executes the `rep movsb`; it faults on the 3rd byte.
        cpu.step().expect("rep movsb step should not halt the core");

        assert_eq!(cpu.cr2, 0x8000, "CR2 = first byte in the not-present page");
        assert_eq!(cpu.rip, 0x5000, "RIP rolled back to the faulting instruction");
        assert_eq!(cpu.r[RCX], 8, "RCX restored (the REP restarts whole)");
        assert_eq!(cpu.r[RSI], 0x6000, "RSI restored");
        assert_eq!(cpu.r[RDI], 0x7FFE, "RDI restored");
        // The two bytes that fit in the present page were really written.
        assert_eq!(cpu.ram[0x7FFE], 0x11, "byte 0 landed in the present page");
        assert_eq!(cpu.ram[0x7FFF], 0x22, "byte 1 landed in the present page");
        // The REP stopped at the fault: phys 0 holds the *single* faulting element
        // (src[2]=0x33), not src[7] (which a spin-to-zero loop would leave).
        assert_eq!(
            cpu.ram[0], 0x33,
            "the faulting element wrote the phys-0 scratch exactly once (REP stopped)"
        );

        // The kernel's #PF handler maps the page; the instruction restarts and finishes.
        map(&mut cpu, 0x8000, 0x9000); // page 2 → frame 0x9000 (present)
        cpu.flush_tlb();
        cpu.fault = None;
        for _ in 0..4 {
            if matches!(cpu.run(1), Halt::Halted) {
                break;
            }
        }
        assert_eq!(cpu.r[RCX], 0, "the REP ran to completion after the page was mapped");
        // Full 8-byte copy is byte-perfect across the page split (2 in page 1, 6 in page 2).
        assert_eq!(&cpu.ram[0x7FFE..0x8000], &src[0..2], "page-1 half of the copy");
        assert_eq!(&cpu.ram[0x9000..0x9006], &src[2..8], "page-2 half of the copy");
    }

    #[test]
    fn rep_stos_bulk_fast_path_fills_a_page_like_the_per_element_loop() {
        // The `clear_page` hot path: a page-aligned 4 KiB `rep stosq` of zero. The bulk
        // fast path must leave RAM + registers exactly as the per-element loop would.
        // (Paging off → `translate_acc` is identity, so RDI is its own phys address.)
        let mut cpu = Cpu::new(256 * 1024);
        let dst = 0x10_000usize;
        cpu.ram[dst..dst + 4096].fill(0xCD); // poison so a missed byte is visible
        cpu.r[RDI] = dst as u64;
        cpu.r[RCX] = 512; // 512 × 8 B = one 4 KiB page
        cpu.r[RAX] = 0; // clear_page zeroes
        cpu.rflags &= !RFLAGS_DF; // forward
        cpu.string_op(StringOp::Stos, 8, RepKind::Rep, cpu.rip);
        assert_eq!(cpu.r[RCX], 0, "REP consumed the whole count");
        assert_eq!(cpu.r[RDI], (dst + 4096) as u64, "RDI advanced by the byte length");
        assert!(
            cpu.ram[dst..dst + 4096].iter().all(|&b| b == 0),
            "the whole page was zeroed (clear_page)"
        );

        // Byte-granular, non-zero fill (small `memset`/`__clear_user` shape).
        cpu.ram[dst..dst + 4096].fill(0);
        cpu.r[RDI] = dst as u64;
        cpu.r[RCX] = 4096;
        cpu.r[RAX] = 0xAB;
        cpu.rflags &= !RFLAGS_DF;
        cpu.string_op(StringOp::Stos, 1, RepKind::Rep, cpu.rip);
        assert_eq!(cpu.r[RDI], (dst + 4096) as u64);
        assert!(
            cpu.ram[dst..dst + 4096].iter().all(|&b| b == 0xAB),
            "byte-granular fill covered the page"
        );

        // A partial fill that does NOT span the page must still be exact, and must not
        // touch the byte just past the end.
        cpu.ram[dst..dst + 4096].fill(0);
        cpu.r[RDI] = dst as u64;
        cpu.r[RCX] = 10;
        cpu.r[RAX] = 0xFFFF_FFFF_FFFF_FFFF;
        cpu.string_op(StringOp::Stos, 8, RepKind::Rep, cpu.rip);
        assert!(cpu.ram[dst..dst + 80].iter().all(|&b| b == 0xFF), "10 qwords filled");
        assert_eq!(cpu.ram[dst + 80], 0, "the byte past the fill is untouched");
    }

    #[test]
    fn rep_movs_bulk_fast_path_copies_like_the_per_element_loop() {
        // The `copy_user` hot path: a forward `rep movsq` of non-overlapping, single-page
        // src/dst. The bulk slice copy must leave RAM + registers exactly as the
        // per-element loop would. (Paging off → identity translate.)
        let mut cpu = Cpu::new(256 * 1024);
        let (src, dst) = (0x10_000usize, 0x20_000usize); // non-overlapping, distinct pages
        for i in 0..4096 {
            cpu.ram[src + i] = (i as u8).wrapping_mul(7).wrapping_add(1);
        }
        cpu.ram[dst..dst + 4096].fill(0);
        cpu.r[RSI] = src as u64;
        cpu.r[RDI] = dst as u64;
        cpu.r[RCX] = 512; // 512 × 8 B = 4 KiB
        cpu.rflags &= !RFLAGS_DF;
        cpu.string_op(StringOp::Movs, 8, RepKind::Rep, cpu.rip);
        assert_eq!(cpu.r[RCX], 0, "REP consumed the whole count");
        assert_eq!(cpu.r[RSI], (src + 4096) as u64, "RSI advanced");
        assert_eq!(cpu.r[RDI], (dst + 4096) as u64, "RDI advanced");
        let (a, b) = cpu.ram.split_at(0x18_000);
        assert_eq!(&a[src..src + 4096], &b[0x8000..0x8000 + 4096], "dst == src after copy");

        // A partial, byte-granular copy that does not span the page, and leaves the
        // byte just past the end untouched.
        cpu.ram[dst..dst + 4096].fill(0xEE);
        cpu.r[RSI] = src as u64;
        cpu.r[RDI] = dst as u64;
        cpu.r[RCX] = 13;
        cpu.string_op(StringOp::Movs, 1, RepKind::Rep, cpu.rip);
        assert_eq!(cpu.r[RDI], (dst + 13) as u64);
        for i in 0..13 {
            assert_eq!(cpu.ram[dst + i], cpu.ram[src + i], "byte {i} copied");
        }
        assert_eq!(cpu.ram[dst + 13], 0xEE, "the byte past the copy is untouched");
    }

    #[test]
    fn call_to_a_retpoline_thunk_fast_paths_to_the_register() {
        let mut cpu = Cpu::new(64 * 1024); // paging off → identity translate
        // `__x86_indirect_thunk_rax` body at 0x6000: call +6; int3; mov %rax,(%rsp); jmp …
        cpu.ram[0x6000..0x600b]
            .copy_from_slice(&[0xe8, 0x01, 0, 0, 0, 0xcc, 0x48, 0x89, 0x04, 0x24, 0xe9]);
        // `call 0x6000` at 0x5000 (E8 rel32).
        let rel = (0x6000i64 - 0x5005) as i32;
        cpu.ram[0x5000] = 0xe8;
        cpu.ram[0x5001..0x5005].copy_from_slice(&rel.to_le_bytes());
        cpu.rip = 0x5000;
        cpu.r[RAX] = 0x7000; // the indirect target
        cpu.r[RSP] = 0x4000;
        cpu.step().expect("the call executes");
        assert_eq!(cpu.rip, 0x7000, "fast-pathed straight to RAX (== call *rax), skipping the thunk");
        assert_eq!(cpu.r[RSP], 0x3ff8, "the outer return address was pushed (rsp -= 8)");
        assert_eq!(
            u64::from_le_bytes(cpu.ram[0x3ff8..0x4000].try_into().unwrap()),
            0x5005,
            "the pushed value is the return address (the instruction after the call)"
        );

        // A normal call (target is NOT a retpoline) jumps to the target unchanged.
        cpu.ram[0x8000] = 0x90; // nop — not a thunk body
        let rel2 = (0x8000i64 - 0x5005) as i32;
        cpu.ram[0x5001..0x5005].copy_from_slice(&rel2.to_le_bytes());
        cpu.rip = 0x5000;
        cpu.r[RSP] = 0x4000;
        cpu.step().expect("the second call executes");
        assert_eq!(cpu.rip, 0x8000, "a non-retpoline call jumps to its target normally");
    }

    /// Micro-benchmark: the bulk string-op fast paths vs the per-element interpreter,
    /// on a synthetic (ASLR-independent) workload — a clean speedup number for the two
    /// hottest *instruction-heavy* ops (each moves a 4 KiB page per call).
    #[test]
    #[ignore = "micro-benchmark (timing) — run explicitly with --nocapture"]
    fn bench_string_op_fast_paths() {
        use std::time::Instant;
        let mut cpu = Cpu::new(1 << 20); // 1 MiB, paging off (identity translate)
        const N: u32 = 200_000;
        let stos = |cpu: &mut Cpu| {
            for _ in 0..N {
                cpu.r[RDI] = 0x10000;
                cpu.r[RCX] = 512; // 512 qwords = one 4 KiB page (clear_page)
                cpu.r[RAX] = 0;
                cpu.rflags &= !RFLAGS_DF;
                cpu.string_op(StringOp::Stos, 8, RepKind::Rep, cpu.rip);
            }
        };
        let movs = |cpu: &mut Cpu| {
            for _ in 0..N {
                cpu.r[RSI] = 0x10000;
                cpu.r[RDI] = 0x20000;
                cpu.r[RCX] = 512;
                cpu.rflags &= !RFLAGS_DF;
                cpu.string_op(StringOp::Movs, 8, RepKind::Rep, cpu.rip);
            }
        };
        for (name, bench) in [
            ("rep stosq (clear_page)", &stos as &dyn Fn(&mut Cpu)),
            ("rep movsq (copy_user)", &movs),
        ] {
            set_fastpaths(true);
            bench(&mut cpu); // warm
            let t0 = Instant::now();
            bench(&mut cpu);
            let on = t0.elapsed();
            set_fastpaths(false);
            bench(&mut cpu); // warm
            let t1 = Instant::now();
            bench(&mut cpu);
            let off = t1.elapsed();
            set_fastpaths(true);
            eprintln!(
                "  {name}: fast {on:>10.2?}  vs interpret {off:>10.2?}  = {:.1}x  ({N} × 4 KiB)",
                off.as_secs_f64() / on.as_secs_f64()
            );
        }
    }

    #[test]
    fn stac_and_clac_drive_the_ac_flag() {
        // SMAP: STAC sets RFLAGS.AC, CLAC clears it. The kernel's page-fault handler
        // checks `regs->flags & AC` to tell a legitimate (stac-bracketed) faulting
        // `copy_to_user` from a stray kernel access to user memory — the latter is
        // `page_fault_oops`. Without modelling AC, a demand-paged `copy_to_user` during
        // execve oopses and the kernel panics "Attempted to kill init" for certain ASLR
        // layouts (the long-standing x86 boot blocker). This guards the fix.
        let mut cpu = Cpu::new(64 * 1024);
        cpu.ram[0..3].copy_from_slice(&[0x0f, 0x01, 0xcb]); // STAC
        cpu.rip = 0;
        cpu.rflags &= !RFLAGS_AC;
        cpu.step().expect("STAC executes");
        assert!(cpu.rflags & RFLAGS_AC != 0, "STAC set RFLAGS.AC");

        cpu.ram[0..3].copy_from_slice(&[0x0f, 0x01, 0xca]); // CLAC
        cpu.rip = 0;
        cpu.step().expect("CLAC executes");
        assert_eq!(cpu.rflags & RFLAGS_AC, 0, "CLAC cleared RFLAGS.AC");
    }

    /// A1 repro (the execve/SMAP keystone) — the *whole* fault path Alpine's
    /// `copy_to_user` hits at execve, end to end through the IDT: `STAC` (AC←1), then
    /// a STAC-bracketed `REP MOVSB` whose destination crosses into a **not-present user
    /// page**, faulting. The kernel's `#PF` handler distinguishes a legitimate
    /// stac-bracketed `copy_to_user` fault from a stray kernel access by reading
    /// `regs->flags & AC` **on the exception frame** — so the saved RFLAGS that
    /// `deliver_interrupt` pushes MUST carry `AC=1`. If it pushes `AC=0`, the real
    /// handler treats the fault as a SMAP violation / oops and SIGSEGVs init
    /// (`Attempted to kill init`, exitcode 0x0b) — the deterministic guest-7.354451 s
    /// panic. This asserts the saved frame's RFLAGS has `AC` (and CR2 is the faulting
    /// page, and the fault vectored to the handler), the differential the fix is
    /// written against.
    #[test]
    fn execve_smap_copy_to_user_fault_delivers_ac_on_the_pf_frame() {
        let mut cpu = Cpu::new(256 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        // 4-level identity tables (as `rep_movsb_faults…`), PT at 0x4000.
        put(&mut cpu, 0x1000, 0x2000 | 0b11); // PML4[0] → PDPT
        put(&mut cpu, 0x2000, 0x3000 | 0b11); // PDPT[0] → PD
        put(&mut cpu, 0x3000, 0x4000 | 0b11); // PD[0]   → PT
        let map = |cpu: &mut Cpu, va: u64, pa: u64| {
            put(cpu, 0x4000 + ((va >> 12) as usize) * 8, pa | 0b11);
        };
        map(&mut cpu, 0x5000, 0x5000); // code
        map(&mut cpu, 0x6000, 0x6000); // source page
        map(&mut cpu, 0x7000, 0x7000); // destination page 1 (present)
        // VA 0x8000 (destination page 2) deliberately NOT present → the fault.
        map(&mut cpu, 0xA000, 0xA000); // #PF handler
        map(&mut cpu, 0xB000, 0xB000); // IDT
        map(&mut cpu, 0xC000, 0xC000); // kernel stack
        cpu.cr3 = 0x1000;
        cpu.cr4 = (1 << 5) | (1 << 21); // PAE | SMAP
        cpu.efer = 1 << 8; // LME
        cpu.cr0 = 1 << 31; // PG

        // IDT with a vector-14 (#PF) gate → handler at 0xA000. `deliver_interrupt`
        // reads off = (lo&0xffff)|((lo>>32)&0xffff0000)|((hi&0xffffffff)<<32); for a
        // 16-bit offset only lo's low word matters (selector/type/ist left 0).
        put(&mut cpu, 0xB000 + 14 * 16, 0xA000); // lo: offset[0..16] = 0xA000
        put(&mut cpu, 0xB000 + 14 * 16 + 8, 0); // hi
        cpu.ram[0xA000] = 0xF4; // handler: hlt
        cpu.sys_mut().idtr = (0xB000, 0x0FFF);

        // Code: STAC (0f 01 cb) ; REP MOVSB (f3 a4) ; hlt — faults inside the REP.
        cpu.ram[0x5000..0x5006].copy_from_slice(&[0x0f, 0x01, 0xcb, 0xf3, 0xa4, 0xf4]);
        cpu.ram[0x6000..0x6008].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        cpu.rip = 0x5000;
        cpu.cpl = 0; // kernel doing copy_to_user
        cpu.rflags &= !RFLAGS_AC; // AC starts clear (SMAP enforcing)
        cpu.r[RSP] = 0xCFF0; // kernel stack (present, identity-mapped)
        cpu.r[RCX] = 8;
        cpu.r[RSI] = 0x6000;
        cpu.r[RDI] = 0x7FFE; // 2 bytes in page 1, then crosses into absent 0x8000
        cpu.rflags &= !RFLAGS_DF;

        cpu.step().expect("STAC executes"); // AC ← 1
        assert!(cpu.rflags & RFLAGS_AC != 0, "STAC set AC before the copy");

        let kstack = cpu.r[RSP]; // RSP before the fault's pushes
        cpu.step().expect("REP MOVSB faults; #PF vectors (does not halt the core)");

        // The fault vectored through the IDT to the handler with CR2 = the bad page.
        assert_eq!(cpu.cr2, 0x8000, "CR2 = the not-present user page");
        assert_eq!(cpu.rip, 0xA000, "#PF vectored to the IDT handler");
        // The frame deliver_interrupt pushed (no ring change at CPL0): from the new
        // RSP — error@+0, rip@+8, cs@+16, rflags@+24. Read the saved RFLAGS.
        let frame_rflags = u64::from_le_bytes(
            cpu.ram[(kstack as usize - 24)..(kstack as usize - 16)].try_into().unwrap(),
        );
        assert!(
            frame_rflags & RFLAGS_AC != 0,
            "the #PF exception frame's saved RFLAGS carries AC=1 — the guest handler \
             recognises a legitimate stac-bracketed copy_to_user fault and fixes up, \
             instead of oopsing and SIGSEGVing init (got rflags={frame_rflags:#x})"
        );
    }

    #[test]
    fn call_and_ret_use_the_stack() {
        // call +1 (push ret, jump over) ; hlt ; (target:) ret would pop — instead
        // verify push/pop directly: mov rax,0x1234 ; push rax ; pop rbx ; hlt
        let code = [
            0x48, 0xc7, 0xc0, 0x34, 0x12, 0, 0,    // mov rax, 0x1234
            0x50, // push rax
            0x5b, // pop rbx
            0xf4, // hlt
        ];
        assert_eq!(run(&code).reg(3), 0x1234);
    }

    /// Step-C de-risk: real guest bytes from a `Cpu`'s RAM, decoded by the library JIT
    /// (`emulator::jit`), compiled to wasm, and executed over the same register state, must
    /// match what the interpreter `step()` produces — proving the decode→compile→execute
    /// dispatch plumbing on register dataflow before the boot-gated integration. (rflags is
    /// the next slab: the IR does not yet model x86 flags, so this compares registers; a
    /// flag-reading block would need flag modeling first. Memory/TLB dispatch is also later.)
    #[test]
    fn jit_block_matches_step_on_register_dataflow() {
        use crate::emulator::jit;
        use wasmtime::{Engine, Instance, Module, Store};

        // rbx = rax ; rbx += rcx ; rbx ^= rdx ; hlt — reg-reg ops the JIT models.
        let code = [
            0x48, 0x89, 0xc3, // mov rbx, rax
            0x48, 0x01, 0xcb, // add rbx, rcx
            0x48, 0x31, 0xd3, // xor rbx, rdx
            0xf4, // hlt
        ];
        let mut init = [0u64; 16];
        init[0] = 0x0102_0304_0506_0708; // rax
        init[1] = 0x1111_1111_1111_1111; // rcx
        init[2] = 0xffff_0000_ffff_0000; // rdx
        init[3] = 0xDEAD; // rbx (overwritten by the block)

        // oracle: the real interpreter steps the block to the hlt.
        let mut cpu = Cpu::new(64 * 1024);
        cpu.load_at(0, &code);
        cpu.r = init;
        cpu.run(100);
        let want = cpu.r;

        // JIT: decode the same guest bytes → compile → execute over the same registers.
        let ir = jit::decode_x86(&code);
        assert_eq!(ir.len(), 3, "the three reg-reg ops decode (hlt bails)");
        let wasm = jit::compile(&ir);
        let engine = Engine::default();
        let module = Module::new(&engine, &wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let inst = Instance::new(&mut store, &module, &[]).unwrap();
        let mem = inst.get_memory(&mut store, "mem").unwrap();
        let run = inst.get_typed_func::<(), ()>(&mut store, "run").unwrap();
        for (i, v) in init.iter().enumerate() {
            mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
        }
        run.call(&mut store, ()).unwrap();
        let mut got = [0u64; 16];
        for (i, g) in got.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            mem.read(&store, i * 8, &mut b).unwrap();
            *g = u64::from_le_bytes(b);
        }

        assert_eq!(got, want, "JIT block diverged from step() on register dataflow");
        assert_eq!(got[3], (init[0].wrapping_add(init[1])) ^ init[2], "rbx = (rax+rcx)^rdx");
    }

    /// Step-C de-risk #2: the JIT's `rflags` model (`compile_tlb_flags`) must match the real
    /// interpreter's flag computation (`flags_arith`/`flags_logic`), not just the in-module
    /// oracle. A flag-setting block run through both must agree on registers AND rflags —
    /// including that AF (which the interpreter never touches) is preserved unchanged.
    #[test]
    fn jit_flags_block_matches_step() {
        use crate::emulator::jit;
        use wasmtime::{Engine, Instance, Module, Store};

        // rbx ^= rax ; rbx += rcx ; rbx -= rdx — exercises logical + add + sub flags. No hlt:
        // run exactly the three ALU instructions (a hlt with IF=1 would tick the timer and
        // perturb rflags, which is not what we are comparing).
        let code = [
            0x48, 0x31, 0xc3, // xor rbx, rax
            0x48, 0x01, 0xcb, // add rbx, rcx
            0x48, 0x29, 0xd3, // sub rbx, rdx
        ];
        let mut init = [0u64; 16];
        init[0] = 0x0000_0000_dead_beef; // rax
        init[1] = 0x0000_0000_0000_0007; // rcx
        init[2] = 0xffff_ffff_ffff_fff0; // rdx (forces a borrow on the final sub)
        init[3] = 0x1234_5678_9abc_def0; // rbx
        let rflags0 = 0x2 | (1 << 4) | (1 << 9); // reserved + AF=1 + IF=1 (must be preserved)

        let mut cpu = Cpu::new(64 * 1024);
        cpu.load_at(0, &code);
        cpu.r = init;
        cpu.rflags = rflags0;
        cpu.run(3); // exactly the three ALU instructions
        let (want_r, want_f) = (cpu.r, cpu.rflags);

        let ir = jit::decode_x86(&code);
        let wasm = jit::compile_tlb_flags(&ir);
        let engine = Engine::default();
        let module = Module::new(&engine, &wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let inst = Instance::new(&mut store, &module, &[]).unwrap();
        let mem = inst.get_memory(&mut store, "mem").unwrap();
        let run = inst.get_typed_func::<(), i32>(&mut store, "run").unwrap();
        for (i, v) in init.iter().enumerate() {
            mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
        }
        mem.write(&mut store, 128, &rflags0.to_le_bytes()).unwrap(); // RFLAGS_MEM
        let bail = run.call(&mut store, ()).unwrap();
        assert_eq!(bail, 3, "no memory ops → block completes (bail == len)");
        let mut got_r = [0u64; 16];
        for (i, g) in got_r.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            mem.read(&store, i * 8, &mut b).unwrap();
            *g = u64::from_le_bytes(b);
        }
        let mut fb = [0u8; 8];
        mem.read(&store, 128, &mut fb).unwrap();
        let got_f = u64::from_le_bytes(fb);

        assert_eq!(got_r, want_r, "JIT registers diverged from step()");
        assert_eq!(got_f, want_f, "JIT rflags diverged from step() (incl. preserved AF/IF)");
    }

    /// Step-1 gate: the JIT executor matches `step()` over REAL long-mode paging — the
    /// in-wasm TLB is filled from the interpreter's own translation, and a load/add/store
    /// block executes against guest RAM indexed by physical address. Maps VA page 0 → PA
    /// frame 0x5000; code lives low in the frame (VA 0), data high (VA 0x800).
    #[test]
    fn jit_executor_matches_step_through_paging() {
        use crate::emulator::jit;
        use wasmtime::{Engine, Instance, Module, Store};

        let code = [
            0x48, 0x8b, 0x03, // mov rax, [rbx]
            0x48, 0x01, 0xc8, // add rax, rcx
            0x48, 0x89, 0x03, // mov [rbx], rax
        ];
        let data0: u64 = 0x0000_1111_2222_3333;
        let rcx: u64 = 0x0000_0000_0000_0007;

        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        put(&mut cpu, 0x1000, 0x2000 | 1); // PML4[0] → PDPT
        put(&mut cpu, 0x2000, 0x3000 | 1); // PDPT[0] → PD
        put(&mut cpu, 0x3000, 0x4000 | 1); // PD[0]   → PT
        put(&mut cpu, 0x4000, 0x5000 | 1); // PT[0]   → frame 0x5000
        cpu.ram[0x5000..0x5000 + code.len()].copy_from_slice(&code); // code at VA 0
        cpu.ram[0x5800..0x5808].copy_from_slice(&data0.to_le_bytes()); // data at VA 0x800
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5; // PAE
        cpu.efer = 1 << 8; // LME
        cpu.cr0 = 1 << 31; // PG → paging on
        cpu.r[3] = 0x800; // rbx → VA of the data
        cpu.r[1] = rcx;
        let (init_r, init_f) = (cpu.r, cpu.rflags);
        let init_ram = cpu.ram.clone(); // snapshot BEFORE the store mutates guest RAM
        cpu.run(3); // load ; add ; store
        let (want_r, want_f) = (cpu.r, cpu.rflags);
        let want_data = u64::from_le_bytes(cpu.ram[0x5800..0x5808].try_into().unwrap());

        // JIT: decode the block, fill the in-wasm TLB from the interpreter's own translation,
        // execute over a copy of guest RAM (indexed by physical address: host = GUEST_BASE+PA).
        let (ops, _, _) = jit::decode_block(&code);
        let wasm = jit::compile_tlb_flags(&ops);
        let pa = cpu.translate(0x800); // the interpreter's translation of the data VA
        let (vpage, host_off) = (0x800u64 >> 12, pa & !0xfff);
        let mut tlb = [0u8; 64 * 16]; // TLB_SIZE * 16
        let slot = (vpage & 63) as usize;
        tlb[slot * 16..slot * 16 + 8].copy_from_slice(&vpage.to_le_bytes());
        tlb[slot * 16 + 8..slot * 16 + 16].copy_from_slice(&host_off.to_le_bytes());

        let engine = Engine::default();
        let module = Module::new(&engine, &wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let inst = Instance::new(&mut store, &module, &[]).unwrap();
        let mem = inst.get_memory(&mut store, "mem").unwrap();
        let run = inst.get_typed_func::<(), i32>(&mut store, "run").unwrap();
        // jit.rs offsets: regs at i*8, rflags at 128, TLB at 0x200, guest RAM at 0x1000.
        for (i, v) in init_r.iter().enumerate() {
            mem.write(&mut store, i * 8, &v.to_le_bytes()).unwrap();
        }
        mem.write(&mut store, 128, &init_f.to_le_bytes()).unwrap();
        mem.write(&mut store, 0x200, &tlb).unwrap();
        mem.write(&mut store, 0x1000, &init_ram).unwrap(); // guest RAM (initial) by physical address
        let bail = run.call(&mut store, ()).unwrap();
        assert_eq!(bail, 3, "page present in the TLB → block completes");

        let mut got_r = [0u64; 16];
        for (i, g) in got_r.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            mem.read(&store, i * 8, &mut b).unwrap();
            *g = u64::from_le_bytes(b);
        }
        let mut fb = [0u8; 8];
        mem.read(&store, 128, &mut fb).unwrap();
        let got_f = u64::from_le_bytes(fb);
        let mut db = [0u8; 8];
        mem.read(&store, 0x1000 + 0x5800, &mut db).unwrap();
        let got_data = u64::from_le_bytes(db);

        assert_eq!(got_r, want_r, "executor regs diverged from step() under paging");
        assert_eq!(got_f, want_f, "executor rflags diverged from step() under paging");
        assert_eq!(got_data, want_data, "executor guest RAM diverged from step() under paging");
        assert_eq!(got_data, data0.wrapping_add(rcx), "stored sum landed at the data VA");
    }

    /// The same paging gate, but through the real library executor `jit_exec::exec_block`
    /// (the `jit` feature) rather than inline wasmtime — proving the promoted lib executor
    /// matches `step()`. Run with `cargo test -p holospaces --features jit`.
    #[cfg(feature = "jit-native")]
    #[test]
    fn jit_exec_block_matches_step_through_paging() {
        use crate::emulator::{jit, jit_exec};

        let code = [
            0x48, 0x8b, 0x03, // mov rax, [rbx]
            0x48, 0x01, 0xc8, // add rax, rcx
            0x48, 0x89, 0x03, // mov [rbx], rax
        ];
        let data0: u64 = 0x0000_1111_2222_3333;
        let rcx: u64 = 7;

        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| {
            cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        };
        put(&mut cpu, 0x1000, 0x2000 | 1);
        put(&mut cpu, 0x2000, 0x3000 | 1);
        put(&mut cpu, 0x3000, 0x4000 | 1);
        put(&mut cpu, 0x4000, 0x5000 | 1);
        cpu.ram[0x5000..0x5000 + code.len()].copy_from_slice(&code);
        cpu.ram[0x5800..0x5808].copy_from_slice(&data0.to_le_bytes());
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5;
        cpu.efer = 1 << 8;
        cpu.cr0 = 1 << 31;
        cpu.r[3] = 0x800;
        cpu.r[1] = rcx;
        let (init_r, init_f) = (cpu.r, cpu.rflags);
        let mut ram = cpu.ram.clone(); // initial guest RAM (before the store)
        cpu.run(3);
        let (want_r, want_f) = (cpu.r, cpu.rflags);
        let want_data = u64::from_le_bytes(cpu.ram[0x5800..0x5808].try_into().unwrap());

        let (ops, _, _) = jit::decode_block(&code);
        let wasm = jit::compile_tlb_flags(&ops);
        let pa = cpu.translate(0x800);
        let (vpage, host_off) = (0x800u64 >> 12, pa & !0xfff);
        let mut tlb = [0u8; 64 * 16];
        let slot = (vpage & 63) as usize;
        tlb[slot * 16..slot * 16 + 8].copy_from_slice(&vpage.to_le_bytes());
        tlb[slot * 16 + 8..slot * 16 + 16].copy_from_slice(&host_off.to_le_bytes());

        let mut regs = init_r;
        let mut rflags = init_f;
        let bail = jit_exec::exec_block(&wasm, &mut regs, &mut rflags, &mut ram, &tlb);
        assert_eq!(bail, 3, "block completes via the lib executor");
        assert_eq!(regs, want_r, "lib executor regs diverged from step()");
        assert_eq!(rflags, want_f, "lib executor rflags diverged from step()");
        let got_data = u64::from_le_bytes(ram[0x5800..0x5808].try_into().unwrap());
        assert_eq!(got_data, want_data, "lib executor guest RAM diverged from step()");
    }

    /// The CHAINING dispatch end-to-end with the SAFETY differential: a guest LOOP is SHADOWED
    /// (run dry, not applied) for `JIT_TRUST_K` rounds — each compared against the interpreter at
    /// the region's exit — then TRUSTED, and the next `jit_run_region` COMMITS it, landing
    /// bit-identical to interpreting the loop instruction-by-instruction.
    #[cfg(feature = "jit-native")]
    #[test]
    fn jit_run_region_shadow_trust_commit_on_a_loop() {
        // top: add rax,rcx ; sub rbx,rdx ; jnz top   (jnz rel -8 → back to top; fall exits)
        let code = [0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x75, 0xf8];
        // 200 iters × 3 insns = 600 ≥ REGION_MIN_INSNS (512), so the region clears the payoff-gate
        // and is trusted (a short loop would be refused as not worth the per-commit marshalling).
        let n = 200u64;
        let setup = |cpu: &mut Cpu| {
            let put = |cpu: &mut Cpu, at: usize, e: u64| {
                cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
            };
            put(cpu, 0x1000, 0x2000 | 1);
            put(cpu, 0x2000, 0x3000 | 1);
            put(cpu, 0x3000, 0x4000 | 1);
            put(cpu, 0x4000, 0x5000 | 1);
            cpu.ram[0x5000..0x5000 + code.len()].copy_from_slice(&code);
            cpu.cr3 = 0x1000;
            cpu.cr4 = 1 << 5;
            cpu.efer = 1 << 8;
            cpu.cr0 = 1 << 31;
            cpu.rip = 0;
            cpu.r[0] = 0;
            cpu.r[1] = 5; // rcx
            cpu.r[3] = n; // rbx counter
            cpu.r[2] = 1; // rdx
        };
        // Clean JIT state for test isolation (thread-locals can carry across tests on a pooled thread).
        JIT_TRUSTED.with(|c| c.borrow_mut().clear());
        JIT_REFUSED.with(|c| c.borrow_mut().clear());
        JIT_REGION_COUNT.with(|c| c.borrow_mut().clear());
        JIT_REGION_PENDING.with(|c| *c.borrow_mut() = None);
        JIT_REGION_CACHE.with(|c| c.borrow_mut().clear());

        let mut interp = Cpu::new(64 * 1024);
        setup(&mut interp);
        interp.run(n * 3);

        // SHADOW: K rounds of (shadow the region dry → interpret the loop → compare at the exit).
        for round in 0..JIT_TRUST_K {
            let mut cpu = Cpu::new(64 * 1024);
            setup(&mut cpu);
            let entry = cpu.rip;
            assert!(!cpu.jit_run_region(), "round {round}: an untrusted region shadows (no commit)");
            assert_eq!(cpu.rip, entry, "shadow must not advance rip");
            cpu.run(n * 3); // the interpreter runs the loop to its exit
            cpu.jit_region_shadow_check(); // compare at the exit rip → trust-count
        }
        // COMMIT: the region is now trusted — one call runs the whole loop in wasm.
        let mut cpu = Cpu::new(64 * 1024);
        setup(&mut cpu);
        assert!(cpu.jit_run_region(), "the trusted region commits in one call");
        assert_eq!(cpu.r, interp.r, "committed registers bit-identical to step()");
        assert_eq!(cpu.rflags, interp.rflags, "committed rflags bit-identical");
        assert_eq!(cpu.rip, interp.rip, "committed rip matches (the loop's fall-through)");
        assert_eq!(cpu.r[0], n * 5, "rax = n * rcx");
        assert_eq!(cpu.r[3], 0, "rbx counted to 0");
    }

    /// BENCHMARK (ignored) — the chained-JIT speedup: a long loop run as ONE region
    /// (`exec_region_pooled`) vs the interpreter (`Cpu::run`). Run:
    /// `cargo test -p holospaces --release --features jit --lib region_jit_speedup -- --ignored --nocapture`
    #[cfg(feature = "jit-native")]
    #[test]
    #[ignore = "benchmark: region JIT vs interpreter on a hot loop"]
    fn region_jit_speedup() {
        use crate::emulator::{jit, jit_exec};
        use std::time::Instant;
        let code = [0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x75, 0xf8]; // add rax,rcx; sub rbx,rdx; jnz top
        let l = 2_000_000u64; // loop iterations
        // Interpreter: a paged Cpu runs the loop instruction-by-instruction.
        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        put(&mut cpu, 0x1000, 0x2000 | 1);
        put(&mut cpu, 0x2000, 0x3000 | 1);
        put(&mut cpu, 0x3000, 0x4000 | 1);
        put(&mut cpu, 0x4000, 0x5000 | 1);
        cpu.ram[0x5000..0x5000 + code.len()].copy_from_slice(&code);
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5;
        cpu.efer = 1 << 8;
        cpu.cr0 = 1 << 31;
        cpu.rip = 0;
        cpu.r[1] = 5;
        cpu.r[3] = l;
        cpu.r[2] = 1;
        let t = Instant::now();
        cpu.run(l * 3); // exactly the loop (l iterations × 3 insns)
        let interp_ns = t.elapsed().as_nanos();
        // Region JIT: compile the loop once, run all l iterations in ONE exec_region_pooled call.
        let region = jit::discover_region(&code, 0, 8);
        let wasm = jit::compile_region(&region, l + 10);
        let mut entry = [0u64; 16];
        entry[1] = 5;
        entry[3] = l;
        entry[2] = 1;
        let t = Instant::now();
        let (regs, _rf, _dirty, exit) =
            jit_exec::exec_region_pooled([42u8; 32], &wasm, entry, 0, |_| None).expect("region runs");
        let region_ns = t.elapsed().as_nanos().max(1);
        assert_eq!(regs[0], cpu.r[0], "region result matches the interpreter (rax)");
        assert_eq!(regs[3], 0, "rbx hit 0");
        let _ = exit;
        eprintln!(
            "\n==== REGION JIT SPEEDUP ({l} iters) ====\n\
             interpreter (Cpu::run): {} ms\n\
             region JIT (1 exec_region_pooled call): {:.2} ms\n\
             speedup: {:.1}x\n====\n",
            interp_ns / 1_000_000,
            region_ns as f64 / 1e6,
            interp_ns as f64 / region_ns as f64,
        );
    }

    /// The cross-crate executor SEAM: an injected executor (`set_region_executor`) is the one
    /// `jit_run_region` calls — this is how the browser peer supplies its `WebAssembly` executor.
    #[cfg(feature = "jit-native")]
    #[test]
    fn region_executor_seam_routes_through_the_injected_executor() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SEAM_CALLED: AtomicBool = AtomicBool::new(false);
        // An injected executor that flags it was called, then delegates to the native one.
        fn injected(
            key: [u8; 32],
            wasm: &[u8],
            regs: [u64; 16],
            rflags: u64,
            fetch: &dyn Fn(u64) -> Option<(usize, Vec<u8>)>,
        ) -> Option<([u64; 16], u64, Vec<(usize, Vec<u8>)>, u64)> {
            SEAM_CALLED.store(true, Ordering::Relaxed);
            crate::emulator::jit_exec::exec_region_pooled(key, wasm, regs, rflags, fetch)
        }
        let code = [0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x75, 0xf8]; // a loop region
        let mut cpu = Cpu::new(64 * 1024);
        let put = |cpu: &mut Cpu, at: usize, e: u64| cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
        put(&mut cpu, 0x1000, 0x2000 | 1);
        put(&mut cpu, 0x2000, 0x3000 | 1);
        put(&mut cpu, 0x3000, 0x4000 | 1);
        put(&mut cpu, 0x4000, 0x5000 | 1);
        cpu.ram[0x5000..0x5000 + code.len()].copy_from_slice(&code);
        cpu.cr3 = 0x1000;
        cpu.cr4 = 1 << 5;
        cpu.efer = 1 << 8;
        cpu.cr0 = 1 << 31;
        cpu.rip = 0;
        cpu.r[1] = 5;
        cpu.r[3] = 8;
        cpu.r[2] = 1;
        JIT_TRUSTED.with(|c| c.borrow_mut().clear());
        JIT_REGION_PENDING.with(|c| *c.borrow_mut() = None);
        SEAM_CALLED.store(false, Ordering::Relaxed);
        set_region_executor(injected);
        let _ = cpu.jit_run_region(); // shadow or commit — either routes through the executor
        REGION_EXEC.with(|c| c.set(None)); // restore for other tests
        assert!(
            SEAM_CALLED.load(Ordering::Relaxed),
            "jit_run_region must call the INJECTED executor (the browser-peer seam)",
        );
    }

    /// The region JIT wired into `run()`: a loop (followed by NOPs) executed through the run loop
    /// with the chaining JIT ON lands bit-identical to the pure interpreter — proving the run-loop
    /// glue (hotness gate, `jit_run_region`, `jit_region_shadow_check`) preserves correctness.
    #[cfg(feature = "jit-native")]
    #[test]
    fn run_loop_through_region_jit_matches_interpreter() {
        let mut code = vec![0x48, 0x01, 0xc8, 0x48, 0x29, 0xd3, 0x75, 0xf8]; // loop
        code.extend(core::iter::repeat(0x90).take(220)); // NOPs after (so over-run is harmless)
        let n = 20u64;
        let budget = 200u64;
        let setup = |cpu: &mut Cpu| {
            let put = |cpu: &mut Cpu, at: usize, e: u64| {
                cpu.ram[at..at + 8].copy_from_slice(&e.to_le_bytes());
            };
            put(cpu, 0x1000, 0x2000 | 1);
            put(cpu, 0x2000, 0x3000 | 1);
            put(cpu, 0x3000, 0x4000 | 1);
            put(cpu, 0x4000, 0x5000 | 1);
            cpu.ram[0x5000..0x5000 + code.len()].copy_from_slice(&code);
            cpu.cr3 = 0x1000;
            cpu.cr4 = 1 << 5;
            cpu.efer = 1 << 8;
            cpu.cr0 = 1 << 31;
            cpu.rip = 0;
            cpu.r[1] = 5; // rcx
            cpu.r[3] = n; // rbx counter
            cpu.r[2] = 1; // rdx
        };
        // Pure interpreter.
        set_region_jit_on(false);
        let mut interp = Cpu::new(64 * 1024);
        setup(&mut interp);
        interp.run(budget);
        // Through the region JIT.
        JIT_TRUSTED.with(|c| c.borrow_mut().clear());
        JIT_REFUSED.with(|c| c.borrow_mut().clear());
        JIT_REGION_COUNT.with(|c| c.borrow_mut().clear());
        JIT_REGION_PENDING.with(|c| *c.borrow_mut() = None);
        JIT_REGION_CACHE.with(|c| c.borrow_mut().clear());
        JIT_REGION_HOT.with(|c| c.borrow_mut().clear());
        set_region_jit_on(true);
        let mut jit = Cpu::new(64 * 1024);
        setup(&mut jit);
        jit.run(budget);
        set_region_jit_on(false);
        assert_eq!(jit.r, interp.r, "region JIT through run() diverged from the interpreter");
        assert_eq!(jit.rflags, interp.rflags, "rflags diverged");
        assert_eq!(jit.rip, interp.rip, "rip diverged");
        assert_eq!(jit.r[0], n * 5, "loop result rax = n * rcx");
    }
}
