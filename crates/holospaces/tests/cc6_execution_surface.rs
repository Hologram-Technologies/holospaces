//! `CC-6` — the execution surface (arc42 chapter 10, Conformance catalog;
//! ADR-008, resolving RT1).
//!
//! Witnesses that a *Wasm-recompiled userland* — the second compute form — is a
//! first-class, validated, bootable holospace form on the **real** substrate,
//! with no mocks. Two external authorities:
//!
//! * the [WebAssembly](https://webassembly.org) specification — the userland is
//!   spec-valid and binds only the substrate host ABI (`surface::validate_userland`);
//! * the [hologram](https://github.com/Hologram-Technologies/hologram)
//!   `ContainerRuntime` contract — for a trivial (no-op) userland, the real
//!   `Runtime` drives the lifecycle state machine (boot → suspend → κ-snapshot →
//!   resume → adopt-on-peer-B), over **both** the native `WasmtimeEngine` (JIT)
//!   and the `wasmi` `BareMetalEngine` interpreter (the browser + bare-metal
//!   `ContainerEngine`), each producing a content-addressed `blake3:` snapshot
//!   label and yielding the same provisioning κ (Q6). These tests exercise the
//!   lifecycle transitions and snapshot labelling only; they do not assert any
//!   observable computation or post-resume state continuity.
//!
//! This is the resolved RT1 surface: a κ-addressed userland over the host ABI,
//! not a located OCI image (Laws L1/L2/L4).

use hologram_runtime::Runtime;
use hologram_runtime_bare::BareMetalEngine;
use hologram_runtime_wasmtime::WasmtimeEngine;
use hologram_store_mem::MemKappaStore;
use holospaces::boot::{provision, Phase, Session};
use holospaces::substrate::{Capabilities, KappaStore};
use holospaces::surface::{self, SurfaceError};
use holospaces::wasm::WasmError;
use holospaces::Source;

/// A trivial (no-op) userland: every entry point returns `(i32.const 0)`. It
/// presents the full container ABI and imports nothing outside the `hologram`
/// host surface, so the real engines can instantiate, suspend, and resume it —
/// but it computes nothing observable.
const USERLAND_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_event")    (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend")  (result i32) (i32.const 0))
  (func (export "hg_resume")   (result i32) (i32.const 0))
  (func (export "hg_callback") (param i32 i32 i32) (result i32) (i32.const 0)))
"#;

/// A userland that *binds the host ABI* (imports a `hologram` host function) —
/// allowed by the surface contract (the syscall boundary, ADR-008).
const HOST_BINDING_WAT: &str = r#"
(module
  (import "hologram" "hg_syscall" (func $sys (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_event")    (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend")  (result i32) (i32.const 0))
  (func (export "hg_resume")   (result i32) (i32.const 0))
  (func (export "hg_callback") (param i32 i32 i32) (result i32) (i32.const 0)))
"#;

/// A module that reaches for an *ambient* host (WASI-style `env`) — refused: the
/// substrate host surface is closed (Law L1 — no escape hatch).
const AMBIENT_IMPORT_WAT: &str = r#"
(module
  (import "env" "write" (func $w (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_event")    (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend")  (result i32) (i32.const 0))
  (func (export "hg_resume")   (result i32) (i32.const 0))
  (func (export "hg_callback") (param i32 i32 i32) (result i32) (i32.const 0)))
"#;

/// A module missing a container-ABI entry point (`hg_event`) — not drivable by
/// the runtime, so the surface refuses it before it can be a holospace's code.
const INCOMPLETE_ABI_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend")  (result i32) (i32.const 0))
  (func (export "hg_resume")   (result i32) (i32.const 0))
  (func (export "hg_callback") (param i32 i32 i32) (result i32) (i32.const 0)))
"#;

fn caps() -> Capabilities {
    Capabilities {
        storage_roots: Vec::new(),
        storage_quota_bytes: 0,
        network_fetch: false,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: 4 << 20,
        cpu_time_per_event_ms: 1000,
        priority_weight: 0,
    }
}

/// The surface validator enforces the execution-surface contract against the
/// WebAssembly specification: spec-valid, host-ABI-only imports, full container
/// ABI present. (CC-6, validity authority.)
#[test]
fn surface_validator_enforces_the_contract() {
    let userland = wat::parse_str(USERLAND_WAT).unwrap();
    surface::validate_userland(&userland).expect("a complete, host-ABI-only userland is valid");

    // Binding the substrate host ABI (the syscall boundary) is allowed.
    let host_binding = wat::parse_str(HOST_BINDING_WAT).unwrap();
    surface::validate_userland(&host_binding).expect("binding the `hologram` host ABI is allowed");

    // An ambient (WASI-style) import is refused — the host surface is closed.
    let ambient = wat::parse_str(AMBIENT_IMPORT_WAT).unwrap();
    assert!(matches!(
        surface::validate_userland(&ambient),
        Err(SurfaceError::Wasm(WasmError::ForbiddenImport { .. }))
    ));

    // A userland that does not present the full container ABI is refused.
    let incomplete = wat::parse_str(INCOMPLETE_ABI_WAT).unwrap();
    assert_eq!(
        surface::validate_userland(&incomplete),
        Err(SurfaceError::MissingAbiExport("hg_event"))
    );
}

/// For a trivial (no-op) κ-addressed userland, the **real** substrate runtime
/// drives the lifecycle state machine (boot → suspend → κ-snapshot → resume →
/// adopt-on-peer-B) over the native Wasmtime engine. Each suspend yields a
/// content-addressed `blake3:` snapshot label. This asserts the lifecycle
/// transitions and snapshot labelling only — it does NOT assert any observable
/// computation, nor any post-resume state continuity distinguishing an adopted
/// run from a fresh boot. (CC-6, runtime-contract authority.)
#[test]
fn the_lifecycle_state_machine_drives_on_the_real_runtime() {
    pollster::block_on(async {
        let module = wat::parse_str(USERLAND_WAT).unwrap();
        surface::validate_userland(&module).expect("surface-valid");

        // Provision the userland holospace (the second compute form, ADR-008).
        let store_a = MemKappaStore::new();
        let code = store_a.put("blake3", &module).unwrap();
        let holospace = provision(&store_a, Source::Userland { entry: code }, caps())
            .expect("provision userland into store");
        assert_eq!(holospace.source(), &Source::Userland { entry: code });

        // The lifecycle drives on the real Wasmtime-backed runtime: boot reaches
        // the Running phase (no observable compute is asserted).
        let runtime_a = Runtime::new(WasmtimeEngine::new(), store_a);
        let mut a = Session::provision(&runtime_a, holospace.clone());
        a.boot()
            .await
            .expect("boot transitions on the real runtime");
        assert_eq!(a.phase(), Phase::Running);

        // Suspend → a content-addressed `blake3:` snapshot label; resume returns
        // to Running (no post-resume state continuity is asserted).
        let snapshot = a.suspend().await.expect("suspend transition");
        assert!(
            snapshot.as_str().starts_with("blake3:"),
            "the snapshot is a content-addressed blake3: label"
        );
        a.resume().await.expect("resume transition");
        assert_eq!(a.phase(), Phase::Running);
        let snapshot = a.suspend().await.expect("re-suspend transition");

        // Adopt on peer B (QS2): ship the reachable bytes to instance B and drive
        // resume there. Because the userland and its snapshot label are content,
        // nothing else transfers; this asserts the adopt → resume transition only,
        // not that B continues A's state.
        let store_b = MemKappaStore::new();
        for k in runtime_a.store().iterate() {
            let v = runtime_a.store().get(&k).unwrap().unwrap();
            store_b.put("blake3", v.as_ref()).unwrap();
        }
        let runtime_b = Runtime::new(WasmtimeEngine::new(), store_b);
        let mut b = Session::adopt(&runtime_b, holospace, snapshot);
        b.resume().await.expect("adopt-on-peer-B resume transition");
        assert_eq!(b.phase(), Phase::Running);
        b.terminate().await.expect("terminate transition");
    });
}

/// The **same** userland κ provisions identically on a *different* environment
/// engine — the `wasmi` interpreter `ContainerEngine` (`hologram-runtime-bare`),
/// the engine the browser and bare-metal peers run (it is `no_std` + pure-Rust,
/// so it compiles to wasm32 and to bare-metal where a JIT cannot) — and the
/// lifecycle then drives on it. The provisioning κ is identical across engines,
/// witnessing Q6 (the same holospace κ on any peer) across heterogeneous
/// engines, not just the native Wasmtime one. For the trivial (no-op) module the
/// lifecycle transitions and `blake3:` snapshot labelling are asserted only — no
/// observable computation. (CC-6, cross-environment execution surface.)
#[test]
fn the_lifecycle_drives_on_the_interpreter_engine() {
    pollster::block_on(async {
        let module = wat::parse_str(USERLAND_WAT).unwrap();
        surface::validate_userland(&module).expect("surface-valid");

        let store = MemKappaStore::new();
        let code = store.put("blake3", &module).unwrap();
        let holospace =
            provision(&store, Source::Userland { entry: code }, caps()).expect("provision");

        // The κ is identical to the one a native Wasmtime peer computes — the
        // holospace is the engine-agnostic content; only the peer's engine differs.
        let native_store = MemKappaStore::new();
        native_store.put("blake3", &module).unwrap();
        let native = provision(&native_store, Source::Userland { entry: code }, caps()).unwrap();
        assert_eq!(holospace.kappa(), native.kappa(), "same κ on any peer (Q6)");

        // The lifecycle drives on the interpreter engine — the real
        // browser/bare-metal execution surface, no JIT, no host — through
        // boot → suspend → resume → terminate (no observable compute asserted).
        let runtime = Runtime::new(BareMetalEngine::new(), store);
        let mut s = Session::provision(&runtime, holospace);
        s.boot()
            .await
            .expect("boot transition on the interpreter engine");
        assert_eq!(s.phase(), Phase::Running);
        let snapshot = s.suspend().await.expect("suspend transition (interpreter)");
        assert!(
            snapshot.as_str().starts_with("blake3:"),
            "the snapshot is a content-addressed blake3: label"
        );
        s.resume().await.expect("resume transition (interpreter)");
        assert_eq!(s.phase(), Phase::Running);
        s.terminate().await.expect("terminate transition");
    });
}

/// The same userland κ yields the same holospace on any peer (Q6/QS1): the
/// execution surface is reproducible, identity is content not location (L1).
#[test]
fn the_userland_holospace_is_reproducible() {
    let module = wat::parse_str(USERLAND_WAT).unwrap();
    let store = MemKappaStore::new();
    let code = store.put("blake3", &module).unwrap();
    let a = provision(&store, Source::Userland { entry: code }, caps()).unwrap();
    let b = provision(&store, Source::Userland { entry: code }, caps()).unwrap();
    assert_eq!(a.kappa(), b.kappa(), "same userland κ ⇒ same holospace κ");
}
