//! `CC-62` — the x86-64 core executes **real userland workloads correctly**: a
//! battery of deterministic shell commands fed to the warm Alpine `.holo` shell
//! produces **byte-exact, POSIX-correct** output, with **zero kernel panics**.
//!
//! This is the durable correctness gate the "run any docker image" promise rests
//! on. It is *behavioral* (the repo's established x86-64 authority — CC-44 uses
//! qemu as the differential): each command's stdout is compared against its
//! spec-defined result (independent of our emulator), so a silent miscompute in
//! ANY layer — instruction decode/execute, flags, SSE, the syscall ABI, the kernel
//! tty/pipe/fork paths — diverges the output and fails the gate. Kernel-mode code
//! is covered for free: every pipe/fork/read/write/sort here drives real kernel
//! paths, not just userspace.
//!
//! Provenance of the expected values: POSIX / coreutils-busybox defined behavior,
//! cross-checkable against `qemu-x86_64 -L <cc45-rootfs> busybox sh -c '<cmd>'`
//! (the CC-44 authority; see scratchpad/goldens.sh). Format-variable busybox
//! output (wc padding, sha256sum trailer) is asserted semantically; everything
//! else is byte-exact.
//!
//! HARNESS DISCIPLINE (the CC-61.5 lesson — see memory holo-x64-printf-escape-crash):
//! commands are Rust **raw strings** (`r"printf 'x\n'"`), so a literal backslash-n
//! reaches the shell. NEVER feed `b"..\\n.."` — that collapses to a real newline
//! and tests a malformed command, not the one you meant.

use std::path::Path;

use holospaces::emulator::x64::Cpu;

fn kblob() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../holospaces-web/web/fixtures/x64-alpine-shell.kblob"),
    )
    .expect("read warm x64 shell kblob")
}

fn console(cpu: &Cpu) -> String {
    String::from_utf8_lossy(cpu.console()).into_owned()
}

/// Feed one command line (literal bytes + newline) to the warm ash shell, run until
/// the `holo$ ` prompt returns (or a panic), and return `(panicked, stdout)` where
/// `stdout` is the command's output with the echoed command line and trailing
/// prompt stripped, `\r` removed.
fn run(cpu: &mut Cpu, cmd: &str) -> (bool, String) {
    let before = console(cpu).len();
    let mut bytes = cmd.as_bytes().to_vec();
    bytes.push(b'\n');
    cpu.feed_console(&bytes);
    let mut panicked = false;
    for _ in 0..400 {
        let _ = cpu.run(1_000_000);
        let c = console(cpu);
        if c.len() > before && c[before..].contains("Kernel panic") {
            panicked = true;
            break;
        }
        if c.len() > before && c[before..].matches("holo$ ").count() >= 1 {
            break;
        }
    }
    let c = console(cpu);
    let delta = c[before.min(c.len())..].replace('\r', "");
    // Strip the echoed command line (first line) and the trailing `holo$ ` prompt.
    let mut out = delta.as_str();
    if let Some(nl) = out.find('\n') {
        if out[..nl].trim_end() == cmd {
            out = &out[nl + 1..];
        }
    }
    let out = out.strip_suffix("holo$ ").unwrap_or(out);
    // The kernel printk and the command's stdout share this serial console; drop
    // kernel log lines (`[   831.893849] …`) so only the command's output remains.
    let is_klog = |l: &str| {
        let t = l.trim_start();
        t.starts_with('[')
            && t[1..].trim_start().chars().next().is_some_and(|c| c.is_ascii_digit())
            && t.contains("] ")
    };
    let filtered: String = out
        .split_inclusive('\n')
        .filter(|l| !is_klog(l))
        .collect();
    (panicked, filtered)
}

/// Every command's stdout is byte-exact (or semantically exact for format-variable
/// busybox output), and nothing panics. A regression anywhere in the x86-64 core or
/// its kernel ABI fails this gate.
#[test]
fn x64_userland_workloads_are_byte_correct() {
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&kblob()), "restore warm x64 shell");
    // Warm the shell to its first prompt.
    for _ in 0..20 {
        cpu.run(2_000_000);
    }

    // (command, expected-exact-stdout). Raw strings: backslashes are literal.
    let exact: &[(&str, &str)] = &[
        (r"printf 'x\n'", "x\n"),
        (r"printf '%d %x %o\n' 255 255 255", "255 ff 377\n"),
        (r"echo hello world", "hello world\n"),
        (r"printf 'a\tb\n'", "a\tb\n"),
        (r"echo -e 'p\nq'", "p\nq\n"),
        (r"printf '%s-' a b c; echo", "a-b-c-\n"),
        (r"seq 1 5 | tr '\n' ','", "1,2,3,4,5,"),
        (r"printf 'cba' | tr a-z A-Z", "CBA"),
        (r"printf 'c\nb\na\n' | sort", "a\nb\nc\n"),
        (r"echo $((6*7))", "42\n"),
        (r"printf 'a\nbb\nccc\n' | grep -c .", "3\n"),
        (r"printf '1\n2\n3\n' | awk '{s+=$1} END{print s}'", "6\n"),
        (r"seq 1 9 | sed -n '3,5p'", "3\n4\n5\n"),
        (r"printf 'hello' | wc -c", "5\n"),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (cmd, want) in exact {
        let (panicked, got) = run(&mut cpu, cmd);
        if panicked {
            failures.push(format!("{cmd:?}: PANIC"));
            continue;
        }
        if &got != want {
            failures.push(format!("{cmd:?}: got {got:?} want {want:?}"));
        }
    }

    // Format-variable busybox output: assert semantically (independent constants).
    // sha256("hello") is a fixed, well-known digest.
    let (p_sha, sha) = run(&mut cpu, r"printf 'hello' | sha256sum");
    if p_sha {
        failures.push("sha256sum: PANIC".into());
    } else if !sha.contains("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824") {
        failures.push(format!("sha256sum: digest mismatch, got {sha:?}"));
    }

    assert!(
        failures.is_empty(),
        "x86-64 userland correctness regressions ({} of {}):\n  {}",
        failures.len(),
        exact.len() + 1,
        failures.join("\n  ")
    );
}
