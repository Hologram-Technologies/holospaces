# Architecture Decisions

Each decision records Status, Context, Decision, and Consequences.
Implemented features link back to the decision they realize.

## ADR-001: No server — content-addressed peers

**Status:** Accepted. **Context:** hologram identifies everything by
content (a κ-label), and forbids host/path identity. A client/server or
control-plane design would reintroduce location as identity.
**Decision:** holospaces has no server and no control plane. Every
participant is a *peer* that becomes the substrate; identity is the
κ-label (Law L1). **Consequences:** No backend to operate or trust;
"remote management" is meaningless — there are only κ-labels and the
peers that hold them. Bootstrap (e.g. GitHub Pages) is content delivery,
not a host.

## ADR-002: Canonical forms only; the store is the memory

**Status:** Accepted. **Context:** Holding deserialized objects in RAM
bounds what can run and duplicates content. **Decision:** Operate only
on canonical forms; hold κ-labels, not objects; canonicalize at the
ingest boundary. The content-addressed store is the address space; RAM
is a cache (Laws L2, L3). **Consequences:** A holospace far larger than
RAM remains bootable (demand-paged by κ-resolve); identical content is
stored once; no serialize/deserialize seam.

## ADR-003: Everything through the substrate — compose hologram’s executor and runtime

**Status:** Accepted. **Context:** Compute is the
[hologram](https://github.com/Hologram-Technologies/hologram) `.holo`
executor; deployment is hologram’s ContainerRuntime. Bypassing the
substrate would create a parallel medium. **Decision:** holospaces runs
a tensor `.holo` via hologram’s executor and a Wasm code module through
hologram’s `ContainerRuntime` — two distinct substrate paths, never a
parallel medium — with all state in the store as κ (Law L4).
**Consequences:** holospaces stays a thin layer;
storage/network/runtime/compute are reused, never re-implemented.

## ADR-004: The holospace is the unit; two provisioning paths

**Status:** Accepted. **Context:** Workloads range from a single model
to a full Linux environment. **Decision:** The unit of management is the
*holospace* — a bootable, κ-addressed environment — provisioned from a
**holo-file** or a **devcontainer**. All holospaces share one lifecycle.
**Consequences:** Uniform management across kinds; platform-type
specifics (e.g. ONNX/GGUF → `.holo`) stay upstream
([hologram-ai](https://github.com/Hologram-Technologies/hologram-ai)),
not in holospaces.

## ADR-005: The documentation is authoritative

**Status:** Accepted. **Context:** A second source of truth (e.g. a
`specs/` directory) drifts from the docs. **Decision:** This
documentation — arc42 + C4, OPM ISO 19450, ISO/IEC/IEEE 15288 — kept
in-repo (no separate wiki), is the single authoritative source. No
`specs/` directory; no narrative status doc. Every implemented feature
links back here. **Consequences:** One spec, validated by the V1–V8
pipeline; status lives in V&V/CI and git, not in prose.

## ADR-006: Consume the stack by reference

**Status:** Accepted. **Context:** holospaces depends on a private
hologram and the public UOR-Foundation crates. **Decision:** Consume
hologram member crates as git dependencies; `uor-addr` /
`uor-foundation` from crates.io (Prism transitively). Describe their
APIs by hyperlink, never by restating internals. Use GitHub Pages only
as an untrusted, content-addressed gateway. **Consequences:** Upstream
remains authoritative for its own interfaces; holospaces docs never
assume external API details.

## ADR-007: Enforce the quality commitments in CI from the beginning

**Status:** Accepted. **Context:** The quality commitments (Chapter 10,
and the laws of Chapter 2) must hold from the first commit, not be
retrofitted once code exists. **Decision:** A Cargo workspace with
workspace lints (forbid `unsafe_code`, warn on `missing_docs`, clippy)
is established up front. CI (`.github/workflows/ci.yml`) runs, on every
change, the V&V (`vv/run.sh`) alongside `cargo fmt`, `cargo clippy` with
warnings denied, and the unit, integration, and e2e test tiers.
**Consequences:** Every component lands already under the documented
quality gates and is held to its external authority (`CC-*`); quality
cannot silently regress. The test tiers are empty until components exist
— the gates are not.

## ADR-008: The execution surface is a κ-addressed Wasm userland over the substrate host ABI

**Status:** Superseded by ADR-009. (Originally resolved RT1 of Chapter
11. Its κ-addressed-Wasm-over-the-host-ABI surface stands and is reused;
but its choice of **recompiled userlands only** runs only what was
recompiled, not an arbitrary operating system — so it cannot host an
arbitrary devcontainer. ADR-009 generalizes the surface to arbitrary OS
images via emulation.)

**Context:** A devcontainer holospace needs a Linux/POSIX execution
surface, but the substrate is Wasm-only with no ambient WASI (the closed
host surface of `CC-5`). Two surfaces were possible: (A) a
hologram-native CPU/system *emulator-as-container* that runs arbitrary
OCI images, or (B) a *Wasm-native* surface in which userlands are
recompiled to κ-addressed Wasm modules that bind only the substrate’s
host ABI. The choice is forced by the laws, not by convenience:

- An OCI image is named by registry, repository, and tag — by
  *location*. Adopting (A) would make a holospace’s code identity a
  location, reintroducing exactly what Law L1 and ADR-001 forbid; an
  opaque layered image is also not a canonical form holospaces could
  operate on (Law L2).

- A CPU/OCI emulator is a *second execution medium* beside hologram’s
  runtime, which Law L4 and ADR-003 forbid ("everything through the
  substrate; no parallel medium").

**Decision:** The execution surface is (B). A holospace’s general/system
code — the second of the two compute forms (Chapter 8) — is a
**Wasm-recompiled userland**: a κ-addressed Wasm code module that
imports only the substrate’s host ABI (the `hologram` host module — the
syscall boundary) and presents the container ABI hologram’s
`ContainerRuntime` drives. The POSIX/libc layer compiles to such a
module (its libraries linked in, as Wasm linking does — the entry module
is what the runtime boots); a workload’s data is paged in by κ-resolve
at runtime (Law L3), not baked into the image. A devcontainer’s
`devcontainer.json` *selects* a κ-addressed userland (content), it does
not name an OCI image to emulate (location). holospaces **defines,
enforces, and boots** this surface on every peer — it ingests the
source, validates the userland against the host-ABI contract, composes a
bootable holospace, and spawns it through hologram’s `ContainerRuntime`
over the peer’s `ContainerEngine` (Wasmtime natively; the `wasmi`
interpreter in the browser and on bare-metal, where a JIT cannot run). A
userland is κ-addressed **content** the platform hosts; authoring it
(compiling C, building a model) is writing a program, not a platform
feature — exactly as a `.holo` is authored by a compiler (ADR-004). The
surface is realized by the Execution Surface building block (Chapter 5)
and witnessed by `CC-6` (Chapter 10) on both the native and interpreter
engines.

**Consequences:** Code identity stays content, never location (L1); a
userland is a canonical form, deduped and verifiable like any κ (L2,
L5); execution stays on the one substrate medium (L4); the same userland
κ boots on any peer (Q6), native or browser/bare-metal. The cost is that
arbitrary prebuilt OCI images do not run as-is — a workload must be a
Wasm userland (compiled for the substrate’s host ABI), which is the
price of content-addressed identity. Choosing the location-addressed
emulator was rejected as a law violation, not a trade-off.

## ADR-009: Run arbitrary operating systems via a κ-addressed system-emulator codemodule

**Status:** Accepted. (Supersedes ADR-008; revisits RT1 of Chapter 11.)

**Context:** holospaces must host **arbitrary** devcontainers with full
[Codespaces](https://github.com/features/codespaces) /
[Gitpod](https://www.gitpod.io) parity — any repository’s environment,
running real binaries (`apt`, shells, language toolchains), not only
software recompiled for the substrate. ADR-008 chose a Wasm-native
**recompiled userland** and rejected the emulator-as-container,
reasoning that an OCI image is named by location (Law L1). That
reasoning conflated the **image** with its **reference**: a recompiled
userland runs only what was recompiled — it cannot run an arbitrary OS —
whereas an emulator can, and an emulator over κ-addressed content
reintroduces no location. The
[hologram](https://github.com/Hologram-Technologies/hologram) substrate
already provides the mechanism (ADR-006): **code is κ-addressed, and
arbitrary code is imported by κ, verified by re-derivation, and
instantiated** (hologram’s driver-import witnesses — real Wasm drivers
perform real block/NIC I/O over the κ-store); workloads are arbitrary
Wasm over the `hologram` host ABI.

**Decision:** A holospace’s execution is a **κ-addressed system-emulator
codemodule** — a real system emulator compiled to Wasm and bound to the
`hologram` host ABI — that boots an arbitrary operating-system image.
Its disk is the OS image + repository as **κ-addressed content** (a
`KappaStore`-backed block device); its console,
`stdin`/`stdout`/`stderr`, and network are **hologram channels**
(`publish`/`subscribe`); its running state is a **κ snapshot** (suspend
/ resume / migrate). The emulator and the OS image are imported and
verified trustlessly like any κ. holospaces **starts with Linux** and
generalizes to any OS the emulator boots. This **extends** the execution
surface of ADR-008 (a κ-addressed Wasm code module over the host ABI —
the Execution Surface building block, `CC-6` — which still stands) from
"recompiled userlands only" to "arbitrary OS images via emulation"; the
emulator is itself such a κ-addressed code module. The emulator core is
a real RISC-V machine (RV64GC (= IMAFDC) + Zicsr, machine/supervisor
traps, Sv39/Sv48/Sv57 paging, CLINT interrupts, SBI) bound directly to
the `hologram` host ABI and **verified against the official
[riscv-tests](https://github.com/riscv-software-src/riscv-tests)
conformance suite** — the same authority real hardware and QEMU are
validated against. It is a clean-room core rather than an adapted
off-the-shelf emulator because existing Wasm system emulators bind WASI
or a browser host surface, which the substrate’s closed host ABI refuses
(`CC-5`, Law L1); binding the ISA emulator directly to `hologram` is the
uor-native realization, with the official ISA suite as its external
oracle (`CC-9`).

**Consequences:** Identity stays content, never location — the OS image
is a κ, not a registry reference (L1); the image dedupes and verifies
like any κ (L2, L5); execution stays on the one substrate medium — the
emulator is Wasm over the host ABI, no parallel runtime (L4); the same
holospace κ boots on any peer (Q6). holospaces runs **arbitrary**
devcontainers — the Codespaces/Gitpod goal — at the cost of emulation
overhead and one substantial component, the emulator codemodule. The
management GUI (served from GitHub Pages) and the VS Code **projection**
that connects to a running holospace are a distinct surface (Chapter 5,
**Projections**; Chapter 7) — the operator manages holospaces in the GUI
and launches one into a Codespaces/Gitpod-like editor + terminal bound
to the running devcontainer.
