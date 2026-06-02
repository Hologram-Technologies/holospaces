//! `CC-28` — the control plane reconfigures a running holospace over the
//! substrate; configuration is content (arc42 ch.10; ADR-018).
//!
//! A Codespaces/Gitpod control panel reconfigures a *running* environment —
//! lifecycle, storage, network, account/user. holospaces does this UOR-native:
//! the panel produces a κ-addressed [`Configuration`] (embedding the issuing
//! operator and the target instance), publishes it over the substrate, and the
//! instance *resolves* it (verify-by-re-derivation, Law L5) and *applies* it —
//! its state changes. No server, no control-plane→instance RPC.
//!
//! The external authority is the substrate's content-addressing + sync contract
//! (a real loopback HTTP content-addressed gateway, verify-on-receipt — as in
//! the `e2e` roster-sync witness) and, for the live network directive, a real
//! host `TcpListener` bound on the running machine (the `CC-21` ingress dual).
//! The Codespaces/Gitpod control-panel *reconfigure* UX is the behavioural model.

use std::net::TcpStream;
use std::sync::Arc;

use hologram_net_http::live::{serve_addr, HttpKappaSync};
use hologram_runtime::Runtime;
use hologram_runtime_wasmtime::WasmtimeEngine;
use hologram_store_mem::MemKappaStore;
use holospaces::boot::Phase;
use holospaces::config::{ConfigError, Directive, LifecycleAction};
use holospaces::emulator::net::{StdEgress, StdIngress};
use holospaces::emulator::Emulator;
use holospaces::identity::Operator;
use holospaces::manager::Manager;
use holospaces::peer::Peer;
use holospaces::substrate::{Capabilities, KappaStore};
use holospaces::{address, Source};

const CONTAINER_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "hg_init")     (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_event")    (param i32 i32) (result i32) (i32.const 0))
  (func (export "hg_suspend")  (result i32) (i32.const 0))
  (func (export "hg_resume")   (result i32) (i32.const 0))
  (func (export "hg_callback") (param i32 i32 i32) (result i32) (i32.const 0)))
"#;

fn caps() -> Capabilities {
    Capabilities {
        storage_roots: Vec::new(),
        storage_quota_bytes: 0,
        network_fetch: false,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: 4 << 20,
        cpu_time_per_event_ms: 1000,
        priority_weight: 0,
    }
}

/// The full control-plane reconfiguration flow over the **real** substrate, the
/// two-peer way (as a Codespaces panel and its running environment are distinct):
/// the control plane (peer A) provisions an instance and publishes a
/// `Configuration` spanning all four operation classes; the instance side (peer
/// B, same operator) resolves it over a real content-addressed gateway
/// (verify-on-receipt, Law L5) and applies it — its effective state changes.
#[test]
fn the_control_plane_reconfigures_an_instance_over_the_substrate() {
    pollster::block_on(async {
        let operator = Operator::from_public_key(b"operator-self-sovereign-key");

        // ── Control plane (peer A): provision an instance, then configure it. ──
        let runtime_a = Runtime::new(WasmtimeEngine::new(), MemKappaStore::new());
        let code = runtime_a
            .store()
            .put("blake3", &wat::parse_str(CONTAINER_WAT).unwrap())
            .unwrap();
        let peer_a = Peer::new(runtime_a.store(), &runtime_a);
        let mut manager_a = Manager::sign_in(peer_a, operator.clone());
        let instance = manager_a
            .provision(Source::Userland { entry: code }, caps())
            .expect("provision the instance");

        // The panel reconfigures the running instance across the four classes:
        // lifecycle (resume), network (forward 8080 + enable fetch), storage
        // (quota), account/user (grant a collaborator).
        let collaborator = Operator::from_public_key(b"a-collaborator")
            .identity()
            .to_owned();
        let config_kappa = manager_a
            .configure(
                &instance,
                vec![
                    Directive::Lifecycle(LifecycleAction::Resume),
                    Directive::ForwardPort(8080),
                    Directive::SetNetwork {
                        fetch: true,
                        announce: false,
                    },
                    Directive::SetStorageQuota(2 << 30),
                    Directive::GrantAccess(collaborator),
                ],
            )
            .expect("publish the configuration");
        // The control plane records what it reconfigured the instance to.
        assert_eq!(manager_a.configuration_of(&instance), Some(config_kappa));

        // Serve A's store as an *untrusted* content-addressed gateway (the
        // "hologram network" the configuration travels over).
        let gateway: Arc<dyn KappaStore> = runtime_a.store_arc();
        let server = serve_addr(gateway, "127.0.0.1:0", false).expect("serve HTTP-CAS");

        // ── Instance side (peer B): resolve the configuration and apply it. ──
        let runtime_b = Runtime::new(WasmtimeEngine::new(), MemKappaStore::new());
        let sync = HttpKappaSync::new(vec![server.addr().to_string()]);
        let peer_b = Peer::new(runtime_b.store(), &runtime_b).with_sync(&sync);
        let manager_b = Manager::sign_in(peer_b, operator.clone());

        let config = manager_b
            .resolve_configuration(&config_kappa)
            .await
            .expect("resolve the configuration over the substrate (verify L5)");
        assert_eq!(config.instance(), &instance, "it targets this instance");
        assert_eq!(
            config.operator(),
            operator.identity(),
            "issued by the operator"
        );

        // Apply it: the instance's effective state changes, authority-checked.
        let applied = config
            .apply(&instance, &[*operator.identity()], &caps())
            .expect("the owner is authorized for its own instance");
        assert_eq!(applied.lifecycle, Some(LifecycleAction::Resume));
        assert_eq!(applied.forward_ports, vec![8080]);
        assert!(applied.capabilities.network_fetch);
        assert_eq!(applied.capabilities.storage_quota_bytes, 2 << 30);
        assert_eq!(
            applied.grants,
            vec![Operator::from_public_key(b"a-collaborator")
                .identity()
                .to_owned()],
            "the account/user grant is honoured"
        );
    });
}

/// Reconfiguration is reproducible and authority-scoped (Law L1 / L3): the same
/// directives from the same operator for the same instance yield the same
/// configuration κ; a configuration for a *different* instance, or from an
/// *unauthorized* operator, is refused (never silently applied).
#[test]
fn reconfiguration_is_reproducible_and_authority_scoped() {
    let operator = Operator::from_public_key(b"owner").identity().to_owned();
    let instance = address(b"the-instance");
    let directives = vec![
        Directive::ForwardPort(3000),
        Directive::SetStorageQuota(1 << 30),
    ];

    let a = holospaces::config::Configuration::new(operator, instance, 0, directives.clone());
    let b = holospaces::config::Configuration::new(operator, instance, 0, directives);
    assert_eq!(a.kappa(), b.kappa(), "same configuration ⇒ same κ (L1)");

    // Wrong instance is refused.
    assert_eq!(
        a.apply(&address(b"another-instance"), &[operator], &caps())
            .unwrap_err(),
        ConfigError::WrongInstance
    );
    // An unauthorized operator is refused.
    assert_eq!(
        a.apply(&instance, &[address(b"someone-else")], &caps())
            .unwrap_err(),
        ConfigError::UnauthorizedOperator
    );
}

/// The control plane **actually manages** its instance: `Manager::reconfigure`
/// publishes a configuration *and drives the running session with it* — the real
/// lifecycle transitions occur (suspend → κ snapshot, resume, terminate) and a
/// storage directive replaces the instance's effective capability set (a new κ).
/// Not a published intent that nothing obeys: the instance state actually changes.
#[test]
fn the_control_plane_actually_drives_its_managed_instance() {
    pollster::block_on(async {
        let operator = Operator::from_public_key(b"operator-self-sovereign-key");
        let runtime = Runtime::new(WasmtimeEngine::new(), MemKappaStore::new());
        let code = runtime
            .store()
            .put("blake3", &wat::parse_str(CONTAINER_WAT).unwrap())
            .unwrap();
        let peer = Peer::new(runtime.store(), &runtime);
        let mut manager = Manager::sign_in(peer, operator);
        let instance = manager
            .provision(Source::Userland { entry: code }, caps())
            .expect("provision");

        let mut session = manager.open(&instance).await.expect("open");
        session.boot().await.expect("boot");
        assert_eq!(session.phase(), Phase::Running);

        // Suspend FROM THE PANEL — the instance *actually* suspends (κ snapshot).
        manager
            .reconfigure(
                &mut session,
                vec![Directive::Lifecycle(LifecycleAction::Suspend)],
            )
            .await
            .expect("manage: suspend");
        assert_eq!(
            session.phase(),
            Phase::Suspended,
            "the instance actually suspended"
        );
        assert!(
            session.snapshot().is_some(),
            "suspend captured a κ snapshot"
        );

        // Resume from the panel — the instance actually resumes.
        manager
            .reconfigure(
                &mut session,
                vec![Directive::Lifecycle(LifecycleAction::Resume)],
            )
            .await
            .expect("manage: resume");
        assert_eq!(
            session.phase(),
            Phase::Running,
            "the instance actually resumed"
        );

        // A storage directive replaces the effective capability set (new κ, L1).
        let before = session.holospace().kappa();
        manager
            .reconfigure(&mut session, vec![Directive::SetStorageQuota(4 << 30)])
            .await
            .expect("manage: quota");
        assert_ne!(
            session.holospace().kappa(),
            before,
            "the effective capabilities changed — a new κ (Law L1)"
        );
        assert_eq!(session.phase(), Phase::Running);

        // Terminate from the panel — the instance actually ends.
        manager
            .reconfigure(
                &mut session,
                vec![Directive::Lifecycle(LifecycleAction::Terminate)],
            )
            .await
            .expect("manage: terminate");
        assert_eq!(
            session.phase(),
            Phase::Terminated,
            "the instance actually terminated"
        );
    });
}

/// The **live network directive** modifies the *running* machine: the control
/// plane's `ForwardPort` is applied to an already-constructed machine, and the
/// new route is a real, reachable host listener — no reboot. (The dual of the
/// `CC-21` ingress, bound live; ADR-018.)
#[test]
fn a_forward_port_directive_modifies_the_running_machine() {
    // A running machine with networking attached (as after boot).
    let mut machine = Emulator::new(0x8000_0000, 1 << 20);
    machine.attach_net_forward(Box::new(StdEgress::new()), Box::new(StdIngress::new()));

    // The control plane forwards a port on the *running* instance.
    let host_port = machine
        .forward_port(8080)
        .expect("the live forward is bound on the running machine");

    // The new route is real: a connection to the host port is accepted (the
    // running instance now forwards it — its state changed, live).
    TcpStream::connect(("127.0.0.1", host_port))
        .expect("the live-forwarded port is reachable on the running machine");
}
