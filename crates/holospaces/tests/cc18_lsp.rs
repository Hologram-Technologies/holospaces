//! `CC-18` — the workbench provides language intelligence: a real language server
//! runs in the devcontainer OS and the workbench speaks LSP to it.
//!
//! holospaces runs a real Language Server (`lsp-demo`, built on `lsp-types` —
//! rust-analyzer's own LSP type crate, the spec's authoritative Rust embodiment)
//! inside the devcontainer OS (`CC-14`). The workbench's session — `initialize`,
//! `textDocument/didOpen`, `hover`, `completion`, `definition`, `shutdown` —
//! flows to the server over the standard LSP stdio base-protocol transport, and
//! the server's responses (capabilities, a hover, completions, a definition
//! location, and diagnostics for a real source file) **conform to the LSP spec**.
//!
//! The external authority is the **Language Server Protocol specification** (via
//! `lsp-types`, which the workbench and this witness both speak) and a **real
//! language server** running in the OS. The session is built with `lsp-types`;
//! the server's responses are deserialized back through `lsp-types` and checked
//! for spec conformance. The OS *executing* the server under a real libc is
//! witnessed on holospaces' own emulator (the same substrate the holospace boots
//! on); a deterministic check verifies the server binary + session are present in
//! the assembled rootfs (e2fsprogs).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_with_files, Layer};
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};
use lsp_types::{
    CompletionResponse, DidOpenTextDocumentParams, Hover, HoverContents, InitializeResult, Location,
    PublishDiagnosticsParams, ServerCapabilities,
};
use serde_json::{json, Value};

// The source file the workbench opens — an identifier `greet` (defined on line 0,
// used on line 2) and a `TODO` marker (line 1) for diagnostics.
const SRC: &str = "fn greet(name) {\n  // TODO: greet\n  return greet(name)\n}\n";
const DOC_URI: &str = "file:///workspace/main.rs";

fn cc18_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc18")
}
fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc18_dir().join("image/blobs/sha256").join(hex)).ok()
}
fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc18_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc18_dir().join("image/index.json")).unwrap();
    ingest_image(store, &layout, &index, blob_bytes)
}
fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

fn req(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}
fn notif(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// The LSP session the workbench sends, built with `lsp-types` params where they
/// add authority, framed by the LSP base protocol (`Content-Length`).
fn lsp_session() -> Vec<u8> {
    let pos = |line: u32, ch: u32| json!({ "line": line, "character": ch });
    let td = || json!({ "uri": DOC_URI });
    let did_open: DidOpenTextDocumentParams = serde_json::from_value(json!({
        "textDocument": { "uri": DOC_URI, "languageId": "rust", "version": 1, "text": SRC }
    }))
    .unwrap();

    let messages = vec![
        req(1, "initialize", json!({ "capabilities": {}, "processId": null })),
        notif("initialized", json!({})),
        notif(
            "textDocument/didOpen",
            serde_json::to_value(&did_open).unwrap(),
        ),
        // hover over `greet` at its definition (line 0).
        req(
            2,
            "textDocument/hover",
            json!({ "textDocument": td(), "position": pos(0, 4) }),
        ),
        req(
            3,
            "textDocument/completion",
            json!({ "textDocument": td(), "position": pos(2, 2) }),
        ),
        // go-to-definition from the use of `greet` (line 2).
        req(
            4,
            "textDocument/definition",
            json!({ "textDocument": td(), "position": pos(2, 10) }),
        ),
        req(5, "shutdown", Value::Null),
        notif("exit", Value::Null),
    ];

    let mut out = Vec::new();
    for m in &messages {
        let body = serde_json::to_vec(m).unwrap();
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        out.extend_from_slice(&body);
    }
    out
}

/// The entry `/init`: run the in-OS language server over the injected session
/// (stdin from `/session.lsp`, responses to the console between sentinels), then
/// power off.
fn lsp_init() -> Vec<u8> {
    let mut s = String::from("#!/bin/busybox sh\n");
    s.push_str("export PATH=/bin:/usr/bin\n");
    s.push_str("echo LSP-BEGIN\n");
    s.push_str("/usr/bin/lsp-demo < /session.lsp\n");
    s.push_str("echo\n");
    s.push_str("echo LSP-END\n");
    s.push_str("busybox reboot -f\n");
    s.into_bytes()
}

/// Assemble the devcontainer rootfs: the busybox + `lsp-demo` base, with the
/// entry `/init` and the LSP `/session.lsp` injected.
fn assemble(store: &MemKappaStore) -> Vec<u8> {
    let img = ingest(store).expect("ingest the CC-18 image");
    let owned: Vec<(String, Vec<u8>)> = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    let layers: Vec<Layer> = owned
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    let init = lsp_init();
    let session = lsp_session();
    let files: Vec<(&str, u16, &[u8])> = vec![
        ("init", 0o755, init.as_slice()),
        ("session.lsp", 0o644, session.as_slice()),
    ];
    assemble_ext4_with_files(&layers, &files).expect("assemble the LSP rootfs")
}

/// (1) The session the workbench sends is **spec-valid** (its params deserialize
/// through `lsp-types`), and the real language server binary + the session are
/// present in the assembled rootfs the OS boots — verified against the **ext4**
/// format by e2fsprogs.
#[test]
fn the_language_server_and_session_are_in_the_assembled_rootfs() {
    // The session is spec-shaped: didOpen params round-trip through lsp-types.
    let session = lsp_session();
    assert!(
        extract_json_objects(&session)
            .iter()
            .any(|v| v.get("method").and_then(Value::as_str) == Some("textDocument/didOpen")),
        "the session opens a document"
    );

    if !have("e2fsck") || !have("debugfs") {
        eprintln!("SKIP: e2fsprogs (e2fsck/debugfs) not available");
        return;
    }
    let store = MemKappaStore::new();
    let rootfs = assemble(&store);
    let img = std::env::temp_dir().join(format!("cc18-rootfs-{}.img", std::process::id()));
    std::fs::write(&img, &rootfs).unwrap();

    let fsck = Command::new("e2fsck")
        .args(["-fn", img.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        fsck.status.code() == Some(0),
        "e2fsck must find the assembled ext4 clean:\n{}",
        String::from_utf8_lossy(&fsck.stdout)
    );

    // The real language server binary (a multi-extent ~1 MB ELF) reads back
    // byte-identically, and the LSP session is present — the rootfs the OS boots.
    let bin = debugfs_cat(&img, "/usr/bin/lsp-demo");
    assert!(
        bin.starts_with(b"\x7fELF") && bin.len() > 100_000,
        "the real language server binary is in the rootfs ({} bytes)",
        bin.len()
    );
    let got_session = debugfs_cat(&img, "/session.lsp");
    let _ = std::fs::remove_file(&img);
    assert_eq!(
        got_session, session,
        "the LSP session reads back byte-identically from /session.lsp"
    );
}

/// (2) holospaces' **own emulator** runs the real language server in the OS and it
/// **speaks LSP**: the emulator boots the assembled rootfs, execs `lsp-demo` over
/// the injected session, and its responses — deserialized back through `lsp-types`
/// — conform to the spec: advertised hover/completion/definition capabilities, a
/// hover for the symbol, completions, a go-to-definition `Location`, and a `TODO`
/// diagnostic for the real source file. Heavy (a real-OS boot to userland), so
/// `#[ignore]`d.
#[test]
#[ignore]
fn the_in_os_language_server_speaks_lsp() {
    let store = MemKappaStore::new();
    let rootfs = assemble(&store);

    let kernel_gz =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14/kernel/Image.gz");
    let kernel = {
        let raw = std::fs::read(&kernel_gz).unwrap();
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut k = Vec::new();
        d.read_to_end(&mut k).unwrap();
        k
    };

    let mut emu = MachineSpec::devcontainer()
        .boot(&kernel, rootfs)
        .expect("boot the holospaces emulator");
    emu.run(2_000_000_000);
    let console = emu.console().to_vec();

    // Extract the server's responses (the JSON objects between the sentinels).
    let begin = find(&console, b"LSP-BEGIN").expect("LSP-BEGIN on the console");
    let end = find(&console[begin..], b"LSP-END")
        .map(|e| begin + e)
        .unwrap_or(console.len());
    let responses = extract_json_objects(&console[begin..end]);
    assert!(
        !responses.is_empty(),
        "the in-OS language server produced LSP responses; console:\n{}",
        String::from_utf8_lossy(&console)
    );

    let by_id = |id: i64| {
        responses
            .iter()
            .find(|v| v.get("id").and_then(Value::as_i64) == Some(id))
    };

    // initialize → ServerCapabilities advertise hover/completion/definition.
    let init_result: InitializeResult =
        serde_json::from_value(by_id(1).expect("initialize response")["result"].clone())
            .expect("InitializeResult conforms to the LSP spec");
    let caps: ServerCapabilities = init_result.capabilities;
    assert!(
        caps.hover_provider.is_some()
            && caps.completion_provider.is_some()
            && caps.definition_provider.is_some(),
        "the server advertises hover, completion, and definition (LSP capabilities)"
    );

    // hover → a Hover whose markup names the symbol under the cursor.
    let hover: Hover =
        serde_json::from_value(by_id(2).expect("hover response")["result"].clone())
            .expect("Hover conforms to the LSP spec");
    let hover_text = match hover.contents {
        HoverContents::Markup(m) => m.value,
        _ => String::new(),
    };
    assert!(
        hover_text.contains("greet"),
        "the hover describes the symbol `greet`: {hover_text}"
    );

    // completion → a real list including the document's identifiers.
    let completion: CompletionResponse =
        serde_json::from_value(by_id(3).expect("completion response")["result"].clone())
            .expect("CompletionResponse conforms to the LSP spec");
    let labels: Vec<String> = match completion {
        CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
        CompletionResponse::List(list) => list.items.into_iter().map(|i| i.label).collect(),
    };
    assert!(
        labels.iter().any(|l| l == "greet"),
        "completion offers the document's identifiers: {labels:?}"
    );

    // definition → a Location at `greet`'s definition (line 0).
    let location: Location =
        serde_json::from_value(by_id(4).expect("definition response")["result"].clone())
            .expect("definition Location conforms to the LSP spec");
    assert_eq!(
        location.range.start.line, 0,
        "go-to-definition points at the definition of `greet` (line 0)"
    );

    // diagnostics → a publishDiagnostics for the TODO on line 1.
    let diag_msg = responses.iter().find(|v| {
        v.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
    });
    let diags: PublishDiagnosticsParams =
        serde_json::from_value(diag_msg.expect("publishDiagnostics")["params"].clone())
            .expect("PublishDiagnosticsParams conforms to the LSP spec");
    assert!(
        diags.diagnostics.iter().any(|d| d.range.start.line == 1),
        "the server reports the TODO diagnostic on line 1"
    );
}

/// `debugfs -R "cat <path>"` — read a file out of the ext4 image.
fn debugfs_cat(img: &Path, path: &str) -> Vec<u8> {
    let mut child = Command::new("debugfs")
        .args(["-R", &format!("cat {path}"), img.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.wait().unwrap();
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract every top-level JSON object from `bytes` (brace-matched, string-aware)
/// — robust to the LSP `Content-Length` headers and any console newline handling
/// between the message bodies.
fn extract_json_objects(bytes: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        let mut j = i;
        while j < bytes.len() {
            let c = bytes[j];
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        if depth == 0 && j < bytes.len() {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes[i..=j]) {
                out.push(v);
            }
            i = j + 1;
        } else {
            break;
        }
    }
    out
}
