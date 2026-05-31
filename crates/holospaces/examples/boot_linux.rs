//! Boot a real RISC-V Linux `Image` + DTB on the emulator core (CC-9 bring-up).
use std::io::Write;
fn main() {
    let mut a = std::env::args().skip(1);
    let image = std::fs::read(a.next().unwrap()).unwrap();
    let dtb = std::fs::read(a.next().unwrap()).unwrap();
    let max_steps: u64 = a.next().map_or(20_000_000_000, |s| s.parse().unwrap());
    let base = 0x8000_0000u64;
    let ram = 128 * 1024 * 1024;
    let dtb_addr = base + 0x0700_0000;
    let mut emu = holospaces::emulator::Emulator::new(base, ram);
    emu.boot_kernel(&image, &dtb, dtb_addr).unwrap();
    let mut last = 0usize;
    let mut steps = 0u64;
    let chunk = 5_000_000u64;
    loop {
        let h = emu.run(chunk);
        steps += chunk;
        let con = emu.console();
        if con.len() > last {
            std::io::stdout().write_all(&con[last..]).ok();
            std::io::stdout().flush().ok();
            last = con.len();
        }
        match h {
            holospaces::emulator::Halt::OutOfBudget => {
                if con.windows(12).any(|w| w == b"USERSPACE-OK") {
                    eprintln!("\n[boot] *** userspace marker reached (~{steps} steps) ***");
                    return;
                }
                if steps >= max_steps {
                    eprintln!("\n[boot] out of steps at {steps}, pc={:#x}", emu.pc());
                    return;
                }
            }
            other => {
                eprintln!(
                    "\n[boot] HALT {:?} at ~{} steps pc={:#x}",
                    other,
                    steps,
                    emu.pc()
                );
                return;
            }
        }
    }
}
