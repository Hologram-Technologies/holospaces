//! Boot a real Linux kernel that mounts its root filesystem over the emulator's
//! VirtIO block device (CC-14 bring-up / differential harness).
//! Usage: virtio_boot <kernel Image> <dtb> <rootfs.ext4> [max_steps]

use holospaces::emulator::Emulator;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let kernel = std::fs::read(&a[1]).expect("kernel");
    let dtb = std::fs::read(&a[2]).expect("dtb");
    let rootfs = std::fs::read(&a[3]).expect("rootfs");
    let max_steps: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(400_000_000);

    let base = 0x8000_0000u64;
    let mut emu = Emulator::new(base, 512 * 1024 * 1024);
    emu.enable_sbi();
    emu.attach_disk(rootfs);
    emu.boot_kernel(&kernel, &dtb, base + 0x0700_0000)
        .expect("boot_kernel");
    let halt = emu.run(max_steps);
    eprintln!("=== halt: {halt:?} ===");
    eprint!("{}", String::from_utf8_lossy(emu.console()));
}
