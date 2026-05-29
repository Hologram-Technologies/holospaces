# Introduction and Goals

holospaces is a UOR-native *boot layer* over the
[hologram](https://github.com/Hologram-Technologies/hologram) substrate.
It provisions and runs *holospaces* — bootable, content-addressed
environments that range from a single compute artifact to a full Linux
development environment — across every environment hologram supports
(browser, native, bare-metal), holding all state as content. Its
first-party holospace, *Hologram*, is the *Platform Manager*: the
operator console that provisions and manages the others. For the
development-environment use case it is, in effect, a UOR-native,
serverless [Gitpod](https://www.gitpod.io) /
[Codespaces](https://github.com/features/codespaces).

This documentation is **authoritative for holospaces only**. Where it
refers to hologram,
[UOR-ADDR](https://github.com/UOR-Foundation/uor-addr), or
[Prism](https://github.com/UOR-Foundation/prism), it **links** to those
projects rather than restating their internals — their APIs and
guarantees are defined there, not here.

## Requirements Overview

holospaces must let an operator provision, boot, and manage holospaces,
uniformly, with no server.

| \#  | Requirement                                                                                                                                                                     |
|-----|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| R1  | Provision a holospace from a **holo-file** (a `.holo` compute artifact, e.g. a model compiled upstream by [hologram-ai](https://github.com/Hologram-Technologies/hologram-ai)). |
| R2  | Provision a holospace from a **git repository + devcontainer config** (a [Dev Container](https://containers.dev) / Codespaces-like experience).                                 |
| R3  | Manage a holospace’s **lifecycle**: boot, suspend (to a κ snapshot), resume, migrate, terminate.                                                                                |
| R4  | Run holospaces across the environments hologram supports: **browser**, **native**, **bare-metal**.                                                                              |
| R5  | **Sign-in** with a self-sovereign identity so an operator’s instances synchronise their holospaces over the substrate.                                                          |
| R6  | Serve the **Hologram** platform (the Platform Manager) from this repository’s **GitHub Pages**.                                                                                 |

## Quality Goals

The top quality goals are the project’s invariants (the "laws"); they
constrain every design decision (see Chapter 2, Architecture
Constraints).

| \#  | Quality Goal                | Motivation                                                                                                          |
|-----|-----------------------------|---------------------------------------------------------------------------------------------------------------------|
| Q1  | **Content, not location**   | Identity is a κ-label (*what* a thing is), never a host/path/URL. No servers.                                       |
| Q2  | **Canonical forms only**    | Every operation is canonical-in → canonical-out; holospaces holds κ-labels, not deserialized objects.               |
| Q3  | **The store is the memory** | The content-addressed store is the address space; RAM is a cache. A holospace far larger than RAM remains bootable. |
| Q4  | **Reproducibility**         | A holospace’s identity is the κ of its definition: the same definition always yields the same holospace.            |
| Q5  | **Verifiability**           | Every received byte is accepted only after re-derivation against its κ; trust is in the math, not the source.       |
| Q6  | **Portability**             | The same holospace κ boots on any peer, in any supported environment.                                               |

## Stakeholders

| Role                                                                                                                                       | Expectation                                                                                                                                          |
|--------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Operator**                                                                                                                               | Provision and manage holospaces from a familiar console; sign in once and find their holospaces on every instance.                                   |
| **Contributors / agents**                                                                                                                  | A single authoritative specification (this documentation) with no gaps, inconsistencies, or assumptions; every implemented feature links back to it. |
| **Upstream projects** ([hologram](https://github.com/Hologram-Technologies/hologram), [UOR-Foundation](https://github.com/UOR-Foundation)) | holospaces consumes their published interfaces and describes them by reference, never forking or restating their internals.                          |
