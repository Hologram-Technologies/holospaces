// CC-49 — two browser peers exchange κ-content over a REAL WebRTC data channel.
//
// CC-38 proved the uor-native content-network PROTOCOL (BareNetSync over a
// portable NetworkInterface, verify-on-receipt) is identical on every surface,
// with an in-test pump standing in for the link. This witness closes the named
// transport frontier (CC-38 / ADR-006): the SAME protocol is carried between two
// browser peers over a genuine RTCDataChannel — peer-to-peer, NO central
// operator (Law L1, UOR-native: no server).
//
// Two separate browser CONTEXTS (A and B), each a `window.hs.Console` with its
// own content store, are connected by a real WebRTC data channel (`WebRtcLink`).
// Signaling (the SDP offer/answer + ICE candidates) is exchanged OUT OF BAND by
// this harness shuttling the pasted blobs between the two pages — there is no
// signaling server, exactly as two operators pasting offers to each other. The
// content exchange itself runs through the PRODUCT pump `Console.cn_pump` (a
// Rust/wasm method, not harness glue): it drains each peer's outbound
// content-network frames onto the data channel and feeds the channel's inbound
// frames back into the peer's `BareNetSync`, so this witness exercises exactly
// the path a deployed tab uses. Then:
//
// The FULL BareNetSync frame set (fetch / announce / discover) crosses the real
// channel through the product API — not just fetch:
//
//   • B ANNOUNCES (over the channel, via Console.cn_announce + cn_pump) that it
//     holds a κ;
//   • A DISCOVERS the holder (over the channel, via Console.cn_discover + cn_pump)
//     — A learns B's κ from B's discover reply, having held nothing itself;
//   • A then FETCHES that κ over the data channel; the bytes are accepted ONLY
//     after they re-derive to the requested κ (verify-on-receipt / Law L5);
//   • a forging responder's bytes (which do not re-derive) are REJECTED;
//   • an unheld κ resolves to NOTHING (no fabrication);
//   • the exchange is SYMMETRIC (B fetches from A as well).
//
// Real WebRTC across two headless-Chromium contexts runs over host (loopback)
// ICE candidates — no STUN/TURN, no server. If the channel cannot open in this
// environment the witness FAILS loudly; it never fakes the result.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css" };

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("WEBRTC-TEST: FAIL —", m)));

// Serve the web peer assets over http://127.0.0.1 (a secure context — WebRTC and
// wasm need one).
const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const url = `http://127.0.0.1:${port}/index.html`;

const browser = await chromium.launch();

// Two independent browser contexts — two distinct peers.
const ctxA = await browser.newContext();
const ctxB = await browser.newContext();
const pageA = await ctxA.newPage();
const pageB = await ctxB.newPage();
for (const [name, p] of [["A", pageA], ["B", pageB]]) {
  p.on("pageerror", (e) => ((failed = true), console.error(`WEBRTC-TEST: pageerror (${name}) —`, e.message)));
}

// Bridge a console.log from inside a page to the harness for diagnosis.
pageA.on("console", (m) => { if (m.type() === "error") console.error("  [A]", m.text()); });
pageB.on("console", (m) => { if (m.type() === "error") console.error("  [B]", m.text()); });

// Connect a real WebRTC data channel between an offerer page and an answerer
// page, shuttling SDP + ICE through THIS harness (out-of-band signaling, no
// server). Returns once both ends report the channel open.
async function connect(offerer, answerer) {
  // 1) offerer creates its link + SDP offer.
  const offer = await offerer.evaluate(async () => {
    window.__link = new window.hs.WebRtcLink(true); // initiator
    return await window.__link.create_offer();
  });
  // 2) answerer accepts the offer, returns the answer SDP.
  const answer = await answerer.evaluate(async (offerSdp) => {
    window.__link = new window.hs.WebRtcLink(false); // answerer
    return await window.__link.accept_offer(offerSdp);
  }, offer);
  // 3) offerer accepts the answer.
  await offerer.evaluate(async (answerSdp) => { await window.__link.accept_answer(answerSdp); }, answer);

  // 4) Trickle ICE: both ends gather host candidates over a few turns; the
  //    harness carries each side's candidates to the other (the signaling relay
  //    a pair of operators would do by hand — content-blind, no operator role).
  const drainIce = async (p) => p.evaluate(() => window.__link.take_ice());
  const addIce = async (p, cands) =>
    p.evaluate(async (cs) => { for (const c of cs) await window.__link.add_ice(c); }, cands);

  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    await addIce(answerer, await drainIce(offerer));
    await addIce(offerer, await drainIce(answerer));
    const [oOpen, aOpen] = await Promise.all([
      offerer.evaluate(() => window.__link.is_open()),
      answerer.evaluate(() => window.__link.is_open()),
    ]);
    if (oOpen && aOpen) return true;
    await new Promise((r) => setTimeout(r, 100));
  }
  // Flush any last candidates, then check once more.
  await addIce(answerer, await drainIce(offerer));
  await addIce(offerer, await drainIce(answerer));
  const [oOpen, aOpen] = await Promise.all([
    offerer.evaluate(() => window.__link.is_open()),
    answerer.evaluate(() => window.__link.is_open()),
  ]);
  return oOpen && aOpen;
}

// Drive a fetch on `fetcher` for `kappa` while servicing `responder`, until done.
//
// Both sides cross the data channel through the PRODUCT pump — `Console.cn_pump`
// (a Rust/wasm method, not harness glue): it drains the peer's outbound
// content-network frames onto the `WebRtcLink` and delivers the channel's inbound
// frames into the peer, in one product call. The harness only re-polls and steps
// the two pages; the transport wiring (cn_outbound → channel, channel →
// cn_inbound) is the product's. So a real deployed tab uses exactly this path.
async function drive(fetcherPage, responderPage, kappa) {
  // Start the fetch (non-blocking) and step both sides in lockstep.
  await fetcherPage.evaluate((k) => { window.__console.cn_fetch_start(k); }, kappa);
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    // Fetcher: product pump (outbound → channel, channel → inbound), then poll.
    const r = await fetcherPage.evaluate(() => {
      const c = window.__console;
      c.cn_pump(window.__link); // PRODUCT transport pump (CC-49)
      const out = c.cn_fetch_poll();
      return out === undefined ? "pending" : out === null ? "null" : Array.from(out);
    });
    // Responder: product pump services the inbound request and sends its reply.
    await responderPage.evaluate(() => { window.__console.cn_pump(window.__link); });
    if (r === "pending") { await new Promise((res) => setTimeout(res, 10)); continue; }
    return r === "null" ? null : r;
  }
  return null;
}

// Drive ANNOUNCE + DISCOVER over the data channel, entirely through the product
// API. `announcer` calls `Console.cn_announce(kappa)` (queues a KIND_ANNOUNCE
// frame); `discoverer` calls `Console.cn_discover()` (broadcasts KIND_DISCOVER_REQ
// and snapshots κs learned from peers' KIND_DISCOVER_RES). Both sides cross the
// channel through the SAME product pump `Console.cn_pump` — no harness glue. The
// harness only steps the two pages and re-snapshots until the discoverer learns
// `wantKappa` (or a deadline, fail-loud). Returns the list of κs the discoverer
// learned over the channel.
async function discover(discovererPage, announcerPage, wantKappa, announceKappa) {
  // The announcer advertises the κ it holds (KIND_ANNOUNCE) over the channel.
  await announcerPage.evaluate((k) => {
    window.__console.cn_announce(k);
    window.__console.cn_pump(window.__link); // PRODUCT pump: announce → channel
  }, announceKappa);
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    // Discoverer: broadcast DISCOVER_REQ + snapshot, then pump it onto the channel.
    const known = await discovererPage.evaluate(() => {
      const c = window.__console;
      const snap = JSON.parse(c.cn_discover()); // PRODUCT discover (sends REQ)
      c.cn_pump(window.__link); // PRODUCT pump: REQ → channel, RES → peer
      return snap;
    });
    // Announcer: pump services the inbound DISCOVER_REQ and sends DISCOVER_RES.
    await announcerPage.evaluate(() => { window.__console.cn_pump(window.__link); });
    if (known.includes(wantKappa)) return known;
    await new Promise((res) => setTimeout(res, 10));
  }
  // Final snapshot after a last round-trip.
  return await discovererPage.evaluate(() => {
    window.__console.cn_pump(window.__link);
    return JSON.parse(window.__console.cn_discover());
  });
}

try {
  await pageA.goto(url);
  await pageB.goto(url);
  await pageA.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });
  await pageB.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  // ── Honest peers: A fetches a κ that B holds, over a real data channel ──────
  // Each page gets a Console; B publishes the content and announces its κ.
  const kappa = await pageB.evaluate(() => {
    window.__console = new window.hs.Console();
    const bytes = new TextEncoder().encode(
      "uor-native content carried peer-to-peer over a real WebRTC data channel"
    );
    return window.__console.cn_put(bytes);
  });
  await pageA.evaluate(() => { window.__console = new window.hs.Console(); });

  const connectedAB = await connect(pageA, pageB); // A offers, B answers
  check(connectedAB, "a real WebRTC data channel opened between two browser peers (host ICE, no server)");

  if (connectedAB) {
    // ── Announce + discover over the channel (product API) ───────────────────
    // B announces the κ it holds; A discovers it over the data channel. A holds
    // NOTHING locally, so a discovered κ can only have come from B's reply across
    // the real channel.
    const known = await discover(pageA, pageB, kappa, kappa);
    check(
      known.includes(kappa),
      "peer A discovered the κ peer B announced — announce + discover crossed the data channel (product API)"
    );

    const got = await drive(pageA, pageB, kappa);
    const text = got ? new TextDecoder().decode(new Uint8Array(got)) : null;
    check(
      text === "uor-native content carried peer-to-peer over a real WebRTC data channel",
      "peer A fetched content-addressed bytes from peer B over the data channel (accepted after re-derivation)"
    );

    // ── Unheld κ resolves to nothing (no fabrication) ────────────────────────
    const unheld = await pageA.evaluate(() => window.hs.kappa(new TextEncoder().encode("content no peer holds")));
    const absent = await drive(pageA, pageB, unheld);
    check(absent === null, "a κ no peer holds resolves to nothing over the channel (no fabrication)");

    // ── Symmetric: B fetches a κ that A holds ────────────────────────────────
    const kappaA = await pageA.evaluate(() => {
      const bytes = new TextEncoder().encode("content held by peer A, fetched by peer B — symmetric");
      return window.__console.cn_put(bytes);
    });
    const gotBA = await drive(pageB, pageA, kappaA);
    const textBA = gotBA ? new TextDecoder().decode(new Uint8Array(gotBA)) : null;
    check(
      textBA === "content held by peer A, fetched by peer B — symmetric",
      "the exchange is symmetric — peer B fetched from peer A over the same channel"
    );
  }

  // ── Forging responder is rejected ──────────────────────────────────────────
  // A fresh pair: F is a FORGING peer that answers every fetch with bytes that do
  // not re-derive to the requested κ. The fetcher must reject them on receipt.
  const pageF = await (await browser.newContext()).newPage();
  pageF.on("pageerror", (e) => ((failed = true), console.error("WEBRTC-TEST: pageerror (F) —", e.message)));
  pageF.on("console", (m) => { if (m.type() === "error") console.error("  [F]", m.text()); });
  const pageV = await (await browser.newContext()).newPage(); // victim/fetcher
  pageV.on("pageerror", (e) => ((failed = true), console.error("WEBRTC-TEST: pageerror (V) —", e.message)));
  await pageF.goto(url);
  await pageV.goto(url);
  await pageF.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });
  await pageV.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });

  // The κ the victim asks for — the address of the GENUINE content (which the
  // forger never holds). The forger fabricates a different payload for it.
  const targetKappa = await pageV.evaluate(() => {
    window.__console = new window.hs.Console();
    return window.hs.kappa(new TextEncoder().encode("the genuine content the victim requested"));
  });
  await pageF.evaluate(() => {
    // A forging Console: answers EVERY fetch with attacker bytes that do not
    // re-derive to the requested κ.
    window.__console = window.hs.Console.new_forging(
      new TextEncoder().encode("FORGED bytes that do not re-derive to the requested kappa")
    );
  });

  const connectedVF = await connect(pageV, pageF); // V offers, F answers
  check(connectedVF, "a real WebRTC data channel opened to the forging peer");
  if (connectedVF) {
    const forged = await drive(pageV, pageF, targetKappa);
    check(
      forged === null,
      "a forging responder's bytes are rejected on re-derivation — the fetch yields nothing (Law L5)"
    );
  }

  console.log(
    failed
      ? "WEBRTC-TEST: FAILED"
      : "WEBRTC-TEST: PASS (two browser peers ran the full content-network frame set — announce + discover + fetch — over a real WebRTC data channel; forgery rejected; unheld κ absent; symmetric)"
  );
} finally {
  await browser.close();
  server.close();
}

process.exit(failed ? 1 : 0);
