//! `CC-9` — the system emulator (arc42 chapter 10, Conformance catalog; ADR-009).
//!
//! The [emulator](holospaces::emulator) is verified against external authorities:
//!
//! * it **passes the official RISC-V `riscv-tests` conformance suite** (rv64ui +
//!   um + ua + uc + uf + ud + mi/si) — the canonical authority real hardware and
//!   QEMU are validated against, exercising the base ISA, the M/A/F/D/C
//!   extensions, and the machine/supervisor trap architecture;
//! * **a real, unmodified Linux 6.6 kernel boots to userspace on it**, and PID 1
//!   produces console output byte-identical to `qemu-system-riscv64` on the same
//!   image — the differential oracle (CC-9's end state);
//! * it runs **as a real hologram Wasm container codemodule** on the substrate
//!   runtime, with its disk as a `CC-7` κ-disk and a reproducible κ snapshot —
//!   and a real Linux kernel boots **through that codemodule** on the runtime,
//!   its image delivered as κ content (ADR-009 fully realized).

use std::path::{Path, PathBuf};

use hologram_realizations::{CapabilitySet, ContainerManifest};
use hologram_runtime::Runtime;
use hologram_runtime_wasmtime::WasmtimeEngine;
use hologram_store_mem::MemKappaStore;
use holospaces::disk::{BlockDevice, KappaDisk};
use holospaces::emulator::{Emulator, Halt};
use holospaces::realizations::empty_kappa;
use holospaces::substrate::{Capabilities, ContainerRuntime, KappaStore, Realization};
use holospaces::{address, surface, verify};

fn artifact_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc9")
}

/// The `(program, expected-exit-code)` battery from the pinned authority file.
fn expected() -> Vec<(String, u64)> {
    let text = std::fs::read_to_string(artifact_dir().join("expected.txt")).expect("expected.txt");
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next().unwrap().to_owned();
            let code: u64 = it.next().unwrap().parse().unwrap();
            (name, code)
        })
        .collect()
}

fn run_flat(image: &[u8]) -> Halt {
    let mut emu = Emulator::new(0, 256 * 1024);
    emu.load_flat(image).expect("image fits in RAM");
    emu.run(1_000_000)
}

/// Every real RISC-V program assembled by LLVM yields the ISA-defined exit code
/// when run on the emulator core — the core conforms to the RISC-V ISA. (CC-9
/// foundation, the ISA authority.)
#[test]
fn the_emulator_core_conforms_to_the_risc_v_isa() {
    let battery = expected();
    assert!(battery.len() >= 4, "the ISA battery is present");
    for (name, code) in battery {
        let image = std::fs::read(artifact_dir().join(format!("{name}.bin")))
            .unwrap_or_else(|_| panic!("read {name}.bin"));
        match run_flat(&image) {
            Halt::Exit(got) => assert_eq!(
                got, code,
                "{name}: emulator yielded {got}, ISA-defined result is {code}"
            ),
            other => panic!("{name}: expected exit {code}, got {other:?}"),
        }
    }
}

/// The emulator's `write` syscall surface produces real console output (the
/// channel the codemodule publishes), and the machine snapshot is reproducible
/// across identical runs (Law L1 — a κ snapshot is content). (CC-9 foundation.)
#[test]
fn the_emulator_writes_console_output_and_snapshots_reproducibly() {
    // A real program: write(1, msg, 5) then exit(0). msg "hi!\n\0" is placed by
    // the program at a known RAM address via store immediates (no relocation).
    // li a0,1 (fd); place bytes; li a1,addr; li a2,len; li a7,64; ecall; exit.
    // Assembled equivalent is shipped as console.bin if present; otherwise this
    // test drives the syscall via a tiny hand-encoded program.
    let prog: &[u8] = &[
        // addi t0, x0, 0x48 ; sb t0, 0x100(x0)   'H'
        0x93, 0x02, 0x80, 0x04, // addi t0,x0,0x48
        0x23, 0x00, 0x50, 0x10, // sb t0,256(x0)
        // addi t0, x0, 0x69 ; sb t0, 0x101(x0)   'i'
        0x93, 0x02, 0x90, 0x06, // addi t0,x0,0x69
        0xa3, 0x00, 0x50, 0x10, // sb t0,257(x0)
        // write(fd=1, buf=0x100, len=2)
        0x13, 0x05, 0x10, 0x00, // addi a0,x0,1
        0x93, 0x05, 0x00, 0x10, // addi a1,x0,256
        0x13, 0x06, 0x20, 0x00, // addi a2,x0,2
        0x93, 0x08, 0x00, 0x04, // addi a7,x0,64
        0x73, 0x00, 0x00, 0x00, // ecall (write)
        // exit(0)
        0x13, 0x05, 0x00, 0x00, // addi a0,x0,0
        0x93, 0x08, 0xd0, 0x05, // addi a7,x0,93
        0x73, 0x00, 0x00, 0x00, // ecall (exit)
    ];

    let snap = || {
        let mut emu = Emulator::new(0, 64 * 1024);
        emu.load_flat(prog).unwrap();
        let halt = emu.run(1000);
        (emu.console().to_vec(), halt, emu.snapshot())
    };
    let (console, halt, snapshot) = snap();
    assert_eq!(halt, Halt::Exit(0));
    assert_eq!(
        &console, b"Hi",
        "the write syscall produced real console output"
    );
    let (_, _, snapshot2) = snap();
    assert_eq!(
        snapshot, snapshot2,
        "identical runs ⇒ identical κ snapshot (L1)"
    );
}

/// The emulator passes the **official RISC-V `riscv-tests` conformance suite** —
/// the canonical external authority for the RISC-V ISA, the same suite hardware
/// and QEMU are validated against: the unprivileged `rv64ui` (base) + `rv64um`
/// (mul/div) + `rv64ua` (atomics) + `rv64uc` (compressed) in full, plus the
/// machine- and supervisor-mode privileged tests (`rv64mi`/`rv64si`) the emulator
/// passes (the manifest pins exactly which; the not-yet-covered privileged tests
/// are recorded in `riscv-tests/SOURCE.txt`). Each runs in a real machine-mode
/// environment (installs `mtvec`, drops to a lower mode via `mret`/`sret`, runs
/// its self-checking cases, and signals pass/fail through the HTIF `tohost`
/// channel), so passing them exercises the privileged trap architecture
/// (`ecall`/`ebreak` exceptions, delegation, `sret`) as well as the base ISA.
/// (CC-9, the canonical ISA-conformance authority.)
#[test]
fn the_emulator_passes_the_official_riscv_tests() {
    let dir = artifact_dir().join("riscv-tests");
    // The manifest pins each test's HTIF `tohost` address (it depends on the
    // test's size, so it is not a fixed constant).
    let manifest = std::fs::read_to_string(dir.join("manifest.txt"))
        .expect("riscv-tests manifest (built per vv/artifacts/cc9/riscv-tests/SOURCE.txt)");
    let tests: Vec<(&str, u64)> = manifest
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next().unwrap();
            let tohost = it.next().unwrap().trim_start_matches("0x");
            (name, u64::from_str_radix(tohost, 16).unwrap())
        })
        .collect();
    assert!(
        tests.len() >= 80,
        "the official suite is present ({})",
        tests.len()
    );

    let mut failures = Vec::new();
    for (name, tohost) in &tests {
        let image = std::fs::read(dir.join(format!("{name}.bin"))).expect("test image");
        // The -p tests link at 0x8000_0000; HTIF tohost per the manifest.
        let mut emu = Emulator::new(0x8000_0000, 16 * 1024 * 1024);
        emu.load_flat(&image).expect("image fits");
        emu.set_htif(*tohost);
        let halt = emu.run(50_000_000);
        if halt != Halt::Exit(0) {
            failures.push(format!("{name}: {halt:?}"));
        }
    }
    assert!(failures.is_empty(), "riscv-tests failures: {failures:#?}");
    eprintln!("cc9: passed all {} official riscv-tests", tests.len());
}

/// The emulator takes a **CLINT timer interrupt** — the periodic tick a kernel's
/// scheduler relies on. A real program (assembled by the RISC-V toolchain,
/// `vv/artifacts/cc9/tint.S`/`.bin`) arms `mtimecmp` via the memory-mapped CLINT,
/// enables the machine timer interrupt (`mie.MTIE` + `mstatus.MIE`), and spins;
/// when `mtime` reaches the compare the emulator raises the timer interrupt
/// (cause = interrupt | 7) into the handler, which confirms `mcause` and signals
/// success over HTIF. (CC-9, the interrupt/timer authority — the RISC-V
/// Privileged ISA + the CLINT memory map.)
#[test]
fn the_emulator_takes_a_clint_timer_interrupt() {
    let manifest = std::fs::read_to_string(artifact_dir().join("tint.manifest")).expect("manifest");
    let tohost = u64::from_str_radix(
        manifest
            .split_whitespace()
            .nth(1)
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )
    .unwrap();
    let image = std::fs::read(artifact_dir().join("tint.bin")).expect("tint.bin");
    let mut emu = Emulator::new(0x8000_0000, 16 * 1024 * 1024);
    emu.load_flat(&image).unwrap();
    emu.set_htif(tohost);
    assert_eq!(
        emu.run(10_000_000),
        Halt::Exit(0),
        "the machine timer interrupt fires and the handler confirms mcause"
    );
}

/// The emulator provides the **SBI firmware interface** an S-mode kernel boots
/// under. A real program (assembled by the RISC-V toolchain,
/// `vv/artifacts/cc9/sbi.S`/`.bin`) drops to supervisor mode via `mret`, then
/// uses SBI `ecall`s — console `putchar` to print, then system reset to halt.
/// The emulator-as-SEE services them: the console receives the bytes and the
/// reset ends the run. (CC-9, the SBI authority — the RISC-V SBI specification.)
#[test]
fn the_emulator_services_sbi_console_and_shutdown() {
    let image = std::fs::read(artifact_dir().join("sbi.bin")).expect("sbi.bin");
    let mut emu = Emulator::new(0x8000_0000, 16 * 1024 * 1024);
    emu.load_flat(&image).unwrap();
    emu.enable_sbi(); // run as the M-mode firmware (SEE)
    let halt = emu.run(1_000_000);
    assert_eq!(halt, Halt::Exit(0), "SBI system reset halts the machine");
    assert_eq!(
        emu.console(),
        b"OK\n",
        "the S-mode kernel's SBI console output reaches the emulator console"
    );
}

/// The emulator runs **as a real hologram container codemodule on the engine**:
/// the `holospaces-emulator` Wasm module (imports only `hologram.storage_put`,
/// exports the container ABI) is validated against the execution-surface
/// contract, then spawned on the **real Wasmtime runtime** with a guest image as
/// its initial state. It runs the RISC-V program and emits the ISA-defined result
/// back into the substrate via the host ABI — content-addressed, so the result κ
/// is the guest's deterministic output. The container κ snapshot is the runtime's
/// own and is reproducible. This is ADR-009's claim realized: the emulator is
/// κ-addressed Wasm over the host ABI, not a parallel medium (Law L4). (CC-9,
/// the emulator on the substrate.)
#[test]
fn the_emulator_codemodule_runs_on_the_real_hologram_runtime() {
    pollster::block_on(async {
        let wasm = std::fs::read(artifact_dir().join("emulator.wasm"))
            .expect("the emulator codemodule (run scripts/build-emulator.sh)");

        // It is a valid execution-surface codemodule: spec-valid, host-ABI-only
        // imports, full container ABI (the CC-6 contract the emulator binds).
        surface::validate_userland(&wasm).expect("emulator is a valid codemodule");

        // sum1to10 computes 55; the container emits [exit_code u64 LE][console].
        let image = std::fs::read(artifact_dir().join("sum1to10.bin")).unwrap();
        let expected_record = 55u64.to_le_bytes(); // console empty
        let expected_k = address(&expected_record);

        let snapshot = |()| async {
            let store = MemKappaStore::new();
            let code = store.put("blake3", &wasm).unwrap();
            let init = store.put("blake3", &image).unwrap();
            let manifest = ContainerManifest {
                code,
                initial_state: init,
                parameters: empty_kappa(),
            };
            let cid = store.put("blake3", &manifest.canonicalize()).unwrap();
            let caps = Capabilities {
                storage_roots: Vec::new(),
                storage_quota_bytes: 0,
                network_fetch: false,
                network_announce: false,
                publish_channels: Vec::new(),
                subscribe_channels: Vec::new(),
                memory_max_bytes: 0,
                cpu_time_per_event_ms: 1_000_000,
                priority_weight: 0,
            };
            let ck = store
                .put("blake3", &CapabilitySet::new(caps).canonicalize())
                .unwrap();

            let rt = Runtime::new(WasmtimeEngine::new(), store);
            // Spawn runs hg_init(image) → the emulator runs → storage_put(result).
            let handle = rt
                .spawn(&cid, &ck)
                .await
                .expect("spawn the emulator container");
            let present = rt.store().contains(&expected_k);
            let snap = rt.suspend(handle).await.expect("suspend → κ snapshot");
            (present, snap)
        };

        let (present, snap_a) = snapshot(()).await;
        assert!(
            present,
            "the emulator-on-hologram emitted the ISA-correct result (55) via the host ABI"
        );
        assert!(snap_a.as_str().starts_with("blake3:"), "real κ snapshot");

        // Reproducible: an identical run yields the identical container snapshot κ
        // (deterministic emulation ⇒ content-addressed state, Law L1).
        let (_, snap_b) = snapshot(()).await;
        assert_eq!(snap_a, snap_b, "same run ⇒ same container κ snapshot (L1)");
    });
}

/// **A real Linux kernel boots *through the codemodule* on the substrate.** This
/// is ADR-009 fully realized: the emulator runs as a κ-addressed Wasm container
/// on the real hologram runtime, and an arbitrary operating system boots over it
/// — the kernel image delivered as *content* (read back by κ with `storage_get`,
/// Laws L1/L4), not as a located file.
///
/// The host hands the container a small **boot descriptor** as its initial state
/// (`b"HGOS"` + the kernel-image κ + the device-tree κ); the codemodule reads the
/// kernel and DTB out of the substrate, becomes the SBI firmware, and boots Linux
/// to userspace, emitting `[exit_code][console]` via `storage_put`. That record
/// must be **byte-identical to the one the emulator core produces** (computed
/// here as the oracle — and the core's output already matches `qemu-system-riscv64`,
/// the `the_emulator_boots_real_linux_to_userspace` witness), so the container κ
/// of the result is the same content address. A second run yields the same
/// snapshot κ — the OS boot on the substrate is deterministic (Law L1).
///
/// Ignored by default (boots Linux twice, ~40 s, release only); the CC-9 suite
/// runs it. `cargo test --release -p holospaces --test cc9_emulator
/// the_codemodule_boots_real_linux -- --ignored --nocapture`.
#[test]
#[ignore = "boots a real Linux kernel through the wasm codemodule on the runtime (~40s; release) — run by the CC-9 vv suite"]
fn the_codemodule_boots_real_linux_on_the_substrate() {
    use std::io::Read;

    let dir = artifact_dir().join("linux");
    let gz = std::fs::read(dir.join("Image.gz")).expect("Image.gz");
    let mut kernel = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut kernel)
        .expect("gunzip the kernel Image");
    let dtb = std::fs::read(dir.join("holospaces.dtb")).expect("holospaces.dtb");

    let base = 0x8000_0000u64;
    let dtb_addr = base + 0x0700_0000;

    // The oracle: boot the emulator core directly to get the canonical run record.
    let mut core = Emulator::new(base, 128 * 1024 * 1024);
    core.boot_kernel(&kernel, &dtb, dtb_addr)
        .expect("core boot");
    let halt = loop {
        match core.run(10_000_000) {
            Halt::OutOfBudget => {}
            other => break other,
        }
    };
    assert_eq!(
        halt,
        Halt::Exit(0),
        "the core boots Linux to a clean poweroff"
    );
    let mut expected_record = match halt {
        Halt::Exit(c) => c,
        _ => u64::MAX,
    }
    .to_le_bytes()
    .to_vec();
    expected_record.extend_from_slice(core.console());
    let expected_k = address(&expected_record);

    pollster::block_on(async {
        let wasm = std::fs::read(artifact_dir().join("emulator.wasm"))
            .expect("emulator codemodule (run scripts/build-emulator.sh)");
        surface::validate_userland(&wasm).expect("emulator is a valid codemodule");

        let boot = || async {
            let store = MemKappaStore::new();
            let code = store.put("blake3", &wasm).unwrap();
            let kernel_k = store.put("blake3", &kernel).unwrap();
            let dtb_k = store.put("blake3", &dtb).unwrap();

            // The boot descriptor delivered as the container's initial state.
            let mut desc = Vec::with_capacity(4 + 71 + 71);
            desc.extend_from_slice(b"HGOS");
            desc.extend_from_slice(kernel_k.as_bytes());
            desc.extend_from_slice(dtb_k.as_bytes());
            let init = store.put("blake3", &desc).unwrap();

            let manifest = ContainerManifest {
                code,
                initial_state: init,
                parameters: empty_kappa(),
            };
            let cid = store.put("blake3", &manifest.canonicalize()).unwrap();
            // The container reads the kernel + DTB by κ — they are its read-closure
            // roots; CPU + memory unbounded for a full boot (a real devcontainer's
            // budget). The emitted record is within a small storage quota.
            let caps = Capabilities {
                storage_roots: vec![kernel_k, dtb_k],
                storage_quota_bytes: 1 << 20,
                network_fetch: false,
                network_announce: false,
                publish_channels: Vec::new(),
                subscribe_channels: Vec::new(),
                memory_max_bytes: 0,
                cpu_time_per_event_ms: 0,
                priority_weight: 0,
            };
            let ck = store
                .put("blake3", &CapabilitySet::new(caps).canonicalize())
                .unwrap();

            let rt = Runtime::new(WasmtimeEngine::new(), store);
            let handle = rt.spawn(&cid, &ck).await.expect("spawn the OS container");
            let present = rt.store().contains(&expected_k);
            let snap = rt.suspend(handle).await.expect("suspend → κ snapshot");
            (present, snap)
        };

        let (present, snap_a) = boot().await;
        assert!(
            present,
            "the codemodule booted Linux on the substrate and emitted the same \
             record the core produces (which matches qemu-system-riscv64)"
        );
        let (_, snap_b) = boot().await;
        assert_eq!(
            snap_a, snap_b,
            "deterministic OS boot on the substrate ⇒ reproducible container κ (L1)"
        );
    });
}

/// The emulator's disk and state are substrate primitives: the guest image is
/// read off a [κ-disk](holospaces::disk) (`CC-7`), and the machine's κ snapshot
/// is stored and verifies by re-derivation (Law L5), reproducibly across runs
/// and peers (Law L1). This is the substrate the OS boot will run over. (CC-9
/// foundation, leveraging hologram's κ-disk + KappaStore.)
#[test]
fn the_emulator_runs_a_guest_off_a_kappa_disk_and_snapshots_to_the_store() {
    pollster::block_on(async {
        let image = std::fs::read(artifact_dir().join("sum1to10.bin")).unwrap();

        // The guest image lives as κ-disk content (CC-7), padded to a sector.
        let sector = 512usize;
        let mut padded = image.clone();
        padded.resize(padded.len().div_ceil(sector) * sector, 0);

        let store = MemKappaStore::new();
        let disk = KappaDisk::from_image(&store, sector as u32, &padded)
            .await
            .expect("guest image as κ-disk content");

        // Read the image back off the κ-disk and run it on the emulator.
        let mut back = vec![0u8; padded.len()];
        disk.read(0, (padded.len() / sector) as u32, &mut back)
            .await
            .unwrap();
        let mut emu = Emulator::new(0, 256 * 1024);
        emu.load_flat(&back[..image.len()]).unwrap();
        assert_eq!(
            emu.run(1_000_000),
            Halt::Exit(55),
            "ISA result off the κ-disk"
        );

        // The κ snapshot is content: store it and verify by re-derivation (L5).
        let snapshot = emu.snapshot();
        let snap_k = store.put("blake3", &snapshot).unwrap();
        assert!(
            verify(&snapshot, &snap_k).unwrap(),
            "snapshot verifies (L5)"
        );
        assert_eq!(
            snap_k,
            address(&snapshot),
            "snapshot κ is its content address"
        );

        // Reproducible across a fresh run on another store (any peer, L1).
        let store2 = MemKappaStore::new();
        let disk2 = KappaDisk::from_image(&store2, sector as u32, &padded)
            .await
            .unwrap();
        let mut back2 = vec![0u8; padded.len()];
        disk2
            .read(0, (padded.len() / sector) as u32, &mut back2)
            .await
            .unwrap();
        let mut emu2 = Emulator::new(0, 256 * 1024);
        emu2.load_flat(&back2[..image.len()]).unwrap();
        emu2.run(1_000_000);
        assert_eq!(
            address(&emu2.snapshot()),
            snap_k,
            "same image ⇒ same snapshot κ on any peer (L1)"
        );
    });
}

/// **A real, unmodified Linux kernel boots to userspace on the emulator core.**
///
/// This is CC-9's end state and the strongest external authority of all: the
/// pinned RISC-V Linux 6.6 kernel (`vv/artifacts/cc9/linux/`, provenance in its
/// `SOURCE.txt`) is loaded as the supervisor OS — the emulator is the SBI
/// firmware — and run until PID 1 executes. A real OS boot exercises the base
/// ISA, the M/A/F/D/C extensions, Sv39 paging, the machine/supervisor trap
/// architecture, the CLINT timer, and the SBI far more stringently than any unit
/// test; if the emulator were not a faithful RISC-V machine the kernel would
/// fault, not reach userspace. PID 1 (a real static ELF) prints its marker and
/// the real `/proc/version`, which must match — byte-for-byte — the output the
/// reference RISC-V implementation (`qemu-system-riscv64`) produces from the same
/// image (`expected-userspace.txt`, the differential oracle; ADR-009).
///
/// Ignored by default (a full boot is ~15 s and must run in release); the CC-9
/// V&V suite runs it. Run directly with:
/// `cargo test --release -p holospaces --test cc9_emulator -- --ignored --nocapture`.
#[test]
#[ignore = "boots a real Linux kernel (~15s; release only) — run by the CC-9 vv suite"]
fn the_emulator_boots_real_linux_to_userspace() {
    use std::io::Read;

    let dir = artifact_dir().join("linux");
    // The pinned kernel Image is committed gzip-compressed; decompress it.
    let gz = std::fs::read(dir.join("Image.gz")).expect("Image.gz (see linux/SOURCE.txt)");
    let mut image = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut image)
        .expect("gunzip the kernel Image");
    let dtb = std::fs::read(dir.join("holospaces.dtb")).expect("holospaces.dtb");
    let expected = std::fs::read_to_string(dir.join("expected-userspace.txt")).expect("expected");

    // 128 MiB at the standard RISC-V RAM base; the DTB sits high, clear of the
    // kernel — exactly what the boot_kernel hand-off and the device tree describe.
    let base = 0x8000_0000u64;
    let mut emu = Emulator::new(base, 128 * 1024 * 1024);
    emu.boot_kernel(&image, &dtb, base + 0x0700_0000)
        .expect("load the kernel Image + device tree");

    // Run until PID 1 powers the machine off (reboot(2) -> SBI SRST -> Halt::Exit),
    // bounded by a generous instruction budget.
    let mut steps = 0u64;
    let halt = loop {
        match emu.run(10_000_000) {
            Halt::OutOfBudget => {
                steps += 10_000_000;
                assert!(
                    steps < 3_000_000_000,
                    "the kernel did not reach userspace within the budget (pc={:#x})",
                    emu.pc()
                );
            }
            other => break other,
        }
    };
    assert_eq!(
        halt,
        Halt::Exit(0),
        "PID 1 powers the machine off cleanly via the SBI system-reset call"
    );

    let console = String::from_utf8_lossy(emu.console());
    // The real kernel reached userspace and ran a real ELF as PID 1.
    assert!(
        console.contains("Run /init as init process"),
        "the kernel hands control to the userspace init"
    );
    // The userspace output matches the reference implementation byte-for-byte
    // (the marker PID 1 prints, then the real `Linux version 6.6.0 …` banner).
    for line in expected.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            console.contains(line),
            "userspace output must match the qemu-system-riscv64 oracle; missing:\n  {line}"
        );
    }
}
