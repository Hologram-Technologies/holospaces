//! The **system-emulator codemodule** — the RISC-V emulator core
//! ([`holospaces::emulator`]) compiled to a *hologram container*: a κ-addressed
//! Wasm module that exports the container ABI (`hg_*`) and imports only the
//! `hologram` host ABI, run by hologram's engine (Wasmtime / `wasmi`). This is
//! the execution surface of ADR-009 — the emulator runs *on the substrate*, not
//! as a parallel medium (Law L4).
//!
//! It runs in one of two profiles, chosen by the initial state the host delivers
//! at `hg_init` (written at memory offset 0, the engine's input convention):
//!
//! * **batch** — the initial state is a flat RISC-V image; the emulator runs it
//!   to completion and emits `[exit_code u64 LE][console]` via `storage_put`
//!   (content-addressed, so the result κ is the guest's deterministic output).
//! * **operating system** — the initial state is a small *boot descriptor*
//!   (`b"HGOS"` + the kernel-image κ + the device-tree κ). The emulator reads
//!   the kernel and the DTB back out of the substrate with `storage_get`
//!   (content, by κ — Law L1/L4), becomes the SBI firmware, and boots a real OS
//!   to userspace, emitting the same `[exit_code][console]` record. This is
//!   ADR-009's claim fully realized: an arbitrary operating system boots *on the
//!   substrate*, its image delivered as content and its result content-addressed.
//!
//! The container's κ snapshot is the runtime's own (it snapshots this module's
//! linear memory), reproducible because the emulation is deterministic.

#![no_std]
#![allow(unsafe_code)] // the host-ABI FFI + reading the offset-0 input region

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

use holospaces::emulator::{Emulator, Halt};

/// The container's heap allocator (the emulator's guest RAM lives here).
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // A panic should not occur; trap deterministically if it does.
    core::arch::wasm32::unreachable()
}

// The substrate host ABI this codemodule binds (the closed host surface, CC-5):
// `storage_put` (put `mem[ptr..ptr+len]`, write the 71-byte κ to `mem[out_ptr]`)
// and `storage_get` (κ at `mem[kappa_ptr..+71]` → content into `mem[out_ptr..]`,
// capped at `out_cap`, returns the byte count or -1). No other imports.
#[link(wasm_import_module = "hologram")]
extern "C" {
    fn storage_put(ptr: *const u8, len: usize, out_ptr: *mut u8) -> i32;
    fn storage_get(kappa_ptr: *const u8, out_ptr: *mut u8, out_cap: usize) -> i32;
}

/// The boot-descriptor magic that selects the operating-system profile. The
/// descriptor is `OS_MAGIC` + four little-endian `u64` machine fields
/// (`ram_bytes`, `base`, `dtb_addr`, `max_steps`) + the kernel-image κ + the
/// device-tree κ — so the host specifies the machine, nothing is baked in.
const OS_MAGIC: &[u8; 4] = b"HGOS";
const OS_DESC_LEN: usize = 4 + 8 * 4 + 71 + 71;

/// Read κ-addressed content (the 71-byte label at `kappa`) out of the substrate
/// into a fresh buffer of at most `cap` bytes; `None` if the host refused it.
fn get_kappa(kappa: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; cap];
    let n = unsafe { storage_get(kappa.as_ptr(), buf.as_mut_ptr(), cap) };
    if n < 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(buf)
}

/// A little-endian `u64` read from a descriptor field. The caller has already
/// checked the descriptor length, so the 8-byte window is always in range.
fn rd_u64(b: &[u8], off: usize) -> u64 {
    let mut field = [0u8; 8];
    field.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(field)
}

/// Emit the run record `[exit_code u64 LE][console]` into the substrate via
/// `storage_put` (content-addressed). Returns the host status (0 = ok).
fn emit(code: u64, console: &[u8]) -> i32 {
    let mut record = Vec::with_capacity(8 + console.len());
    record.extend_from_slice(&code.to_le_bytes());
    record.extend_from_slice(console);
    let mut kappa = [0u8; 71];
    if unsafe { storage_put(record.as_ptr(), record.len(), kappa.as_mut_ptr()) } != 0 {
        return 2;
    }
    0
}

/// `hg_init(ptr=0, len)` — the engine wrote the initial state at `mem[0..len]`.
///
/// # Safety
///
/// The host guarantees `mem[0..len]` holds the initial-state bytes.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // the container ABI's host contract
pub extern "C" fn hg_init(ptr: *const u8, len: i32) -> i32 {
    let state = unsafe { core::slice::from_raw_parts(ptr, len as usize) };

    // Operating-system profile: a boot descriptor naming the machine + the
    // kernel/DTB content κ. The host owns the machine spec — no sizes are baked
    // into the codemodule, so it boots an OS of any size the host provisions.
    if state.len() == OS_DESC_LEN && &state[..4] == OS_MAGIC {
        let ram_bytes = rd_u64(state, 4) as usize;
        let base = rd_u64(state, 12);
        let dtb_addr = rd_u64(state, 20);
        let max_steps = rd_u64(state, 28);
        let kernel_kappa = &state[36..36 + 71];
        let dtb_kappa = &state[36 + 71..36 + 71 + 71];
        // The kernel must fit in RAM; the DTB sits at `dtb_addr` and runs to the
        // top of RAM — the caps follow the machine, they are not fixed numbers.
        let Some(kernel) = get_kappa(kernel_kappa, ram_bytes) else {
            return 3;
        };
        let dtb_cap = ram_bytes.saturating_sub(dtb_addr.wrapping_sub(base) as usize);
        let Some(dtb) = get_kappa(dtb_kappa, dtb_cap) else {
            return 4;
        };
        let mut emu = Emulator::new(base, ram_bytes);
        if emu.boot_kernel(&kernel, &dtb, dtb_addr).is_err() {
            return 5;
        }
        let code = match emu.run(max_steps) {
            Halt::Exit(c) => c,
            _ => u64::MAX,
        };
        return emit(code, emu.console());
    }

    // Batch profile: a flat RISC-V image run to completion. RAM is sized to the
    // image (plus a working margin); the step budget is the host's fuel-backed
    // liveness guard (a non-halting guest is bounded by the runtime's fuel).
    let ram = (state.len() * 2).max(256 * 1024);
    let mut emu = Emulator::new(0, ram);
    if emu.load_flat(state).is_err() {
        return 1;
    }
    let code = match emu.run(u64::MAX) {
        Halt::Exit(c) => c,
        _ => u64::MAX,
    };
    emit(code, emu.console())
}

#[no_mangle]
pub extern "C" fn hg_event(_ptr: *const u8, _len: i32) -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn hg_suspend() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn hg_resume() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn hg_callback(_id: i32, _ptr: i32, _len: i32) -> i32 {
    0
}
