//! `CC-8` — arbitrary code imported by κ runs over capability-scoped I/O (arc42
//! chapter 10, Conformance catalog; ADR-009, mirroring hologram's driver-import
//! witnesses).
//!
//! Witnesses the trustless-import-and-run path that the architecture is built on
//! (ADR-006/ADR-009: "code is κ-addressed; arbitrary code is imported by κ,
//! verified by re-derivation, and instantiated"). No mocks — the real Wasmtime
//! [`Runtime`] enforces every bound. Two external authorities:
//!
//! * the [hologram](https://github.com/Hologram-Technologies/hologram)
//!   driver-import contract — a program fetched by κ is accepted only if its
//!   bytes re-derive to that κ ([`holospaces::boot::import`] over
//!   `get_with_fetch`); a **forged** import is refused (Law L5);
//! * the hologram `ContainerRuntime` capability contract — the imported program
//!   does **real** `storage_put`/`storage_get` host calls, bounded by its
//!   Capability Set: a put over its storage quota is refused, and a get of a κ
//!   outside its storage roots is denied — by the real runtime, not by
//!   holospaces. The program's output is its own oracle (identical code + input
//!   ⇒ identical output κ).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use hologram_runtime::Runtime;
use hologram_runtime_wasmtime::WasmtimeEngine;
use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{
    address_bytes, Bytes, KappaLabel71, KappaStore, KappaSync, SyncError,
};
use holospaces::boot::{import, provision};
use holospaces::substrate::{Capabilities, ContainerRuntime};
use holospaces::Source;

/// A program that persists its event bytes via the `storage_put` host call —
/// real capability-scoped write I/O (the writer's output κ is deterministic).
const WRITER_WAT: &str = r#"
(module
  (import "hologram" "storage_put" (func $put (param i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (func (export "hg_init") (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend") (result i32) (i32.const 0))
  (func (export "hg_resume")  (result i32) (i32.const 0))
  ;; The host wrote the event bytes at mem[0]; persist `len` of them.
  (func (export "hg_event") (param i32 i32) (result i32)
    (call $put (i32.const 0) (local.get 1) (i32.const 600))))
"#;

/// A program that reads content via the `storage_get` host call — real
/// capability-scoped read I/O (gated by the storage roots / read-closure).
const READER_WAT: &str = r#"
(module
  (import "hologram" "storage_get" (func $get (param i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (func (export "hg_init") (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend") (result i32) (i32.const 0))
  (func (export "hg_resume")  (result i32) (i32.const 0))
  ;; The host wrote a 71-byte κ at mem[0]; read it into mem[600].
  ;; Returns bytes copied, or -1 (0xFFFFFFFF) when denied by capability scope.
  (func (export "hg_event") (param i32 i32) (result i32)
    (call $get (i32.const 0) (i32.const 600) (i32.const 1024))))
"#;

/// A source peer that serves content by κ — the "remote" the local peer imports
/// from. It can be told to serve *forged* bytes for a κ (to witness L5).
#[derive(Default)]
struct SourcePeer {
    blobs: Mutex<HashMap<[u8; 71], Vec<u8>>>,
}

impl SourcePeer {
    fn publish(&self, bytes: &[u8]) -> KappaLabel71 {
        let k = address_bytes(bytes);
        self.blobs
            .lock()
            .unwrap()
            .insert(*k.as_array(), bytes.to_vec());
        k
    }
    /// Advertise `claimed` but serve `forged` bytes — a lying gateway.
    fn publish_forged(&self, claimed: &KappaLabel71, forged: &[u8]) {
        self.blobs
            .lock()
            .unwrap()
            .insert(*claimed.as_array(), forged.to_vec());
    }
}

#[async_trait]
impl KappaSync for SourcePeer {
    async fn fetch(&self, kappa: &KappaLabel71) -> Result<Option<Bytes>, SyncError> {
        Ok(self
            .blobs
            .lock()
            .unwrap()
            .get(kappa.as_array())
            .map(|v| Bytes::from(v.as_slice())))
    }
    async fn announce(&self, _kappa: &KappaLabel71) {}
    async fn discover(&self, _prefix: Option<&[u8]>, _limit: usize) -> Vec<KappaLabel71> {
        Vec::new()
    }
    async fn add_peer(&self, _m: &str) -> Result<(), SyncError> {
        Ok(())
    }
    async fn add_gateway(&self, _u: &str) -> Result<(), SyncError> {
        Ok(())
    }
}

fn caps(quota: u64, roots: Vec<KappaLabel71>) -> Capabilities {
    Capabilities {
        storage_roots: roots,
        storage_quota_bytes: quota,
        network_fetch: true,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: 0,
        cpu_time_per_event_ms: 100,
        priority_weight: 0,
    }
}

/// A forged import — bytes that do not re-derive to the requested κ — is refused
/// at the import boundary (Law L5). Trust is in the math, not the gateway.
#[test]
fn a_forged_import_is_refused_by_re_derivation() {
    pollster::block_on(async {
        let honest = wat::parse_str(WRITER_WAT).unwrap();
        let source = SourcePeer::default();
        let code_k = address_bytes(&honest);
        // The gateway lies: it serves tampered bytes under the honest κ.
        source.publish_forged(&code_k, b"this is not the program you asked for");

        let local = MemKappaStore::new();
        let err = import(&local, &source, &code_k)
            .await
            .expect_err("a forged import must be refused");
        assert!(
            matches!(
                err,
                hologram_substrate_core::AccessError::VerificationFailed
            ),
            "tampered bytes are rejected on re-derivation (L5), got {err:?}"
        );
        assert!(!local.contains(&code_k), "forged bytes are not cached");
    });
}

/// An imported program runs and persists deterministic output via real
/// `storage_put`, and its writes are bounded by its storage quota — the real
/// runtime refuses an over-budget put. (CC-8, the write-capability authority.)
#[test]
fn imported_program_runs_with_capability_scoped_writes() {
    pollster::block_on(async {
        let program = wat::parse_str(WRITER_WAT).unwrap();
        let source = SourcePeer::default();
        let code_k = source.publish(&program);

        // Import by κ (verified on receipt), then provision it as a holospace
        // with a tight storage quota of 8 bytes.
        let store = MemKappaStore::new();
        let imported = import(&store, &source, &code_k)
            .await
            .unwrap()
            .expect("program imported");
        assert_eq!(imported.as_ref(), &program[..], "verified bytes");

        let holospace = provision(&store, Source::Userland { entry: code_k }, caps(8, vec![]))
            .expect("provision the imported program");

        let rt = Runtime::new(WasmtimeEngine::new(), store);
        let h = rt
            .spawn(holospace.manifest(), holospace.capabilities())
            .await
            .expect("spawn the imported program");

        // A within-quota put (5 bytes) succeeds; the output content is in the
        // store at its deterministic κ (the program's own oracle).
        assert_eq!(
            rt.deliver_event(h, b"12345").unwrap(),
            0,
            "within-quota put"
        );
        assert!(
            rt.store().contains(&address_bytes(b"12345")),
            "output persisted at its content address (identical input ⇒ identical κ)"
        );

        // An over-quota put (remaining budget 3 bytes, asking 50) is refused by
        // the real runtime, and nothing is stored.
        let big = [7u8; 50];
        assert_eq!(
            rt.deliver_event(h, &big).unwrap(),
            u32::MAX,
            "over-quota put refused by the runtime"
        );
        assert!(!rt.store().contains(&address_bytes(&big)), "not persisted");
    });
}

/// An imported program's reads are gated by its storage roots: a get of a κ
/// outside the read-closure is denied; granting the root admits it — enforced by
/// the real runtime. (CC-8, the read-capability authority.)
#[test]
fn imported_program_reads_are_gated_by_storage_roots() {
    pollster::block_on(async {
        let program = wat::parse_str(READER_WAT).unwrap();
        let source = SourcePeer::default();
        let code_k = source.publish(&program);

        // ── Run 1: no storage roots — the get is denied. ──
        let store = MemKappaStore::new();
        let secret = store.put("blake3", b"capability-scoped secret").unwrap();
        import(&store, &source, &code_k).await.unwrap().unwrap();
        let hs_denied =
            provision(&store, Source::Userland { entry: code_k }, caps(0, vec![])).unwrap();
        let rt = Runtime::new(WasmtimeEngine::new(), store);
        let h = rt
            .spawn(hs_denied.manifest(), hs_denied.capabilities())
            .await
            .unwrap();
        assert_eq!(
            rt.deliver_event(h, secret.as_array()).unwrap(),
            u32::MAX,
            "get of a κ outside the storage roots is denied"
        );

        // ── Run 2: grant the secret as a storage root — the get is admitted. ──
        let store2 = MemKappaStore::new();
        let secret2 = store2.put("blake3", b"capability-scoped secret").unwrap();
        assert_eq!(secret, secret2, "same content ⇒ same κ");
        import(&store2, &source, &code_k).await.unwrap().unwrap();
        let hs_ok = provision(
            &store2,
            Source::Userland { entry: code_k },
            caps(0, vec![secret2]),
        )
        .unwrap();
        let rt2 = Runtime::new(WasmtimeEngine::new(), store2);
        let h2 = rt2
            .spawn(hs_ok.manifest(), hs_ok.capabilities())
            .await
            .unwrap();
        let n = rt2.deliver_event(h2, secret2.as_array()).unwrap();
        assert_eq!(
            n,
            b"capability-scoped secret".len() as u32,
            "granting the storage root admits the read"
        );
    });
}
