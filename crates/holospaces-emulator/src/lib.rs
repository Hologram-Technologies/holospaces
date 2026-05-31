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

/// Batch-profile guest RAM (the ISA-conformance guests are tiny).
const RAM_BYTES: usize = 256 * 1024;
/// Liveness bound on a batch run (the host's fuel budget bounds it too).
const MAX_STEPS: u64 = 5_000_000;

/// The boot-descriptor magic that selects the operating-system profile.
const OS_MAGIC: &[u8; 4] = b"HGOS";
/// OS-profile machine: 128 MiB of RAM at the standard RISC-V base, the device
/// tree high and clear of the kernel (matching `holospaces.dts` / `boot_kernel`).
const OS_BASE: u64 = 0x8000_0000;
const OS_RAM_BYTES: usize = 128 * 1024 * 1024;
const OS_DTB_ADDR: u64 = OS_BASE + 0x0700_0000;
/// Liveness bound on an OS boot (a full Linux boot to userspace is ~10⁸ steps).
const OS_MAX_STEPS: u64 = 3_000_000_000;
/// Upper bound on the kernel image read out of the substrate (the buffer cap).
const KERNEL_CAP: usize = 64 * 1024 * 1024;
const DTB_CAP: usize = 64 * 1024;

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

    // Operating-system profile: a boot descriptor (magic + kernel κ + DTB κ).
    if state.len() == OS_MAGIC.len() + 71 + 71 && &state[..4] == OS_MAGIC {
        let kernel_kappa = &state[4..4 + 71];
        let dtb_kappa = &state[4 + 71..4 + 71 + 71];
        let Some(kernel) = get_kappa(kernel_kappa, KERNEL_CAP) else {
            return 3;
        };
        let Some(dtb) = get_kappa(dtb_kappa, DTB_CAP) else {
            return 4;
        };
        let mut emu = Emulator::new(OS_BASE, OS_RAM_BYTES);
        if emu.boot_kernel(&kernel, &dtb, OS_DTB_ADDR).is_err() {
            return 5;
        }
        let code = match emu.run(OS_MAX_STEPS) {
            Halt::Exit(c) => c,
            _ => u64::MAX,
        };
        return emit(code, emu.console());
    }

    // Batch profile: a flat RISC-V image run to completion.
    let mut emu = Emulator::new(0, RAM_BYTES);
    if emu.load_flat(state).is_err() {
        return 1;
    }
    let code = match emu.run(MAX_STEPS) {
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
