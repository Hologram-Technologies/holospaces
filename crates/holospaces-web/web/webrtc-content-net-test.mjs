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
// signaling server, exactly as two operators pasting offers to each other. Then:
//
//   • A fetches a κ that B holds, over the data channel; the bytes are accepted
//     ONLY after they re-derive to the requested κ (verify-on-receipt / Law L5);
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

// Pump content-network frames between a fetching page and a responding page over
// their open data channels, polling the fetch to completion. Returns the fetched
// bytes (as a normal array) or null (absent / rejected). The pump only shuttles
// opaque frames; all verification happens inside each peer (verify-on-receipt).
async function fetchOverChannel(fetcherPage, kappa) {
  return await fetcherPage.evaluate(async (k) => {
    const c = window.__console;
    c.cn_fetch_start(k);
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      // Drain this peer's outbound content-network frames onto the data channel.
      let f;
      while ((f = c.cn_outbound()) !== undefined) window.__link.send(f);
      // Deliver any frames the channel received from the peer into this peer.
      let g;
      while ((g = window.__link.recv()) !== undefined) c.cn_inbound(g);
      const r = c.cn_fetch_poll();
      if (r === undefined) { await new Promise((res) => setTimeout(res, 10)); continue; }
      return r === null ? null : Array.from(r);
    }
    return null;
  }, kappa);
}

// The responder must service inbound requests as frames arrive — run a pump on
// the responder page in the background for the duration of a fetch. We do this by
// pumping the responder between fetch polls from the harness side.
async function pumpResponder(responderPage) {
  await responderPage.evaluate(() => {
    const c = window.__console;
    let g;
    while ((g = window.__link.recv()) !== undefined) c.cn_inbound(g);
    let f;
    while ((f = c.cn_outbound()) !== undefined) window.__link.send(f);
  });
}

// Drive a fetch on `fetcher` for `kappa` while pumping `responder`, until done.
async function drive(fetcherPage, responderPage, kappa) {
  // Start the fetch (non-blocking) and step both sides in lockstep.
  await fetcherPage.evaluate((k) => { window.__console.cn_fetch_start(k); }, kappa);
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    // Fetcher: drain outbound → channel, channel → inbound, poll.
    const r = await fetcherPage.evaluate(() => {
      const c = window.__console;
      let f; while ((f = c.cn_outbound()) !== undefined) window.__link.send(f);
      let g; while ((g = window.__link.recv()) !== undefined) c.cn_inbound(g);
      const out = c.cn_fetch_poll();
      return out === undefined ? "pending" : out === null ? "null" : Array.from(out);
    });
    // Responder: drain channel → inbound (services the request), outbound → channel.
    await pumpResponder(responderPage);
    if (r === "pending") { await new Promise((res) => setTimeout(res, 10)); continue; }
    return r === "null" ? null : r;
  }
  return null;
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
      : "WEBRTC-TEST: PASS (two browser peers exchanged κ-content over a real WebRTC data channel; forgery rejected; unheld κ absent; symmetric)"
  );
} finally {
  await browser.close();
  server.close();
}

process.exit(failed ? 1 : 0);
