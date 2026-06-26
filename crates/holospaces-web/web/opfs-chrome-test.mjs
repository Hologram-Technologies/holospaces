// Does createSyncAccessHandle work in headless Chrome (Playwright) on THIS machine? Isolates CEF vs Chrome.
// Serves a COI page (COOP same-origin + COEP require-corp), runs the SAME blob-worker SyncAccessHandle test
// as scratchpad/m1-opfs.mjs.
import http from "node:http";
import { chromium } from "playwright";

const PAGE = `<!doctype html><meta charset=utf8><title>opfs</title><body>opfs test</body>`;
const server = http.createServer((req, res) => {
  res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
  res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
  res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
  res.setHeader("Content-Type", "text/html");
  res.end(PAGE);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("console", (m) => { if (m.type() === "error") console.error("  [page]", m.text()); });
await page.goto(`http://127.0.0.1:${port}/`);
const coi = await page.evaluate(() => self.crossOriginIsolated + " sab=" + (typeof SharedArrayBuffer));
console.log("page crossOriginIsolated:", coi);

const result = await page.evaluate(async () => {
  const code = "onmessage=async()=>{try{const r=await navigator.storage.getDirectory();"
    + "const h=await r.getFileHandle('t'+Math.random(),{create:true});"
    + "const a=await h.createSyncAccessHandle();a.write(new Uint8Array([1,2,3]),{at:0});a.flush();a.close();"
    + "postMessage('OK coi='+self.crossOriginIsolated);}catch(e){postMessage('ERR '+String(e&&e.name)+': '+String(e&&e.message));}};";
  return await new Promise((res) => {
    const w = new Worker(URL.createObjectURL(new Blob([code], { type: "text/javascript" })));
    w.onmessage = (e) => res(e.data);
    w.onerror = (e) => res("WORKER-ERR " + (e.message || ""));
    w.postMessage("go");
    setTimeout(() => res("TIMEOUT"), 8000);
  });
});
console.log("CHROME createSyncAccessHandle:", result);
console.log("Chrome version:", browser.version());
await browser.close(); server.close();
process.exit(0);
