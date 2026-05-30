//! The **system-emulator codemodule** — the RISC-V emulator core
//! ([`holospaces::emulator`]) compiled to a *hologram container*: a κ-addressed
//! Wasm module that exports the container ABI (`hg_*`) and imports only the
//! `hologram` host ABI, run by hologram's engine (Wasmtime / `wasmi`). This is
//! the execution surface of ADR-009 — the emulator runs *on the substrate*, not
//! as a parallel medium (Law L4).
//!
//! The container runs one guest: the host delivers the guest image as the
//! container's initial state at `hg_init` (written at memory offset 0, the
//! engine's input convention); the emulator runs it to completion and emits the
//! result (exit code + console output) back into the substrate via the
//! `storage_put` host call — content-addressed, so the result κ is the guest's
//! deterministic output (`CC-9`). The container's κ snapshot is the runtime's
//! own (it snapshots this module's linear memory), reproducible because the
//! emulation is deterministic.

#![no_std]
#![allow(unsafe_code)] // the host-ABI FFI + reading the offset-0 input region

extern crate alloc;
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
// the only import is `hologram.storage_put` — put `mem[ptr..ptr+len]` and write
// the 71-byte κ-label to `mem[out_ptr..]`.
#[link(wasm_import_module = "hologram")]
extern "C" {
    fn storage_put(ptr: *const u8, len: usize, out_ptr: *mut u8) -> i32;
}

/// Guest RAM size (the ISA-conformance guests are tiny; grown with the OS).
const RAM_BYTES: usize = 256 * 1024;
/// Liveness bound on a single guest run (the host's fuel budget bounds it too).
const MAX_STEPS: u64 = 5_000_000;

/// `hg_init(ptr=0, len)` — the engine wrote the guest image at `mem[0..len]`.
/// Run it on the emulator and emit `[exit_code u64 LE][console bytes]` via
/// `storage_put` (the result is content-addressed in the substrate).
///
/// # Safety
///
/// The host guarantees `mem[0..len]` holds the initial-state bytes.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)] // the container ABI's host contract
pub extern "C" fn hg_init(ptr: *const u8, len: i32) -> i32 {
    let image = unsafe { core::slice::from_raw_parts(ptr, len as usize) };
    let mut emu = Emulator::new(0, RAM_BYTES);
    if emu.load_flat(image).is_err() {
        return 1;
    }
    let halt = emu.run(MAX_STEPS);
    let code = match halt {
        Halt::Exit(c) => c,
        _ => u64::MAX,
    };
    let mut record = Vec::with_capacity(8 + emu.console().len());
    record.extend_from_slice(&code.to_le_bytes());
    record.extend_from_slice(emu.console());

    let mut kappa = [0u8; 71];
    let rc = unsafe { storage_put(record.as_ptr(), record.len(), kappa.as_mut_ptr()) };
    if rc != 0 {
        return 2;
    }
    0
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
