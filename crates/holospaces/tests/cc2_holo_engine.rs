//! **CC-2 — the `.holo` engine equals the native one.**
//!
//! The Conformance catalog row `CC-2` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): the browser `.holo`
//! engine equals the native one — the native
//! [hologram](https://github.com/Hologram-Technologies/hologram) executor as
//! oracle, identical `.holo` yielding identical κ — by a differential check.
//!
//! The engine holospaces runs is hologram's `.holo` executor (`hologram-exec`),
//! native and (for the browser peer) compiled to wasm. The guarantee that the
//! browser engine equals the native one rests on the executor being a
//! **deterministic, content-addressed** function of the `.holo` semantics:
//! identical `.holo` + inputs yield identical output bytes, hence an identical
//! κ-label under the substrate's σ-axis (the same `address_bytes` both builds
//! use). This witness establishes that property natively, against the executor
//! as its own oracle:
//!
//! 1. **Determinism across independent builds** — two independent `compile`s of
//!    the same graph, executed in independent sessions, yield byte-identical
//!    output and the same holospaces κ.
//! 2. **Cross-surface agreement** — the byte-boundary surface (`execute`) and
//!    the address-boundary surface (`execute_addressed`, the path a pipeline /
//!    the browser FFI drives) yield the same output κ.
//!
//! The **live** browser-vs-native differential is `scripts/browser-manager-test.sh`
//! (the CI `browser` job): it runs this same `.holo`, compiled and executed
//! natively here, through the executor compiled to wasm in headless Chromium,
//! and asserts an identical output κ — the browser `.holo` engine equals the
//! native one (arc42 chapter 11, RT2). See `vv/PROVENANCE.md`.
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc2_holo_engine`.

use hologram_backend::CpuBackend;
use hologram_compiler::{compile, BackendKind};
use hologram_exec::{BufferArena, InferenceSession};
use hologram_graph::constant::ConstantEntry;
use hologram_graph::node::Node;
use hologram_graph::registry::{DTypeId, ShapeDescriptor};
use hologram_graph::{Graph, GraphOp, InputSource};
use holospaces::Kappa;
use prism::vocabulary::WittLevel;
use smallvec::SmallVec;

const DTYPE_F32: u8 = 8;

/// A minimal `.holo`: an Output node sourced from a constant scalar (`64.0`).
/// Compiled fresh each call, so two calls are two independent builds.
fn compile_constant_holo(value: f32) -> Vec<u8> {
    let mut graph = Graph::new();
    let shape = graph.shape_registry_mut().intern(ShapeDescriptor::rank1(1));
    let c = graph.constants_mut().insert(ConstantEntry {
        bytes: value.to_le_bytes().to_vec(),
        dtype: DTypeId(DTYPE_F32),
        shape,
    });
    let out_node = graph.add_node(Node {
        op: GraphOp::Output,
        inputs: SmallVec::from_iter([InputSource::Constant(c)]),
        output_dtype: DTypeId(DTYPE_F32),
        output_shape: shape,
    });
    graph.add_output(out_node);
    compile(graph, BackendKind::Cpu, WittLevel::W32)
        .expect("compile .holo")
        .archive
}

fn execute_to_output(archive: &[u8]) -> Vec<u8> {
    let backend: CpuBackend<BufferArena> = CpuBackend::new();
    let mut session = InferenceSession::load(archive, backend).expect("load .holo");
    let outputs = session.execute(&[]).expect("execute .holo");
    outputs[0].bytes.clone()
}

/// (1) Determinism across independent builds: identical `.holo` semantics yield
/// the same output bytes and the same holospaces κ — the invariant that makes
/// the browser engine equal the native one.
#[test]
fn identical_holo_yields_identical_kappa_across_independent_builds() {
    let out_a = execute_to_output(&compile_constant_holo(64.0));
    let out_b = execute_to_output(&compile_constant_holo(64.0));
    assert_eq!(out_a, out_b, "executor is deterministic");

    let kappa_a: Kappa = holospaces::address(&out_a);
    let kappa_b: Kappa = holospaces::address(&out_b);
    assert_eq!(
        kappa_a, kappa_b,
        "identical .holo yields identical κ (CC-2)"
    );
    assert_eq!(kappa_a.sigma_axis(), Some("blake3"));

    // A different .holo (different constant) yields a different κ.
    let out_c = execute_to_output(&compile_constant_holo(65.0));
    assert_ne!(holospaces::address(&out_c), kappa_a);
}

/// holospaces' `.holo Engine` building block runs a `.holo` and content-
/// addresses its output, agreeing with the direct executor (the engine is the
/// real component, not just the test harness).
#[test]
fn holo_engine_runs_and_addresses_the_output() {
    let archive = compile_constant_holo(64.0);
    let direct = holospaces::address(&execute_to_output(&archive));
    let via_engine = holospaces::engine::HoloEngine::run(&archive, &[]).expect("engine run");
    assert_eq!(via_engine.len(), 1);
    assert_eq!(
        via_engine[0], direct,
        "the .holo Engine addresses the output"
    );
}

/// (2) Cross-surface agreement: the address-boundary surface
/// (`execute_addressed`, the pipeline / browser-FFI path) yields the same
/// output κ as the byte-boundary surface (`execute`).
#[test]
fn byte_and_address_boundary_surfaces_agree() {
    let archive = compile_constant_holo(64.0);
    let via_bytes = holospaces::address(&execute_to_output(&archive));

    let backend: CpuBackend<BufferArena> = CpuBackend::new();
    let mut session = InferenceSession::load(&archive, backend).expect("load .holo");
    let out_labels: Vec<hologram_archive::ContentLabel> =
        session.execute_addressed(&[]).expect("execute_addressed");
    let out_bytes = session
        .resolve(&out_labels[0])
        .expect("resolve output label")
        .to_vec();
    let via_addr = holospaces::address(&out_bytes);

    assert_eq!(via_bytes, via_addr, "both engine surfaces agree on the κ");
}
