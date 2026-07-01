// link-preview-server.mjs — serve the web/ dir with COOP/COEP + correct MIME so resume-link.html
// can stream a κ-bundle and resume in a real browser tab. κ-objects (link-bundle/k/*) have no
// extension → served as octet-stream. Dev-only preview server.
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
const WEB = path.dirname(fileURLToPath(import.meta.url));
const TYPE = { ".html":"text/html", ".js":"text/javascript", ".mjs":"text/javascript",
  ".wasm":"application/wasm", ".css":"text/css", ".json":"application/json" };
const PORT = Number(process.env.PORT || 8099);
http.createServer(async (req, res) => {
  try {
    let p = decodeURIComponent((req.url || "/").split("?")[0]);
    if (p === "/") p = "/resume-link.html";
    const body = await readFile(path.join(WEB, p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPE[path.extname(p)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("not found"); }
}).listen(PORT, () => console.log("link-preview on http://localhost:" + PORT));
