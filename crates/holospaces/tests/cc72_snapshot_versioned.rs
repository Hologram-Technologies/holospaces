//! `CC-72` — the κ-snapshot format is **versioned**, so a blob minted by a different core fails
//! **loud and self-named** instead of silently restoring a dead machine.
//!
//! This is the durable fix for the class of failure that cost this session dearly: the snapshot
//! layout churned while the core evolved, but the magic's version digit was never bumped, so an
//! old-format blob passed the magic check, deserialized a mismatched layout, and `restore` returned
//! `true` on a **frozen VM** (empty console, zero execution) — surfacing as a cc62 "hang" and, in
//! the browser, days of a "malformed blob" chase (CC-62/CC-66). With a versioned magic, the same
//! stale blob is rejected up front and `classify_kappa_blob` names *why* ("built by another core").

use holospaces::emulator::x64::Cpu;

/// A fresh core's own snapshot is current-version and round-trips; a blob whose version byte is
/// bumped is rejected loudly (never a silent dead-machine restore); a non-κ blob is classified as
/// "not a κ-blob", not "wrong version".
#[test]
fn snapshot_blob_is_versioned_and_mismatch_fails_loud() {
    // A fresh machine snapshots to a well-formed, current-version blob.
    let cpu = Cpu::new(0x1000);
    let blob = cpu.snapshot_kappa_blob();
    assert!(!blob.is_empty(), "snapshot produced bytes");
    assert_eq!(Cpu::classify_kappa_blob(&blob), Ok(()), "own snapshot is current-version");

    // It round-trips into a fresh core.
    let mut into = Cpu::new(0x1000);
    assert!(into.restore_kappa_blob(&blob), "current-version blob restores");

    // Forge a *different-version* blob by mutating only the magic's trailing version byte
    // (index 7 of "HOLOKSB<v>"). This is exactly the stale-fixture shape that silently died before.
    let mut stale = blob.clone();
    let vpos = 7;
    let cur = stale[vpos];
    stale[vpos] = if cur == b'1' { b'0' } else { b'1' }; // any version != current
    match Cpu::classify_kappa_blob(&stale) {
        Err(Some(v)) => assert_eq!(v, stale[vpos], "reports the blob's own version"),
        other => panic!("stale-version blob should classify as Err(Some(version)), got {other:?}"),
    }
    let mut victim = Cpu::new(0x1000);
    assert!(
        !victim.restore_kappa_blob(&stale),
        "a version-mismatched blob must be REJECTED, not silently restored into a dead machine"
    );

    // A blob that isn't a κ-snapshot at all is "not a κ-blob", not "wrong version".
    assert_eq!(Cpu::classify_kappa_blob(b"not a kappa blob at all").err(), Some(None));
    assert_eq!(Cpu::classify_kappa_blob(b"x").err(), Some(None), "too-short is Err(None)");
}
