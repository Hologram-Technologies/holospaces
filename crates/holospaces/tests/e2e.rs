//! End-to-end tests (the *e2e* tier).
//!
//! E2E tests exercise whole operator flows — sign-in, provisioning and booting
//! a holospace, its lifecycle across instances, and resolution with
//! re-derivation — over the real hologram substrate (`KappaStore`, the
//! `ContainerRuntime` surface, real κ-addressing). They follow the conceptual
//! model of arc42 chapters 6 and 8 and the quality scenarios of chapter 10.
//! Native flows run here; the browser flow (the Hologram Platform Manager on
//! GitHub Pages) is the browser peer's Playwright harness. CI runs this tier
//! via `cargo test --workspace --test e2e`.

use core::sync::atomic::{AtomicU64, Ordering};

use hologram_store_mem::MemKappaStore;
use holospaces::boot::{ingest_devcontainer, Phase, Resolver, Session};
use holospaces::identity::Operator;
use holospaces::substrate::{
    Capabilities, ContainerHandle, ContainerInfo, ContainerRuntime, KappaStore, Realization,
};
use holospaces::{Holospace, Kappa, Source};

fn caps() -> Capabilities {
    Capabilities {
        storage_roots: Vec::new(),
        storage_quota_bytes: 0,
        network_fetch: false,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: 2 << 30,
        cpu_time_per_event_ms: 0,
        priority_weight: 0,
    }
}

/// A minimal in-test `ContainerRuntime`: the one substrate piece a native e2e
/// cannot stand up without a container code artifact. Everything else —
/// identity, ingest, store, κ, resolution — is the real substrate.
#[derive(Default)]
struct MockRuntime {
    next: AtomicU64,
}

#[async_trait::async_trait]
impl ContainerRuntime for MockRuntime {
    async fn spawn(
        &self,
        _id: &Kappa,
        _caps: &Kappa,
    ) -> Result<ContainerHandle, hologram_substrate_core::RuntimeError> {
        Ok(ContainerHandle(self.next.fetch_add(1, Ordering::SeqCst)))
    }
    async fn suspend(
        &self,
        _h: ContainerHandle,
    ) -> Result<Kappa, hologram_substrate_core::RuntimeError> {
        Ok(holospaces::address(b"running-state-snapshot"))
    }
    async fn resume(
        &self,
        _s: &Kappa,
        _c: &Kappa,
    ) -> Result<ContainerHandle, hologram_substrate_core::RuntimeError> {
        Ok(ContainerHandle(self.next.fetch_add(1, Ordering::SeqCst)))
    }
    async fn terminate(
        &self,
        _h: ContainerHandle,
    ) -> Result<(), hologram_substrate_core::RuntimeError> {
        Ok(())
    }
    fn list(&self) -> Vec<ContainerHandle> {
        Vec::new()
    }
    fn info(&self, _h: ContainerHandle) -> Option<ContainerInfo> {
        None
    }
}

/// The full operator flow for a devcontainer holospace: sign in, provision,
/// store, resolve-and-verify (L5), then boot → suspend → migrate → resume →
/// terminate across two instances.
#[test]
fn operator_provisions_boots_and_migrates_a_devcontainer_holospace() {
    pollster::block_on(async {
        // Sign in: unlock a self-sovereign key → a content-addressed identity.
        let operator = Operator::from_public_key(b"operator-ed25519-public-key");
        assert!(operator.identity().as_str().starts_with("blake3:"));

        // Provision from a git repo + devcontainer.json (validated, CC-4).
        let holospace = ingest_devcontainer(
            "https://example.invalid/workspace.git",
            "v1",
            ".devcontainer/devcontainer.json",
            br#"{"name":"workspace","image":"mcr.microsoft.com/devcontainers/rust:1"}"#,
            caps(),
        )
        .expect("provision");
        let identity = holospace.kappa();

        // The definition lives in the content-addressed store on instance A.
        let store_a = MemKappaStore::new();
        let stored = store_a.put("blake3", &holospace.canonicalize()).unwrap();
        assert_eq!(stored, identity);

        // Boot: resolve + verify by re-derivation (L5), then run.
        let bytes = Resolver::resolve_local(&store_a, &identity)
            .expect("resolve")
            .expect("present");
        assert_eq!(Holospace::references(&bytes).unwrap().len(), 2);

        let runtime_a = MockRuntime::default();
        let mut a = Session::provision(&runtime_a, holospace.clone());
        a.boot().await.unwrap();
        assert_eq!(a.phase(), Phase::Running);

        // Suspend to a κ snapshot.
        let snapshot = a.suspend().await.unwrap();
        assert_eq!(a.phase(), Phase::Suspended);

        // Migrate (QS2): instance B adopts the snapshot κ and resumes.
        let runtime_b = MockRuntime::default();
        let mut b = Session::adopt(&runtime_b, holospace, snapshot);
        b.resume().await.unwrap();
        assert_eq!(b.phase(), Phase::Running);
        b.terminate().await.unwrap();
        assert_eq!(b.phase(), Phase::Terminated);
    });
}

/// The holo-file provisioning path (ADR-004): a `.holo` artifact referenced by
/// its κ becomes a holospace whose identity is reproducible and resolvable.
#[test]
fn operator_provisions_a_holo_file_holospace() {
    pollster::block_on(async {
        let artifact = holospaces::address(b"a .holo tensor-graph artifact");
        let holospace = Holospace::compose(Source::HoloFile { artifact }, caps());

        let store = MemKappaStore::new();
        let identity = store.put("blake3", &holospace.canonicalize()).unwrap();
        assert_eq!(identity, holospace.kappa());
        assert!(Resolver::resolve_local(&store, &identity)
            .unwrap()
            .is_some());

        let runtime = MockRuntime::default();
        let mut session = Session::provision(&runtime, holospace);
        session.boot().await.unwrap();
        session.terminate().await.unwrap();
        assert_eq!(session.phase(), Phase::Terminated);
    });
}

/// A Wasm-code holospace: the container code is a Wasm module validated against
/// the WebAssembly spec and the substrate's closed host surface (CC-5) before
/// it becomes the holospace's code.
#[test]
fn operator_validates_a_wasm_code_module_before_provisioning() {
    // A spec-valid module importing only the `hologram` host surface.
    let module = wat::parse_str(
        r#"(module (import "hologram" "log" (func (param i32 i32 i32))) (memory 1))"#,
    )
    .unwrap();
    holospaces::wasm::validate_substrate_module(&module).expect("module is substrate-valid");

    let code = holospaces::address(&module);
    let holospace = Holospace::compose(Source::HoloFile { artifact: code }, caps());
    let store = MemKappaStore::new();
    let identity = store.put("blake3", &holospace.canonicalize()).unwrap();
    assert_eq!(identity, holospace.kappa());
}
