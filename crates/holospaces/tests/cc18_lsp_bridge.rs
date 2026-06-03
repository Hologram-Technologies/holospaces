//! `CC-18` (deployed delivery) + `CC-33`/ADR-020 — the workbench gets real
//! language intelligence from a language server **running in the devcontainer
//! OS**, reached over the **in-process substrate bridge**, with no Node.
//!
//! `CC-18` already proves the in-OS language server speaks the LSP spec over
//! stdio. This witnesses the *deployed* transport: the same `lsp-demo` runs as a
//! TCP service inside the booted devcontainer (`lsp-demo --listen`), and the
//! workbench (here, the host driving the wasm peer's `dial_guest`) speaks a full
//! LSP session to it over the loopback bridge (ADR-020) — the editor's language
//! intelligence flowing to a server in the OS, exactly the VS Code remote model
//! (ADR-015), but in the browser tab over the substrate.
//!
//! Authority: the **Language Server Protocol** spec, embodied by `lsp-types`
//! (rust-analyzer's own crate) — the server's responses are deserialized back
//! through `lsp-types` and checked for spec conformance, identically to `CC-18`;
//! the transport authority is TCP over the `CC-16`/`CC-21` NAT (`CC-33`).

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_with_files, Layer};
use holospaces::emulator::net::NoEgress;
use holospaces::emulator::Halt;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};
use lsp_types::{
    CompletionResponse, Hover, HoverContents, InitializeResult, Location, PublishDiagnosticsParams,
    ServerCapabilities,
};
use serde_json::{json, Value};

const SRC: &str = "fn greet(name) {\n  // TODO: greet\n  return greet(name)\n}\n";
const DOC_URI: &str = "file:///workspace/main.rs";
const LSP_PORT: u16 = 7000;

fn cc18_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc18")
}
fn cc16_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc16")
}
fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc18_dir().join("image/blobs/sha256").join(hex)).ok()
}
fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc18_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc18_dir().join("image/index.json")).unwrap();
    ingest_image(
        store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        blob_bytes,
    )
}
fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// The entry `/init`: bring up the OS and run the language server as a TCP
/// service (`lsp-demo --listen`). The kernel's `ip=dhcp` (the `devcontainer_net`
/// spec) configures the interface, so the server is reachable over the NAT.
fn bridge_init() -> Vec<u8> {
    let mut s = String::from("#!/bin/busybox sh\n");
    s.push_str("/bin/busybox mkdir -p /proc /sys /dev\n");
    s.push_str("/bin/busybox mount -t proc proc /proc\n");
    s.push_str("/bin/busybox mount -t sysfs sysfs /sys\n");
    s.push_str("/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null\n");
    s.push_str("export PATH=/bin:/usr/bin\n");
    s.push_str(&format!("/usr/bin/lsp-demo --listen {LSP_PORT}\n"));
    s.into_bytes()
}

fn req(id: i64, method: &str, params: Value) -> Vec<u8> {
    frame(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
}
fn notif(method: &str, params: Value) -> Vec<u8> {
    frame(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}
fn frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Parse all complete LSP base-protocol messages (`Content-Length` framed) out of
/// a byte buffer; returns the parsed JSON values and the count of bytes consumed.
fn parse_messages(buf: &[u8]) -> (Vec<Value>, usize) {
    let mut msgs = Vec::new();
    let mut pos = 0;
    while let Some(rel) = find(&buf[pos..], b"\r\n\r\n") {
        let hdr_end = pos + rel + 4;
        let header = String::from_utf8_lossy(&buf[pos..hdr_end]);
        let Some(len) = header.lines().find_map(|l| {
            l.strip_prefix("Content-Length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        }) else {
            break;
        };
        if hdr_end + len > buf.len() {
            break; // body not fully arrived yet
        }
        if let Ok(v) = serde_json::from_slice::<Value>(&buf[hdr_end..hdr_end + len]) {
            msgs.push(v);
        }
        pos = hdr_end + len;
    }
    (msgs, pos)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A real LSP session flows to the in-OS server over the in-process bridge and its
/// responses conform to the LSP spec. Heavy (a real-OS boot), so `#[ignore]`d.
#[test]
#[ignore]
fn the_in_os_language_server_serves_the_workbench_over_the_bridge() {
    // Assemble the CC-18 base (busybox + lsp-demo) with the TCP-service /init.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-18 image");
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
    let init = bridge_init();
    let files: Vec<(&str, u16, &[u8])> = vec![("init", 0o755, init.as_slice())];
    let rootfs = assemble_ext4_with_files(&layers, &files).expect("assemble the LSP rootfs");

    // The CC-16 net kernel (virtio-net) boots the rootfs with the loopback bridge.
    let kernel = gunzip(&cc16_dir().join("kernel/Image.gz"));
    let mut emu = MachineSpec::devcontainer_net()
        .boot_net(&kernel, rootfs, Box::new(NoEgress))
        .expect("boot the networked devcontainer");
    assert!(emu.enable_loopback(), "the loopback bridge attaches");

    // Run until the language server is listening.
    let mut listening = false;
    for _ in 0..600 {
        if !matches!(emu.run(5_000_000), Halt::OutOfBudget) {
            break;
        }
        if String::from_utf8_lossy(emu.console()).contains("LSP-LISTENING:7000") {
            listening = true;
            break;
        }
    }
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    assert!(
        listening,
        "the in-OS language server bound and listened on :7000; console:\n{console}"
    );

    // Dial the server over the in-process bridge and drive a full LSP session.
    let id = emu.dial_guest(LSP_PORT).expect("the bridge is enabled");
    for _ in 0..20 {
        emu.run(2_000_000);
    }

    let pos = |line: u32, ch: u32| json!({ "line": line, "character": ch });
    let td = || json!({ "uri": DOC_URI });
    let mut session = Vec::new();
    session.extend(req(
        1,
        "initialize",
        json!({ "capabilities": {}, "processId": null }),
    ));
    session.extend(notif("initialized", json!({})));
    session.extend(notif(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": DOC_URI, "languageId": "rust", "version": 1, "text": SRC } }),
    ));
    session.extend(req(
        2,
        "textDocument/hover",
        json!({ "textDocument": td(), "position": pos(0, 4) }),
    ));
    session.extend(req(
        3,
        "textDocument/completion",
        json!({ "textDocument": td(), "position": pos(2, 2) }),
    ));
    session.extend(req(
        4,
        "textDocument/definition",
        json!({ "textDocument": td(), "position": pos(2, 10) }),
    ));
    session.extend(req(5, "shutdown", Value::Null));
    session.extend(notif("exit", Value::Null));
    emu.guest_send(id, &session);

    // Drain the server's replies over the bridge until all responses arrive.
    let mut buf: Vec<u8> = Vec::new();
    let mut responses: Vec<Value> = Vec::new();
    for _ in 0..600 {
        emu.run(2_000_000);
        buf.extend(emu.guest_recv(id));
        let (msgs, _consumed) = parse_messages(&buf);
        responses = msgs;
        // initialize(1) + hover(2) + completion(3) + definition(4) + shutdown(5)
        // + the publishDiagnostics notification.
        let have_ids = |id: i64| {
            responses
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        };
        if have_ids(1) && have_ids(2) && have_ids(3) && have_ids(4) && have_ids(5) {
            break;
        }
    }
    assert!(
        !responses.is_empty(),
        "the in-OS language server replied over the bridge; console:\n{console}"
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
        "the server advertises hover, completion, and definition over the bridge"
    );

    // hover → a Hover naming the symbol under the cursor.
    let hover: Hover = serde_json::from_value(by_id(2).expect("hover response")["result"].clone())
        .expect("Hover conforms to the LSP spec");
    let hover_text = match hover.contents {
        HoverContents::Markup(m) => m.value,
        _ => String::new(),
    };
    assert!(
        hover_text.contains("greet"),
        "the hover describes `greet`: {hover_text}"
    );

    // completion → the document's identifiers.
    let completion: CompletionResponse =
        serde_json::from_value(by_id(3).expect("completion response")["result"].clone())
            .expect("CompletionResponse conforms to the LSP spec");
    let labels: Vec<String> = match completion {
        CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
        CompletionResponse::List(list) => list.items.into_iter().map(|i| i.label).collect(),
    };
    assert!(
        labels.iter().any(|l| l == "greet"),
        "completion offers `greet`: {labels:?}"
    );

    // definition → a Location at the definition (line 0).
    let location: Location =
        serde_json::from_value(by_id(4).expect("definition response")["result"].clone())
            .expect("definition Location conforms to the LSP spec");
    assert_eq!(
        location.range.start.line, 0,
        "definition points at `greet` (line 0)"
    );

    // diagnostics → a publishDiagnostics for the TODO (line 1).
    let diag_msg = responses
        .iter()
        .find(|v| {
            v.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        })
        .expect("publishDiagnostics over the bridge");
    let diags: PublishDiagnosticsParams = serde_json::from_value(diag_msg["params"].clone())
        .expect("PublishDiagnosticsParams conforms");
    assert!(
        diags.diagnostics.iter().any(|d| d.range.start.line == 1),
        "the server reports the TODO diagnostic (line 1) over the bridge"
    );

    // The session completing end-to-end (all responses validated above) is the
    // proof the bridge carried it. After the `exit` notification the server closes
    // the connection, so the bridge connection is now closed — a clean teardown.
    emu.guest_close(id);
}
