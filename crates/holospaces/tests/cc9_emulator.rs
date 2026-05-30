//! `CC-9` (in progress) — the system-emulator core conforms to the RISC-V ISA
//! (arc42 chapter 10, Conformance catalog; ADR-009).
//!
//! CC-9's end state is "a real operating system boots and runs on the emulator."
//! That is reached conformance-first: the emulator is grown against the
//! https://riscv.org/technical/specifications/[RISC-V] ISA as its external
//! authority, exactly as `CC-5` is grown against the WebAssembly spec suite. This
//! witness is the foundation step — the [emulator](holospaces::emulator) core
//! executes **real RISC-V machine code** (assembled by LLVM's RISC-V backend,
//! `vv/artifacts/cc9/`, in the self-checking style of riscv-tests) and reproduces
//! the ISA-defined result exactly.
//!
//! The full-OS boot, the κ-disk-as-disk and console-over-channels integration,
//! and the QEMU differential are the subsequent CC-9 steps; until a real OS
//! boots, `vv/run.sh` reports CC-9 *pending* (this is the cargo-tier ISA witness,
//! not yet the CC-9 suite).

use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use holospaces::disk::{BlockDevice, KappaDisk};
use holospaces::emulator::{Emulator, Halt};
use holospaces::substrate::KappaStore;
use holospaces::{address, verify};

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
