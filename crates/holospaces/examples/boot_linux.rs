//! Boot a real RISC-V Linux `Image` + DTB on the emulator core (CC-9/CC-11 bring-up).
//! `boot_linux <Image> <dtb> [max_steps] [input-after:MARKER]`
use std::io::Write;
fn main() {
    let mut a = std::env::args().skip(1);
    let image = std::fs::read(a.next().unwrap()).unwrap();
    let dtb = std::fs::read(a.next().unwrap()).unwrap();
    let max_steps: u64 = a.next().map_or(20_000_000_000, |s| s.parse().unwrap());
    // optional "input-after:MARKER" — once MARKER appears, feed the rest as console input.
    let feed = a.next();
    let base = 0x8000_0000u64;
    let ram = 128 * 1024 * 1024;
    let dtb_addr = base + 0x0700_0000;
    let mut emu = holospaces::emulator::Emulator::new(base, ram);
    emu.boot_kernel(&image, &dtb, dtb_addr).unwrap();
    let (marker, input): (Vec<u8>, Vec<u8>) = match &feed {
        Some(s) => {
            let (m, i) = s.split_once(':').unwrap();
            (m.as_bytes().to_vec(), i.replace("\\n", "\n").into_bytes())
        }
        None => (Vec::new(), Vec::new()),
    };
    let mut fed = input.is_empty();
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
        if !fed && con.windows(marker.len()).any(|w| w == marker.as_slice()) {
            emu.feed_console(&input);
            fed = true;
            eprintln!("\n[fed {} bytes of input]", input.len());
        }
        match h {
            holospaces::emulator::Halt::OutOfBudget => {
                if steps >= max_steps {
                    eprintln!("\n[out of steps at {steps}]");
                    return;
                }
            }
            other => {
                eprintln!("\n[HALT {:?} at ~{} steps]", other, steps);
                return;
            }
        }
    }
}
