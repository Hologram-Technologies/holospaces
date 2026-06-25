//! `CC-11` — the **workspace projection** renders and drives a running holospace
//! (arc42 chapter 10, Conformance catalog; ADR-009; the Codespaces/Gitpod
//! experience).
//!
//! The [`Workspace`](holospaces::projection::Workspace) is verified against
//! external authorities on its two surfaces:
//!
//! * **Terminal / Intent** — it drives a *real, running Linux terminal*: the
//!   operator's lines are published as canonical events that advance the
//!   holospace's κ snapshot, and the rendered terminal is byte-identical to the
//!   reference RISC-V machine (`qemu-system-riscv64`) on the same image — the
//!   differential oracle.
//! * **Editor / FS** — it reads the environment's content *by κ* and an edit
//!   advances that κ (Law L1), the new content re-deriving to its address on
//!   read-back (Law L5) — grounded in the substrate store (`CC-3`) and the
//!   reference σ-axis hashes (`CC-1`).

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use holospaces::emulator::Emulator;
use holospaces::projection::{Intent, Workspace};
use holospaces::substrate::KappaStore;
use holospaces::{address, verify};

fn cc11_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc11")
}

fn cc9_linux_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc9/linux")
}

/// `Intent` values — including `Intent::Edit` — are content-addressed in the
/// κ-store: content put under a κ reads back and re-derives to its address (Laws
/// L1/L5), an edit's content carries a fresh κ that equals the content's address
/// and differs from the prior version's, and two identical `Intent::Edit` values
/// share one κ (Laws L1/L2). This is a bare `MemKappaStore` witness of the
/// content-addressing identities (`CC-3` store, `CC-1` hashes) — there is no
/// `Workspace`, no editor, and no running environment. No boot required — a fast
/// cargo-tier witness.
#[test]
fn intent_edits_are_content_addressed_in_the_store() {
    let store = MemKappaStore::new();

    // Content put under a κ reads back by that κ and re-derives to it (L5). This
    // exercises the store's content-addressing directly (no editor, no holospace).
    let original = b"hello from the devcontainer\n".to_vec();
    let file_k = store.put("blake3", &original).unwrap();
    let opened = store
        .get(&file_k)
        .unwrap()
        .expect("content reads back by κ");
    let opened = &opened[..];
    assert_eq!(
        opened,
        &original[..],
        "content read back by κ equals what was put"
    );
    assert!(
        verify(opened, &file_k).unwrap(),
        "the content re-derives to its κ (Law L5)"
    );

    // An edit: an `Intent::Edit` value carrying new content with its own κ.
    let edited = b"hello from the EDITED devcontainer\n".to_vec();
    let intent = Intent::Edit {
        path: String::from("/work/readme.txt"),
        content: edited.clone(),
    };
    let new_k = intent.content_kappa().expect("an edit carries new content");
    assert_ne!(
        new_k, file_k,
        "the edited content has a different κ (Law L1)"
    );
    assert_eq!(
        new_k,
        address(&edited),
        "the intent's content κ is the content's address"
    );

    // The edited content is content-addressed in the store: it reads back by κ
    // (Law L5), and the old content keeps its own identity.
    let put_k = store.put("blake3", &edited).unwrap();
    assert_eq!(
        put_k, new_k,
        "the store address equals the intent's content κ"
    );
    assert_eq!(
        &store.get(&new_k).unwrap().unwrap()[..],
        &edited[..],
        "read back by κ (L5)"
    );
    assert_eq!(
        &store.get(&file_k).unwrap().unwrap()[..],
        &original[..],
        "the prior version keeps its identity (content is immutable, L1)"
    );

    // The intent itself is a canonical event with a stable κ (Laws L1/L2).
    let same = Intent::Edit {
        path: String::from("/work/readme.txt"),
        content: edited,
    };
    assert_eq!(
        intent.kappa(),
        same.kappa(),
        "identical intent ⇒ identical event κ"
    );
}

/// The workspace's **Terminal / Intent** surface drives a *real, running Linux
/// terminal*. The pinned interactive kernel (`vv/artifacts/cc11/`, a tiny shell
/// as PID 1) boots; the projection waits for the ready banner, then types the
/// operator's command lines. Each line is published as a canonical event and
/// **advances the holospace's κ snapshot** (it drove the running machine), and
/// the rendered terminal is **byte-identical to `qemu-system-riscv64`** on the
/// same image (`expected-session.txt`, the differential oracle).
///
/// Ignored by default (boots Linux, ~20 s, release only); the CC-11 suite runs
/// it. `cargo test --release -p holospaces --test cc11_workspace
/// the_workspace_drives -- --ignored --nocapture`.
#[test]
#[ignore = "drives an interactive Linux terminal (~20s; release only) — run by the CC-11 vv suite"]
fn the_workspace_drives_a_running_linux_terminal() {
    let gz = std::fs::read(cc11_dir().join("Image.gz")).expect("cc11 Image.gz");
    let mut kernel = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut kernel)
        .expect("gunzip the interactive kernel");
    // The same machine model as CC-9 (one device tree).
    let dtb = std::fs::read(cc9_linux_dir().join("holospaces.dtb")).expect("holospaces.dtb");
    let input = std::fs::read_to_string(cc11_dir().join("input.txt")).expect("input.txt");
    let expected = std::fs::read_to_string(cc11_dir().join("expected-session.txt"))
        .expect("expected-session.txt");

    let base = 0x8000_0000u64;
    let mut emu = Emulator::new(base, 128 * 1024 * 1024);
    emu.boot_kernel(&kernel, &dtb, base + 0x0700_0000)
        .expect("boot the interactive kernel");

    let mut ws = Workspace::attach(&mut emu);
    assert!(
        ws.run_until(b"HOLOSPACES-WORKSPACE-READY", 3_000_000_000),
        "the workspace boots to a ready terminal"
    );

    // Type each operator line; each advances the running holospace's κ snapshot.
    let lines: Vec<&str> = input.lines().filter(|l| !l.is_empty()).collect();
    let mut states = vec![ws.state_kappa()];
    for line in &lines {
        ws.type_line(line, 300_000_000);
        states.push(ws.state_kappa());
    }
    for w in states.windows(2) {
        assert_ne!(
            w[0], w[1],
            "typing a line drove (advanced) the holospace's κ snapshot"
        );
    }

    // Each input was published as a canonical event — the κ of its intent (L1/L2).
    assert_eq!(
        ws.channel().len(),
        lines.len(),
        "one event per operator line"
    );
    for (event, line) in ws.channel().iter().zip(&lines) {
        assert_eq!(
            *event,
            Intent::Type(String::from(*line)).kappa(),
            "the channel event is the canonical κ of the operator's intent"
        );
    }

    // The rendered terminal matches the reference RISC-V machine byte-for-byte.
    let term = String::from_utf8_lossy(ws.terminal()).replace('\r', "");
    let start = term
        .find("HOLOSPACES-WORKSPACE-READY")
        .expect("the terminal reached the ready banner");
    let done = "HOLOSPACES-WORKSPACE-DONE";
    let end = term.find(done).expect("the session completed") + done.len();
    let session = &term[start..end];
    assert_eq!(
        session.trim_end(),
        expected.trim_end(),
        "the workspace terminal is byte-identical to qemu-system-riscv64 (differential oracle)"
    );
}
