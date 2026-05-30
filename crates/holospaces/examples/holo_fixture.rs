//! Generate a `.holo` fixture for the **browser `.holo`-engine differential**
//! (arc42 chapter 10 `CC-2`; chapter 11 RT2).
//!
//! Compiles a small `.holo`, runs it through the *native* hologram executor for
//! the reference output κ-label, and writes both the archive and the κ to an
//! output directory. The browser peer then runs the same archive through the
//! executor compiled to wasm and asserts an identical κ — the browser `.holo`
//! engine equals the native one.
//!
//! It also emits a `fixture-userland.wasm` — a real recompiled userland (the
//! second compute form, the execution surface ADR-008; `CC-6`) — so the browser
//! Platform Manager can validate it against the host-ABI surface and provision
//! it as a holospace in-browser.
//!
//! Run by `scripts/browser-manager-test.sh`:
//! `cargo run -p holospaces --example holo_fixture -- crates/holospaces-web/web`

use hologram_compiler::{compile, BackendKind};
use hologram_graph::constant::ConstantEntry;
use hologram_graph::node::Node;
use hologram_graph::registry::{DTypeId, ShapeDescriptor};
use hologram_graph::{Graph, GraphOp, InputSource};
use holospaces::engine::HoloEngine;
use prism::vocabulary::WittLevel;
use smallvec::SmallVec;

const DTYPE_F32: u8 = 8;

/// A minimal `.holo`: an Output node sourced from a constant scalar.
fn compile_constant_holo(value: f32) -> Vec<u8> {
    let mut graph = Graph::new();
    let shape = graph.shape_registry_mut().intern(ShapeDescriptor::rank1(1));
    let c = graph.constants_mut().insert(ConstantEntry {
        bytes: value.to_le_bytes().to_vec(),
        dtype: DTypeId(DTYPE_F32),
        shape,
    });
    let out = graph.add_node(Node {
        op: GraphOp::Output,
        inputs: SmallVec::from_iter([InputSource::Constant(c)]),
        output_dtype: DTypeId(DTYPE_F32),
        output_shape: shape,
    });
    graph.add_output(out);
    compile(graph, BackendKind::Cpu, WittLevel::W32)
        .expect("compile .holo")
        .archive
}

/// A real recompiled userland (the execution surface, ADR-008): general/system
/// code that presents the full container ABI and imports only the `hologram`
/// host ABI — so `holospaces::surface::validate_userland` accepts it and the
/// browser peer can provision it as a holospace.
const USERLAND_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_event")    (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend")  (result i32) (i32.const 0))
  (func (export "hg_resume")   (result i32) (i32.const 0))
  (func (export "hg_callback") (param i32 i32 i32) (result i32) (i32.const 0)))
"#;

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .expect("usage: holo_fixture <output-dir>");
    let archive = compile_constant_holo(64.0);
    let kappa = HoloEngine::run(&archive, &[]).expect("native run")[0]
        .as_str()
        .to_owned();
    std::fs::write(format!("{out_dir}/fixture.holo"), &archive).expect("write fixture.holo");
    std::fs::write(format!("{out_dir}/fixture.kappa"), &kappa).expect("write fixture.kappa");

    // The execution-surface fixture: a recompiled userland, validated here so
    // the emitted artifact is guaranteed surface-valid for the browser peer.
    let userland = wat::parse_str(USERLAND_WAT).expect("assemble userland wasm");
    holospaces::surface::validate_userland(&userland).expect("userland is surface-valid");
    std::fs::write(format!("{out_dir}/fixture-userland.wasm"), &userland)
        .expect("write fixture-userland.wasm");

    println!(
        "holo_fixture: wrote fixture.holo ({} bytes, native output κ = {kappa}) and \
         fixture-userland.wasm ({} bytes, surface-valid)",
        archive.len(),
        userland.len()
    );
}
