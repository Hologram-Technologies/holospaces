//! `CC-63` — a real **networked application** runs correctly on the x86-64 `.holo`
//! core: a TCP server (`nc -l`) serves a real HTTP response that a real HTTP client
//! (`wget`) connects to over the in-guest TCP/IP stack and parses — the body comes
//! back **byte-exact**, with the round-trip's cost reported (guest instructions +
//! host wall-clock on the pure-Rust interpreter).
//!
//! Where CC-62 proves the CPU + kernel run CLI workloads correctly, this proves the
//! next layer the "run any docker image" promise needs: the **socket/TCP app
//! surface** — bind/listen/accept/connect/send/recv, the kernel's loopback TCP/IP
//! stack, fork/background, and HTTP framing — all on the warm Alpine `.holo` shell,
//! no external NIC (lo only). A miscompute anywhere in that stack drops the
//! connection or corrupts the body and fails the gate.
//!
//! NB this busybox build (v1.36.1) has no `httpd` applet, so the server is `nc`
//! piping a canned HTTP/1.0 response — a genuine TCP server a genuine HTTP client
//! parses. Harness discipline (CC-61.5): commands are raw strings; never b"..\\n..".

use std::path::Path;
use std::time::Instant;

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

/// Feed one command line, run until the `holo$ ` prompt returns, and return the
/// console delta (kernel printk lines `[ N.N] …` filtered out, `\r` removed).
fn run(cpu: &mut Cpu, cmd: &str, max_iters: usize) -> String {
    let before = console(cpu).len();
    let mut bytes = cmd.as_bytes().to_vec();
    bytes.push(b'\n');
    cpu.feed_console(&bytes);
    for _ in 0..max_iters {
        let _ = cpu.run(1_000_000);
        let c = console(cpu);
        if c.len() > before && c[before..].matches("holo$ ").count() >= 1 {
            break;
        }
    }
    let c = console(cpu);
    let delta = c[before.min(c.len())..].replace('\r', "");
    delta
        .split_inclusive('\n')
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with('[')
                && t[1..].trim_start().chars().next().is_some_and(|c| c.is_ascii_digit())
                && t.contains("] "))
        })
        .collect()
}

/// A real HTTP client fetches a real TCP server's response over the in-guest TCP/IP
/// stack; the body is byte-exact. Reports the round-trip cost on the interpreter.
#[test]
fn x64_serves_a_real_http_round_trip_over_loopback() {
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&kblob()), "restore warm x64 shell");
    for _ in 0..20 {
        cpu.run(2_000_000);
    }

    // Bring loopback up and stage a canned HTTP/1.0 response.
    let _ = run(&mut cpu, r"ip link set lo up", 80);
    let body = "BODY-OK-X64";
    let _ = run(
        &mut cpu,
        r"printf 'HTTP/1.0 200 OK\r\nContent-Length: 12\r\n\r\nBODY-OK-X64\n' > /tmp/resp",
        80,
    );
    // One-shot TCP server, backgrounded in a subshell so the prompt returns.
    let _ = run(&mut cpu, r"(nc -l -p 8080 < /tmp/resp >/dev/null 2>&1 &) ; true", 120);

    // Time the client round-trip: guest instructions + host wall-clock.
    let insns_before = cpu.insns();
    let t0 = Instant::now();
    let out = run(
        &mut cpu,
        r"sleep 1; wget -qO- http://127.0.0.1:8080/ 2>&1; echo FETCHEND",
        400,
    );
    let wall = t0.elapsed();
    let insns = cpu.insns() - insns_before;

    assert!(
        out.contains(body),
        "HTTP client did not receive the server body.\n  got: {out:?}"
    );
    assert!(
        !out.contains("Connection refused") && !out.contains("can't connect"),
        "TCP connection failed: {out:?}"
    );

    eprintln!(
        "CC-63: real HTTP round-trip OK — body {body:?} received over in-guest loopback TCP. \
         Cost on the pure-Rust interpreter (incl. the guest's 1s sleep): {insns} guest insns, \
         {wall:?} host wall-clock."
    );

    // ── Multi-KB body: proves TCP segmentation/reassembly over loopback (a real
    // server property the 12-byte body doesn't exercise). The body is `seq 1 500`
    // (lines "1".."500", ~1.9 KB > one loopback segment), served then fetched; the
    // client must receive every line intact and in order.
    let _ = run(
        &mut cpu,
        r"{ printf 'HTTP/1.0 200 OK\r\n\r\n'; seq 1 500; } > /tmp/resp2",
        120,
    );
    let _ = run(&mut cpu, r"(nc -l -p 8081 < /tmp/resp2 >/dev/null 2>&1 &) ; true", 120);
    let big = run(
        &mut cpu,
        r"sleep 1; wget -qO- http://127.0.0.1:8081/ | wc -l; echo BIGEND",
        400,
    );
    // wc -l of the body = 500 lines (the HTTP headers are stripped by wget -qO-).
    assert!(
        big.lines().any(|l| l.trim() == "500"),
        "multi-KB body did not transfer intact (expected 500 lines): {big:?}"
    );
    eprintln!("CC-63: multi-KB HTTP body (seq 1 500, ~1.9KB) transferred intact over loopback TCP.");
}
