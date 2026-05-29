# Conceptual Model

# Conceptual Model

The holospaces conceptual model in Object-Process Methodology (OPM, ISO 19450). The top-level System Diagram (SD) presents holospaces at its highest abstraction; each in-zoom diagram refines one part — **SD1** provisioning, **SD2** the lifecycle (boot · suspend · resume · migrate · terminate), **SD3** identity and sync. Each diagram is bimodal: an Object-Process Diagram (OPD) paired with equivalent Object-Process Language (OPL) sentences.

Scope: the substrate is modeled as a single external object (`Substrate`); its internal pillars are defined by [hologram](https://github.com/Hologram-Technologies/hologram), not here. Peer deployment topology (browser / native / bare-metal) is described in the Deployment View (arc42 chapter 7), not modeled as an OPD.

## SD

![SD](images/opm-SD.svg)

```opl
Operator is environmental and physical.
Holospaces is informatical.
Substrate is informatical.
Holospace is informatical.
Provisioning is informatical.
Booting is informatical.
Holospaces exhibits Provisioning and Booting.
Operator handles Provisioning.
Operator handles Booting.
Provisioning yields Holospace.
Booting requires Holospace.
Booting requires Substrate.
```

## SD1 Provisioning

![SD1 Provisioning](images/opm-SD1-Provisioning.svg)

```opl
Provisioning is informatical.
Holo-File Provisioning is informatical.
Devcontainer Provisioning is informatical.
Holo-File is informatical.
Devcontainer is informatical.
Holospace is informatical.
Provisioning consists of Holo-File Provisioning and Devcontainer Provisioning.
Holo-File Provisioning requires Holo-File.
Devcontainer Provisioning requires Devcontainer.
Holo-File Provisioning yields Holospace.
Devcontainer Provisioning yields Holospace.
```

## SD2 Lifecycle

![SD2 Lifecycle](images/opm-SD2-Lifecycle.svg)

```opl
Holospace is informatical.
Booting is informatical.
Suspending is informatical.
Resuming is informatical.
Migrating is informatical.
Terminating is informatical.
Snapshot is informatical.
Substrate is informatical.
Booting requires Holospace.
Booting requires Substrate.
Suspending requires Holospace.
Suspending yields Snapshot.
Resuming requires Snapshot.
Resuming yields Holospace.
Migrating requires Snapshot.
Migrating yields Holospace.
Terminating requires Holospace.
```

## SD3 Identity

![SD3 Identity](images/opm-SD3-Identity.svg)

```opl
Operator is environmental and physical.
Identity is informatical.
Sign-In is informatical.
Sync is informatical.
Holospace is informatical.
Substrate is informatical.
Operator handles Sign-In.
Sign-In requires Identity.
Sync requires Identity.
Sync requires Substrate.
Sync affects Holospace.
```

