//! `CC-59` — warm κ-resume of the amd64 (x86-64) machine is **instant** and a
//! **fixed point**: a content-addressed snapshot, resumed into a fresh core,
//! reconstructs the *whole* machine without executing a single guest instruction,
//! and from there runs byte-identically to the machine that was never snapshotted.
//!
//! This is the CI-affordable guard under the *"resume, don't re-run"* speed
//! architecture (the answer to amd64's no-JIT cold-boot cost: a region JIT is a
//! measured net loss on real short-region workloads — see
//! `cc45_x64_alpine::region_jit_real_alpine_speedup` — so the speed comes from
//! resuming the state the planet computed once, not from re-running the boot).
//!
//! The heavier behavioural proofs live in `cc44_x64_boot.rs` and run in release
//! via the CC-59 suite: `kappa_resume_lands_at_userspace_instantly` (the cold
//! boot vs κ-resume wall-clock — resume lands AT userspace with zero guest
//! execution) and `kappa_snapshot_{midboot_restore,kappa_resume}_*` (bit-exact to
//! userspace). This witness proves the same fidelity **without** a multi-minute
//! boot: it snapshots a short, live mid-boot window and shows resume is a fixed
//! point — so a snapshot/restore regression fails fast in the default test gate.
//!
//! The oracle here is the machine itself: `Cpu::snapshot_kappa` →
//! [`KappaSnapshot::to_manifest_bytes`] is the deterministic content label of the
//! *entire* machine (CPU + device state inline, every RAM page by BLAKE3 κ). Two
//! byte-equal manifests are the same machine — so equality after an equal run is
//! the Law-L1 fixed point, not a self-referential read (a dropped register, timer,
//! device queue, or RAM page would diverge the manifest within the window).

use std::io::Read;
use std::path::Path;

use hologram_store_mem::MemKappaStore;
use holospaces::emulator::x64::Cpu;

/// The committed CC-44 amd64 platform kernel, gunzipped (the x86-64 core enters
/// `startup_64` directly — 64-bit boot protocol, no in-guest decompressor).
fn vmlinux_elf() -> Vec<u8> {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc44/linux/vmlinux.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc44 vmlinux.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel ELF");
    img
}

/// A warm κ-resume reconstructs the whole machine **instantly** (no guest
/// instruction re-executed) and is a **fixed point** (it then runs byte-identically
/// to the machine that was never snapshotted). Cheap by construction: it snapshots a
/// short live mid-boot window — no run to userspace — so it guards snapshot/restore
/// fidelity in the default gate without a multi-minute boot.
#[test]
fn warm_resume_is_instant_and_a_fixed_point() {
    let kernel = vmlinux_elf();
    let cmdline = "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on";

    // A live, mid-boot machine — kernel running, deliberately NOT run to userspace
    // (that is the heavy boot the suite times; here the snapshot point is cheap).
    // 256 MiB keeps the per-page BLAKE3 manifest small enough for the default gate.
    let mut orig = Cpu::boot_linux(256 * 1024 * 1024, &kernel, cmdline);
    orig.run(3_000_000);
    let at_snapshot = orig.insns();

    // Content-address the running machine: unique RAM pages dedup into the store.
    let store = MemKappaStore::new();
    let snap = orig.snapshot_kappa(&store).expect("snapshot_kappa");

    // ── INSTANT: resume reconstructs the state; it does not re-run the boot. ──
    let mut resumed = Cpu::new(0x1000);
    assert!(
        resumed.restore_kappa(&snap, &store),
        "restore_kappa verifies every page (L5) and reconstructs the machine"
    );
    assert_eq!(
        resumed.insns(),
        at_snapshot,
        "the resumed machine is AT the snapshot point having executed zero guest \
         instructions — resume reconstructs the state, it does not re-run the boot"
    );

    // ── FIXED POINT: run both the SAME bounded budget; the whole-machine content
    // labels must be byte-equal. A snapshot that dropped any CPU/segment/device/
    // timer/interrupt/RAM state would diverge the manifest within this window. ──
    orig.run(3_000_000);
    resumed.run(3_000_000);
    assert_eq!(orig.insns(), resumed.insns(), "equal budget ⇒ equal retired count");

    let manifest_orig = orig
        .snapshot_kappa(&MemKappaStore::new())
        .expect("re-snapshot the original")
        .to_manifest_bytes();
    let manifest_resumed = resumed
        .snapshot_kappa(&MemKappaStore::new())
        .expect("re-snapshot the resumed")
        .to_manifest_bytes();
    assert_eq!(
        manifest_orig, manifest_resumed,
        "snapshot→restore→run is a FIXED POINT: the resumed machine is byte-identical \
         to the never-snapshotted machine (Law L1, the whole-machine κ manifest)"
    );

    // Belt-and-braces: the consoles match too (implied by the manifest equality).
    assert_eq!(
        orig.console(),
        resumed.console(),
        "the resumed machine's console is byte-identical to the original"
    );
}
