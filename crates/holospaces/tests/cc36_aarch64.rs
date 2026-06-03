//! `CC-36` — a real `arm64` Linux kernel boots to userspace on the AArch64
//! emulator (ADR-021, arc42 ch.10).
//!
//! The implementation under test is the privileged AArch64 system
//! ([`holospaces::emulator::aarch64`]): the EL0/EL1 exception model, VMSAv8-64
//! paging, and the ARM `virt` platform (GICv2, the generic timer, a PL011
//! console, PSCI). The authority is a real, unmodified `arm64` Linux 6.6 kernel
//! (`vv/artifacts/cc36/linux/Image.gz`) — the most stringent A64 + privileged
//! correctness test — with `qemu-system-aarch64 -M virt` as the differential
//! oracle (`vv/artifacts/cc36/linux/expected-userspace.txt`,
//! `vv/suites/cc36-aarch64-linux.sh`). The kernel boots over the holospaces
//! devicetree, reaches `Run /init`, and PID 1 prints its marker + the real
//! `/proc/version`, byte-identical to qemu.

use std::io::Read;
use std::path::Path;

use holospaces::emulator::aarch64::{Cpu, Halt};

fn kernel_image() -> Vec<u8> {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc36/linux/Image.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc36 Image.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel");
    img
}

#[test]
#[ignore = "boots a real arm64 Linux to userspace (~release) — run by the CC-36 vv suite"]
fn the_emulator_boots_real_arm64_linux_to_userspace() {
    let kernel = kernel_image();
    let mut cpu = Cpu::boot_linux(512 * 1024 * 1024, &kernel, "earlycon console=ttyAMA0");
    let halt = cpu.run(20_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    // Show the boot log for diagnosis if the marker is missing.
    eprintln!("---- guest console ----\n{console}\n---- end ----  (halt: {halt:?})");

    // The kernel reached userspace, ran PID 1, and powered off via PSCI (the
    // init's `reboot` → `PSCI SYSTEM_OFF` → the emulator halts).
    assert!(
        console.contains("Run /init as init process"),
        "the kernel handed control to PID 1"
    );
    assert_eq!(
        halt,
        Halt::Exit(0),
        "PID 1 powered the machine off via PSCI (a clean shutdown)"
    );

    // The differential oracle: the userspace marker + the real /proc/version the
    // emulator produced must be byte-identical to what `qemu-system-aarch64 -M
    // virt` printed booting the same image (captured in expected-userspace.txt).
    let expected = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vv/artifacts/cc36/linux/expected-userspace.txt"),
    )
    .expect("read the qemu oracle");
    for line in expected.lines() {
        assert!(
            console.contains(line),
            "emulator userspace output matches the qemu oracle, missing line:\n  {line}"
        );
    }
    assert!(
        console.contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "PID 1 printed its marker"
    );
}
