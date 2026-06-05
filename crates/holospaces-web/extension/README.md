# holospaces router — the gateway that carries a browser-tab guest's traffic

A browser tab has no raw sockets, so a holospace's guest (a real Linux + the
devcontainer's binaries, running in the tab) cannot reach the internet on its
own. This extension is the guest's **router** — the gateway that carries
*arbitrary traffic* for *any* holospace out of the sandbox, so package managers,
network configuration, and applications work as they do on a Codespace VM. It is
the one path that needs **no other device at all**:

| Egress surface | How | When |
|---|---|---|
| **holospaces-node** (CC-39) | a flashed device you own forwards guest TCP over a WebSocket | you have a node on your network |
| **mesh** (CC-38) | route over the WebRTC content mesh to an exit peer | a peer in your mesh can exit |
| **this extension** | **Direct Sockets** (`TCPSocket`) opened *locally in the browser* | a self-contained Chromebook — nothing else needed |

The extension is a **local egress node in the browser**. It speaks the *exact
same* egress protocol the browser peer uses for a node (the OPEN/DATA/CLOSE
framing, CC-16; the node implements it with `std::net::TcpStream`,
`crates/holospaces-node/src/egress.rs`) — only here each guest connection is a
`TCPSocket` the extension opens. So `apt`/`pip`/`npm`, a `git` clone, ssh, an
outbound socket all work on a Chromebook with **no node, no relay, no proxy**.

## Why an extension (and not the page)

A page cannot open a raw socket; an MV3 service worker can, via **Direct
Sockets** (`TCPSocket`/`UDPSocket`). That single capability gives the guest
*arbitrary* internet — and it is *all* this extension does. (Direct Sockets is a
powerful, gated capability, enabled out of band — an enterprise
`DirectSocketsAllowedForUrls` policy on a managed device, or `chrome://flags` for
development — not via a broad store permission. Confirm the gating for your Chrome
version.)

It is the **operator's own** extension, installed by them — self-sovereign, like
a node is a device you own. Only the operator's holospaces origin may talk to it
(`externally_connectable`), and it forwards content it cannot perceive (the
egress is content-blind — SEC-7).

## Permissions — minimal by design (fast store review)

The router has two roles and asks only for what each needs — nothing more:

| Power | Requested? | Why |
|---|---|---|
| raw sockets (Direct Sockets) | yes (out-of-band gated) | the **egress** role — the one thing only an extension can do |
| `host_permissions` (`*://*/*`) | yes | the **content** role — a CORS-free `fetch()` to pull the repo's image layers from any registry (Docker Hub/ghcr) the page cannot |
| `externally_connectable` (one origin) | yes | so *only* the operator's holospaces tab can reach it |
| `tabs`, `scripting`, `storage`, `webRequest` | **No** | it never reads/injects a page, holds state, or intercepts requests |
| broad `permissions` block | **No** | not used |

> A router fundamentally fetches arbitrary registries, so `host_permissions` is
> honest for what it does. (An earlier design deferred this as
> `optional_host_permissions` requested at runtime — but `chrome.permissions.request`
> needs a user gesture a message handler doesn't have, so the content role would
> silently fail; declaring it is both correct and reliable.)

## Configure for your deployment

`manifest.json` lists **one** origin in both `externally_connectable.matches` and
`content_scripts.matches` — the project's Pages site. A self-host sets it to its
own origin(s); narrower is faster to review, and only those origins can reach the
router (self-sovereign). `host_permissions` (`*://*/*`) is the content role's host
access (CORS-free registry fetch); scope it to the registries you use
(`https://*.docker.io/*`, `https://ghcr.io/*`, …) if you prefer narrower.

## Files

- `manifest.json` — MV3; **minimal**: no host permissions, no tabs, no storage —
  only raw sockets (out-of-band gated) + the operator's one origin in
  `externally_connectable`.
- `background.js` — the service worker: the egress protocol over `TCPSocket`
  (mirrors the proven node `EgressServer`).
- `connector.js` — the page side: the holospaces tab opens a `chrome.runtime`
  port and hands the guest's egress frames to the extension, exactly as it would
  a node's WebSocket.

## Build the upload package

```sh
scripts/build-extension.sh
# → crates/holospaces-web/extension/dist/holospaces-egress-extension-v<ver>.zip
```

The zip contains **only** the files Chrome loads — `manifest.json`,
`background.js`, and the icons — so it is exactly what the store expects (the
build refuses to ship if a dev/test/page file leaks in, or if the manifest is not
MV3 + minimal-permission). The Pages deploy also builds it and offers it at
`/extension/` for download.

## Publish / install

- **Chrome Developer Console:** upload the `.zip` above.
- **Load unpacked (dev):** `chrome://extensions` → Developer mode → **Load
  unpacked** → this folder.

Then copy the extension id into the holospaces page (or set
`HOLOSPACES_EGRESS_EXTENSION_ID` in `connector.js` before publishing). The
guest's network then exits through the extension's sockets — local, no node.

## Icons

`icons/{16,48,128}.png` are committed source assets, regenerated deterministically
by `icons/gen-icons.py` (a dependency-free raw-PNG generator).

## Verification (CC-41)

The service worker's egress protocol is **witnessed hermetically in V&V**
(`egress-test.mjs`, `vv/suites/cc41-extension-egress.sh`): `TCPSocket` is
polyfilled over `node:net` — faithful to the Direct Sockets contract
(`new TCPSocket(host,port)`, `await .opened → {readable, writable}`, `.close()`)
— `chrome.runtime` is mocked, and the worker is driven against a **real echo
server**. It opens a socket and reports `OPENED`, round-trips `DATA`, and reports
`FAILED` for an unreachable host — proving it is **wire-compatible** with the
browser's `WsEgress` (CC-16) and the node's `EgressServer` (CC-39): same frames,
same behaviour. Direct Sockets itself is a gated capability that needs a real,
configured Chrome to run for real (it cannot run in headless CI), so *that* hop —
the OS opening the actual socket — is exercised manually; the worker's logic is
proven in the gate.

## Integration status

Binding the extension to the browser peer's guest networking is an
**extension-backed `Egress`** (the wasm NAT drains outbound frames to the
connector and feeds inbound frames from it) — the local analogue of `WsEgress`
(which targets a node's WebSocket); the egress *mechanism* is the same one
CC-16/CC-39/CC-41 prove.
