//! `CC-60` — the x86-64 core's **port-forward + loopback reachability control
//! plane** is at parity with the riscv/aarch64 cores (`CC-16` + `CC-21` + `CC-33`).
//!
//! A docker **server** image is only useful if its listening socket is reachable.
//! The riscv core reaches a guest server two ways, both over the shared
//! [`net`](holospaces::emulator::net) NAT: an external host-socket forward
//! ([`Emulator::attach_net_forward`] + [`net::StdIngress`], `CC-21`) and the
//! in-process loopback bridge ([`Emulator::enable_loopback`] / `dial_guest` /
//! `guest_send` / `guest_recv`, `CC-33`). The x86-64 core already had the loopback
//! bridge; this witnesses the newly-added [`Cpu::attach_net_forward`] (the external
//! forward parity) and that the whole reachability control plane functions on the
//! x64 core — **without** a multi-minute distro boot.
//!
//! Scope (honest): this witnesses the *control plane* (the device + bridge a host
//! preview drives) — `attach_net_forward` installs the NIC, the loopback bridge
//! enables, and a dial allocates an open connection toward a guest port. The full
//! *behavioural* proof — a real amd64 server brought up over virtio-net actually
//! returning bytes to a host client — is the heavy on-demand target
//! `vv/targets/cc60-x64-net-reachable-server.sh` (RED until an amd64 server fixture
//! exists; the x64 virtio-net path has not yet been driven by a real guest).

use holospaces::emulator::net::{NoEgress, NoIngress};
use holospaces::emulator::x64::Cpu;

/// The x64 reachability control plane is at parity: with no NIC there is no
/// loopback bridge (a dial is refused); after [`Cpu::attach_net_forward`] installs
/// the device with a forwarded-port ingress, the in-process loopback bridge enables
/// and a dial toward a guest port allocates an open connection — the exact surface
/// a host "running-app preview" drives. No boot required.
#[test]
fn x64_port_forward_and_loopback_control_plane_is_at_parity() {
    let mut cpu = Cpu::new(64 * 1024 * 1024);

    // No network device yet: the loopback bridge cannot attach and a dial is refused.
    assert!(
        !cpu.enable_loopback(),
        "with no NIC attached, the loopback bridge does not enable"
    );
    assert!(
        cpu.dial_guest(80).is_none(),
        "with no loopback bridge, a dial toward the guest is refused"
    );

    // attach_net_forward installs the NIC with an outbound egress AND a
    // forwarded-port ingress (the CC-21 external-forward parity the x64 core lacked).
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(NoIngress));

    // The in-process loopback bridge (CC-33) now enables on the attached device,
    // and a dial toward a guest port allocates an open connection.
    assert!(
        cpu.enable_loopback(),
        "attach_net_forward installs the NIC, so the loopback bridge enables"
    );
    let id = cpu
        .dial_guest(80)
        .expect("a dial toward the guest's port allocates a connection id");
    assert!(
        cpu.guest_is_open(id),
        "the freshly dialed loopback connection is open"
    );

    // The host side can write a request toward the guest server (no guest is running
    // in this control-plane witness, so no reply is asserted — that is the heavy
    // behavioural target). The send must not panic or close the connection.
    cpu.guest_send(id, b"GET / HTTP/1.0\r\n\r\n");
    assert!(
        cpu.guest_is_open(id),
        "the connection stays open after a host-side send"
    );
    let _ = cpu.guest_recv(id); // empty (no guest server) — drains without error
    cpu.guest_close(id); // closing the host side does not panic (the guest-side
                         // teardown completes when the machine is next pumped).
}

/// `attach_net` (egress only) and `attach_net_forward` (egress + ingress) both
/// install the network device, so the loopback bridge enables after either — the
/// two entry points are interchangeable for in-process reachability, differing only
/// in whether an external host-socket forward is also wired.
#[test]
fn attach_net_and_attach_net_forward_both_install_the_device() {
    let mut a = Cpu::new(32 * 1024 * 1024);
    a.attach_net(Box::new(NoEgress));
    assert!(a.enable_loopback(), "attach_net installs the NIC");

    let mut b = Cpu::new(32 * 1024 * 1024);
    b.attach_net_forward(Box::new(NoEgress), Box::new(NoIngress));
    assert!(b.enable_loopback(), "attach_net_forward installs the NIC");
}
