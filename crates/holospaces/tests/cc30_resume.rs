//! `CC-30` — a suspended machine resumes from its κ snapshot (arc42 ch.10;
//! ADR-009, the *running state as a κ snapshot* property).
//!
//! [`Emulator::snapshot`] captures a running machine as canonical, content-
//! addressed bytes (the κ the substrate stores on suspend). [`Emulator::restore`]
//! is its inverse: it reconstructs the machine so that suspend → resume is a
//! round trip. This makes "resume from a κ snapshot" real — the foundation for a
//! second launch that does not cold-boot.
//!
//! Authority/oracle: the snapshot's own determinism (Law L1) and the
//! `qemu-system-riscv64` differential oracle. Witnessed:
//!   * **round-trip identity** — `restore(snapshot(m))` re-snapshots to the
//!     *same* bytes (so restore faithfully reconstructs everything snapshot
//!     captured), including a machine with a virtio-blk disk and a virtio-9p
//!     *workspace* (the user's files survive the round trip);
//!   * **identical continuation** — the restored machine continues execution
//!     byte-identically to the one that was suspended (same console, same final
//!     snapshot, Law L1);
//!   * **resume is content** — a snapshot restored on a *fresh* context yields
//!     the same snapshot κ, so a suspended machine migrates by its κ (Law L1);
//!   * **malformed input is rejected** — a truncated snapshot is an error, never
//!     a half-restored machine;
//!   * **a real Linux boot resumes** (`#[ignore]`, release) — suspended
//!     mid-boot and restored, it reaches the byte-identical userspace the
//!     un-suspended machine did (the differential oracle).

use holospaces::emulator::{Emulator, Halt, SnapshotError};
use holospaces::realizations::address;

/// A real (deterministic) RISC-V program: place "Hi" at 0x100 and `write(1, …)`
/// it, then `exit(0)`. ~11 instructions — long enough to suspend mid-execution.
/// (Byte-for-byte the program the CC-9 snapshot-reproducibility witness drives.)
const PROG: &[u8] = &[
    0x93, 0x02, 0x80, 0x04, // addi t0,x0,0x48  ('H')
    0x23, 0x00, 0x50, 0x10, // sb   t0,256(x0)
    0x93, 0x02, 0x90, 0x06, // addi t0,x0,0x69  ('i')
    0xa3, 0x00, 0x50, 0x10, // sb   t0,257(x0)
    0x13, 0x05, 0x10, 0x00, // addi a0,x0,1
    0x93, 0x05, 0x00, 0x10, // addi a1,x0,256
    0x13, 0x06, 0x20, 0x00, // addi a2,x0,2
    0x93, 0x08, 0x00, 0x04, // addi a7,x0,64
    0x73, 0x00, 0x00, 0x00, // ecall (write)
    0x13, 0x05, 0x00, 0x00, // addi a0,x0,0
    0x93, 0x08, 0xd0, 0x05, // addi a7,x0,93
    0x73, 0x00, 0x00, 0x00, // ecall (exit)
];

#[test]
fn restore_is_the_inverse_of_snapshot_and_continues_identically() {
    // Suspend a running machine partway through execution.
    let mut suspended = Emulator::new(0, 64 * 1024);
    suspended.load_flat(PROG).unwrap();
    for _ in 0..4 {
        suspended.step_once().unwrap();
    }
    let snap = suspended.snapshot();

    // Round-trip identity: restore reconstructs everything snapshot captured, so
    // the restored machine re-snapshots to the very same bytes (and κ).
    let mut resumed = Emulator::restore(0, &snap).expect("restore a valid snapshot");
    assert_eq!(
        resumed.snapshot(),
        snap,
        "restore(snapshot(m)) re-snapshots to the same bytes (faithful inverse)"
    );
    assert_eq!(
        address(&resumed.snapshot()),
        address(&snap),
        "...and therefore the same snapshot κ (Law L1)"
    );

    // Identical continuation: both run to completion and end byte-identical.
    let a = suspended.run(1000);
    let b = resumed.run(1000);
    assert_eq!(a, b, "the resumed machine halts the same way");
    assert_eq!(a, Halt::Exit(0));
    assert_eq!(
        suspended.console(),
        resumed.console(),
        "the resumed machine produces the identical console output"
    );
    assert_eq!(
        suspended.snapshot(),
        resumed.snapshot(),
        "the resumed machine reaches the identical final state (Law L1)"
    );
}

#[test]
fn restore_reconstructs_a_machine_with_a_virtio_disk() {
    // A machine with a virtio-blk disk attached: the restored machine
    // re-serializes to byte-identical snapshot bytes (the disk/queue state is
    // captured in the snapshot). This does not read disk bytes back out of the
    // restored machine.
    let mut emu = Emulator::new(0x8000_0000, 1024 * 1024);
    let disk: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    emu.attach_disk(disk);
    let snap = emu.snapshot();
    let resumed = Emulator::restore(0x8000_0000, &snap).expect("restore");
    assert_eq!(
        resumed.snapshot(),
        snap,
        "a machine with a virtio disk re-serializes to byte-identical snapshot bytes after restore"
    );
}

#[test]
fn restore_reconstructs_a_workspace_filesystem_over_virtio_9p() {
    // A workspace machine carries the user's files in the virtio-9p share. Resume
    // must bring them back, or a resumed devcontainer would lose the editor's
    // content. The whole machine — including the 9p filesystem — round-trips.
    let mut emu = Emulator::new(0x8000_0000, 1024 * 1024);
    emu.attach_workspace(&[("README.md", b"# hello"), ("src/main.rs", b"fn main() {}")]);
    emu.workspace_write("notes.txt", b"a file the user edited");
    let snap = emu.snapshot();

    let resumed = Emulator::restore(0x8000_0000, &snap).expect("restore");
    assert_eq!(
        resumed.snapshot(),
        snap,
        "a machine with a virtio-9p workspace round-trips through snapshot/restore exactly"
    );
    // The workspace content itself survives the resume (the user's files).
    assert_eq!(resumed.workspace_file("README.md"), Some(&b"# hello"[..]));
    assert_eq!(
        resumed.workspace_file("src/main.rs"),
        Some(&b"fn main() {}"[..])
    );
    assert_eq!(
        resumed.workspace_file("notes.txt"),
        Some(&b"a file the user edited"[..]),
        "an edit made before suspend is present after resume"
    );
}

#[test]
fn a_resumed_snapshot_migrates_by_its_kappa() {
    // Resume is content: the same snapshot bytes restore to the same machine and
    // re-snapshot to the same κ — so a suspended machine moves to another peer by
    // its κ alone (Law L1), no shared mutable state.
    let mut emu = Emulator::new(0, 64 * 1024);
    emu.load_flat(PROG).unwrap();
    emu.run(3);
    let snap = emu.snapshot();
    let k = address(&snap);

    let resumed = Emulator::restore(0, &snap).unwrap();
    assert_eq!(
        address(&resumed.snapshot()),
        k,
        "resume is κ-addressed content"
    );
}

#[test]
fn restore_rejects_a_truncated_snapshot() {
    let mut emu = Emulator::new(0, 64 * 1024);
    emu.load_flat(PROG).unwrap();
    emu.run(3);
    let snap = emu.snapshot();

    // A snapshot cut within its fixed header (here, mid-register-file) is
    // malformed — restore refuses it rather than producing a half-built machine.
    // (A truncation of the trailing RAM is caught earlier, by the snapshot's own
    // κ re-derivation — Law L5 — before restore is ever called.)
    assert!(
        matches!(
            Emulator::restore(0, &snap[..20]),
            Err(SnapshotError::Truncated)
        ),
        "a snapshot truncated in its header is rejected"
    );
    assert!(
        matches!(Emulator::restore(0, &[]), Err(SnapshotError::Truncated)),
        "empty input is rejected"
    );
}

#[test]
#[ignore = "boots real Linux twice (~30s, release) — run by the CC-30 vv suite"]
fn a_suspended_real_linux_machine_resumes_to_the_identical_boot() {
    use std::io::Read;
    use std::path::PathBuf;

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc9/linux")
        .canonicalize()
        .expect("CC-9 Linux artifacts");
    let gz = std::fs::read(dir.join("Image.gz")).expect("Image.gz");
    let mut image = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut image)
        .expect("gunzip");
    let dtb = std::fs::read(dir.join("holospaces.dtb")).expect("dtb");

    let base = 0x8000_0000u64;
    let boot = |steps_before_suspend: Option<u64>| -> (Vec<u8>, Halt) {
        let mut emu = Emulator::new(base, 128 * 1024 * 1024);
        emu.boot_kernel(&image, &dtb, base + 0x0700_0000).unwrap();
        // The console *output buffer* is a projection of past execution, not
        // state that affects the future — so `snapshot` does not carry it. To
        // compare the whole boot, keep the pre-suspend output and prepend it to
        // what the resumed machine prints next (their seam must be invisible).
        let mut prefix = Vec::new();
        let mut emu = match steps_before_suspend {
            None => emu,
            Some(n) => {
                let mut ran = 0u64;
                while ran < n {
                    if !matches!(emu.run(1_000_000), Halt::OutOfBudget) {
                        break;
                    }
                    ran += 1_000_000;
                }
                prefix = emu.console().to_vec();
                Emulator::restore(base, &emu.snapshot()).expect("resume from κ")
            }
        };
        let halt = loop {
            match emu.run(10_000_000) {
                Halt::OutOfBudget => {}
                other => break other,
            }
        };
        let mut console = prefix;
        console.extend_from_slice(emu.console());
        (console, halt)
    };

    // The un-suspended boot is the oracle; the suspended-and-resumed boot must
    // reach the byte-identical userspace.
    let (plain, plain_halt) = boot(None);
    let (resumed, resumed_halt) = boot(Some(50_000_000));
    assert_eq!(plain_halt, Halt::Exit(0));
    assert_eq!(
        resumed_halt, plain_halt,
        "the resumed machine reaches the same clean power-off"
    );
    assert_eq!(
        String::from_utf8_lossy(&resumed),
        String::from_utf8_lossy(&plain),
        "a Linux machine suspended mid-boot and resumed from its κ produces the \
         byte-identical boot the un-suspended machine did (Law L1)"
    );
}
