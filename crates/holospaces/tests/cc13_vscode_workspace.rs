//! `CC-13` — the VS Code workspace's editor + terminal components (arc42 ch.10;
//! ADR-010).
//!
//! Entering a holospace renders the Codespaces/Gitpod experience from the *real*
//! VS Code components — the Monaco editor and the xterm.js terminal. They are
//! imported into the workspace as κ-addressed content and **verified by
//! re-derivation through the substrate** before they load (Law L5; the σ-axis of
//! `CC-1`). This witnesses that the pinned components re-derive to their κ and
//! that a forged byte is refused — the gateway cannot lie. The browser IDE
//! (`crates/holospaces-web/web/workspace-test.mjs`) witnesses them rendering and
//! driving the running OS.

use std::path::{Path, PathBuf};

use holospaces::address;

fn cc13_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc13")
}

#[test]
fn the_vscode_components_re_derive_to_their_pinned_kappa() {
    let dir = cc13_dir();
    let manifest = std::fs::read_to_string(dir.join("vendor.kappa")).expect("cc13 vendor.kappa");

    let mut count = 0;
    for line in manifest.lines().filter(|l| !l.trim().is_empty()) {
        let (kappa, rel) = line.split_once("  ").expect("`<κ>  <path>` manifest line");
        let bytes = std::fs::read(dir.join(rel)).unwrap_or_else(|_| panic!("read {rel}"));

        // Re-derivation (Law L5): the bytes address to exactly the pinned κ.
        assert_eq!(
            address(&bytes).as_str(),
            kappa,
            "{rel} re-derives to its pinned κ"
        );

        // A forged byte is refused — the gateway cannot serve a different file.
        let mut forged = bytes.clone();
        forged[0] ^= 1;
        assert_ne!(
            address(&forged).as_str(),
            kappa,
            "a forged {rel} is refused (L5)"
        );
        count += 1;
    }
    assert!(
        count >= 8,
        "the pinned VS Code components are present ({count})"
    );
    assert!(
        manifest.contains("monaco/editor/editor.main.js"),
        "the real Monaco editor"
    );
    assert!(
        manifest.contains("xterm/xterm.js"),
        "the real xterm.js terminal"
    );
}
