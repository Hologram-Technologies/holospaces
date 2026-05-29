//! End-to-end tests (the *e2e* tier).
//!
//! Whole operator flows over the **real** hologram substrate — no mocks: real
//! `KappaStore`, real `Runtime` + `WasmtimeEngine` executing a real
//! `hologram.*` Wasm container, real κ-addressing. Follows the runtime view
//! (arc42 chapter 6, `docs/src/arc42/adoc/06_runtime_view.adoc`) and the
//! concepts (chapter 8). CI runs this tier via
//! `cargo test --workspace --test e2e`.

use hologram_runtime::Runtime;
use hologram_runtime_wasmtime::WasmtimeEngine;
use hologram_store_mem::MemKappaStore;
use holospaces::boot::{provision, Phase, Resolver, Session};
use holospaces::identity::Operator;
use holospaces::substrate::{Capabilities, KappaStore, Realization};
use holospaces::{Holospace, Source};

/// A minimal real `hologram.*` Wasm container: it exports the container ABI
/// (`hg_init`/`hg_event`/`hg_suspend`/`hg_resume`/`hg_callback`) and imports
/// nothing outside the `hologram` host surface, so it is substrate-valid
/// (CC-5) and the real Wasmtime engine can instantiate, suspend, and resume it.
const CONTAINER_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_event")    (param i32 i32) (result i32) (i32.const 0))
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

/// Provision a Wasm-container holospace into a store. The code module is a real
/// `hologram.*` Wasm container; how a devcontainer is *built* into such a
/// module is the open Linux-surface decision (arc42 chapter 11, RT1) — the Boot
/// Layer and lifecycle are identical regardless of how the code κ is produced.
fn provision_container(store: &MemKappaStore) -> Holospace {
    let code = store
        .put("blake3", &wat::parse_str(CONTAINER_WAT).unwrap())
        .unwrap();
    provision(store, Source::HoloFile { artifact: code }, caps()).expect("provision into store")
}

/// The full operator flow over the real runtime: sign in, provision into the
/// store, boot a real container, resolve+verify the definition (L5), suspend to
/// a κ snapshot, migrate to a second instance, resume, terminate.
#[test]
fn operator_boots_suspends_and_migrates_a_real_container() {
    pollster::block_on(async {
        // Sign in: unlock a self-sovereign key → a content-addressed identity.
        let operator = Operator::from_public_key(b"operator-ed25519-public-key");
        assert!(operator.identity().as_str().starts_with("blake3:"));

        // Instance A: provision into the store, then wire the real runtime.
        let store_a = MemKappaStore::new();
        let holospace = provision_container(&store_a);
        let identity = holospace.kappa();

        // The definition resolves and verifies by re-derivation (L5).
        let bytes = Resolver::resolve_local(&store_a, &identity)
            .unwrap()
            .unwrap();
        assert_eq!(Holospace::references(&bytes).unwrap().len(), 2);

        let runtime_a = Runtime::new(WasmtimeEngine::new(), store_a);
        let mut a = Session::provision(&runtime_a, holospace.clone());
        a.boot().await.expect("real Wasmtime spawn");
        assert_eq!(a.phase(), Phase::Running);

        // Suspend → a real κ snapshot of the container's state.
        let snapshot = a.suspend().await.expect("real suspend");
        assert_eq!(a.phase(), Phase::Suspended);
        assert!(snapshot.as_str().starts_with("blake3:"));

        // Migrate (QS2): ship the reachable bytes to instance B and resume from
        // the snapshot κ — because state is content, nothing else need transfer.
        let store_b = MemKappaStore::new();
        for k in runtime_a.store().iterate() {
            let v = runtime_a.store().get(&k).unwrap().unwrap();
            store_b.put("blake3", v.as_ref()).unwrap();
        }
        let runtime_b = Runtime::new(WasmtimeEngine::new(), store_b);
        let mut b = Session::adopt(&runtime_b, holospace, snapshot);
        b.resume().await.expect("real resume on instance B");
        assert_eq!(b.phase(), Phase::Running);
        b.terminate().await.expect("real terminate");
        assert_eq!(b.phase(), Phase::Terminated);
    });
}

/// Invalid lifecycle transitions are rejected by the Boot Layer's guards before
/// they reach the substrate runtime.
#[test]
fn invalid_transitions_are_rejected_against_the_real_runtime() {
    pollster::block_on(async {
        let store = MemKappaStore::new();
        let holospace = provision_container(&store);
        let runtime = Runtime::new(WasmtimeEngine::new(), store);

        let mut s = Session::provision(&runtime, holospace);
        assert!(s.resume().await.is_err(), "cannot resume what never ran");
        s.boot().await.unwrap();
        s.terminate().await.unwrap();
        assert!(
            s.boot().await.is_err(),
            "cannot boot a terminated holospace"
        );
    });
}

/// The holo-file provisioning path (ADR-004): a `.holo` artifact referenced by
/// its κ becomes a holospace whose identity is reproducible and resolvable.
/// (Tensor `.holo` execution is witnessed by `CC-2`; here the operator flow is
/// provision + resolve.)
#[test]
fn operator_provisions_a_holo_file_holospace() {
    let store = MemKappaStore::new();
    let artifact = store
        .put(
            "blake3",
            b"a .holo tensor-graph artifact (compiled upstream)",
        )
        .unwrap();
    let holospace = provision(&store, Source::HoloFile { artifact }, caps()).expect("provision");
    let identity = holospace.kappa();
    assert_eq!(
        Resolver::resolve_local(&store, &identity)
            .unwrap()
            .unwrap()
            .as_ref(),
        holospace.canonicalize().as_slice()
    );
}

/// A Wasm-code holospace's code module is validated against the WebAssembly
/// spec and the substrate's closed host surface (CC-5) before provisioning.
#[test]
fn operator_validates_a_wasm_code_module_before_provisioning() {
    let module = wat::parse_str(CONTAINER_WAT).unwrap();
    holospaces::wasm::validate_substrate_module(&module).expect("module is substrate-valid");

    let store = MemKappaStore::new();
    let code = store.put("blake3", &module).unwrap();
    let holospace = provision(&store, Source::HoloFile { artifact: code }, caps()).unwrap();
    assert!(store.contains(holospace.manifest()));
}
