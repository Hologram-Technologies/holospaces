//! Boot a real Linux kernel that mounts its root filesystem over the emulator's
//! VirtIO block device, via the Boot Orchestrator (CC-14 bring-up / differential
//! harness). The device tree is generated in-crate.
//! Usage: virtio_boot <kernel Image> <rootfs.ext4> [max_steps]

use holospaces::machine::MachineSpec;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let kernel = std::fs::read(&a[1]).expect("kernel");
    let rootfs = std::fs::read(&a[2]).expect("rootfs");
    let max_steps: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(600_000_000);

    let mut emu = MachineSpec::devcontainer()
        .boot(&kernel, rootfs)
        .expect("boot");
    let halt = emu.run(max_steps);
    eprintln!("=== halt: {halt:?} ===");
    eprint!("{}", String::from_utf8_lossy(emu.console()));
}
