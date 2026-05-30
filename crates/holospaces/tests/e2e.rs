//! End-to-end tests (the *e2e* tier).
//!
//! Whole operator flows over the **real** hologram substrate — no mocks: real
//! `KappaStore`, real `Runtime` + `WasmtimeEngine` executing a real
//! `hologram.*` Wasm container, real κ-addressing. Follows the runtime view
//! (arc42 chapter 6, `docs/src/arc42/adoc/06_runtime_view.adoc`) and the
//! concepts (chapter 8). CI runs this tier via
//! `cargo test --workspace --test e2e`.

use std::sync::Arc;

use hologram_net_http::live::{serve_addr, HttpKappaSync};
use hologram_runtime::Runtime;
use hologram_runtime_wasmtime::WasmtimeEngine;
use hologram_store_mem::MemKappaStore;
use holospaces::boot::{provision, Phase, Resolver, Session};
use holospaces::identity::Operator;
use holospaces::manager::Manager;
use holospaces::peer::Peer;
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
/// `hologram.*` Wasm container — the *Userland* compute form (the execution
/// surface, ADR-008): general/system code the runtime boots over the host ABI.
/// A userland is κ-addressed content; the Boot Layer and lifecycle are identical
/// however it was authored.
fn provision_container(store: &MemKappaStore) -> Holospace {
    let code = store
        .put("blake3", &wat::parse_str(CONTAINER_WAT).unwrap())
        .unwrap();
    provision(store, Source::Userland { entry: code }, caps()).expect("provision into store")
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
        .put("blake3", b"a .holo tensor-graph artifact")
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
    let holospace = provision(&store, Source::Userland { entry: code }, caps()).unwrap();
    assert!(store.contains(holospace.manifest()));
}

/// Whole operator-console flow across two peers: the Platform Manager
/// provisions and boots a holospace on instance A, then on instance B (same
/// operator) synchronises the operator's roster + holospaces over a real
/// loopback HTTP-CAS gateway (verify-on-receipt, Law L5) and boots the migrated
/// holospace (R5/QS5, QS2). Realizes arc42 ch.5 (Platform Manager, Peer),
/// ch.7 (deployment), and ch.8 (Identity and sync).
#[test]
fn operator_console_provisions_on_a_then_syncs_and_boots_on_b() {
    pollster::block_on(async {
        let operator = Operator::from_public_key(b"operator-self-sovereign-key");

        // Instance A: provision + boot through the Manager over a real peer.
        let runtime_a = Runtime::new(WasmtimeEngine::new(), MemKappaStore::new());
        let code = runtime_a
            .store()
            .put("blake3", &wat::parse_str(CONTAINER_WAT).unwrap())
            .unwrap();
        let peer_a = Peer::new(runtime_a.store(), &runtime_a);
        let mut manager_a = Manager::sign_in(peer_a, operator.clone());
        let holospace = manager_a
            .provision(Source::Userland { entry: code }, caps())
            .expect("provision");
        assert_eq!(manager_a.view().holospaces, vec![holospace]);

        let mut session = manager_a.open(&holospace).await.expect("open");
        session.boot().await.expect("boot on A");
        assert_eq!(session.phase(), Phase::Running);
        session.suspend().await.expect("suspend on A");
        session.terminate().await.expect("terminate on A");
        drop(session);
        let roster = manager_a.roster().kappa();

        // Serve A's store as an untrusted content-addressed gateway.
        let gateway: Arc<dyn KappaStore> = runtime_a.store_arc();
        let server = serve_addr(gateway, "127.0.0.1:0", false).expect("serve HTTP-CAS");

        // Instance B: sign in, sync from A's roster, boot the synced holospace.
        let runtime_b = Runtime::new(WasmtimeEngine::new(), MemKappaStore::new());
        let sync = HttpKappaSync::new(vec![server.addr().to_string()]);
        let peer_b = Peer::new(runtime_b.store(), &runtime_b).with_sync(&sync);
        let mut manager_b = Manager::sign_in(peer_b, operator);
        assert!(manager_b.view().holospaces.is_empty());

        let synced = manager_b.sync_from(&roster).await.expect("sync from A");
        assert_eq!(synced, 1);
        assert_eq!(manager_b.view().holospaces, vec![holospace]);

        let mut session_b = manager_b.open(&holospace).await.expect("open on B");
        session_b
            .boot()
            .await
            .expect("boot on B (migrated content)");
        assert_eq!(session_b.phase(), Phase::Running);
        session_b.terminate().await.unwrap();
    });
}

/// A devcontainer holospace boots (ADR-008; `CC-4` + `CC-6`): its config selects
/// a κ-addressed Wasm userland, which the runtime spawns. This is the resolved
/// RT1 surface end-to-end — the devcontainer path produces a *bootable*
/// holospace, not a config hash.
#[test]
fn operator_boots_a_devcontainer_holospace() {
    pollster::block_on(async {
        let store = MemKappaStore::new();
        // The recompiled userland the devcontainer's config selects.
        let userland = store
            .put("blake3", &wat::parse_str(CONTAINER_WAT).unwrap())
            .unwrap();
        let config = holospaces::address(br#"{"name":"app","image":"debian:12"}"#);
        let holospace = provision(
            &store,
            Source::Devcontainer {
                repo: "https://example.invalid/app.git".to_string(),
                reference: "main".to_string(),
                config_path: ".devcontainer/devcontainer.json".to_string(),
                config,
                userland,
            },
            caps(),
        )
        .expect("provision the devcontainer holospace");
        // Its Container ID's code is the userland — bootable, not the config hash.
        assert_eq!(holospace.container_manifest().code, userland);

        let runtime = Runtime::new(WasmtimeEngine::new(), store);
        let mut session = Session::provision(&runtime, holospace);
        session
            .boot()
            .await
            .expect("boot the devcontainer userland");
        assert_eq!(session.phase(), Phase::Running);
        session.suspend().await.expect("suspend");
        session.resume().await.expect("resume");
        session.terminate().await.expect("terminate");
    });
}

/// A peer with no network pillar resolves only locally; a holospace it never
/// provisioned is absent (no forging — Laws L1/L5).
#[test]
fn offline_peer_resolves_locally_only() {
    pollster::block_on(async {
        let runtime = Runtime::new(WasmtimeEngine::new(), MemKappaStore::new());
        let peer = Peer::new(runtime.store(), &runtime);
        let absent = holospaces::address(b"a holospace this peer never provisioned");
        assert!(peer.resolve(&absent).await.unwrap().is_none());
    });
}
