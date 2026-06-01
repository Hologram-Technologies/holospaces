import { spawn } from "node:child_process";
import { chromium } from "playwright";
const port = 3221;
// A real, arbitrary, web-compatible extension from Open VSX (a theme — declarative,
// neutral, not a holospaces default).
const srv = spawn("npx", ["--no-install","@vscode/test-web","--browser","none","--port",String(port),"--extensionId","dracula-theme.theme-dracula"], { cwd: process.cwd() });
let log=""; srv.stdout.on("data",d=>log+=d); srv.stderr.on("data",d=>log+=d);
const up = await new Promise(r=>{const t=setInterval(()=>{if(/Listening on/.test(log)){clearInterval(t);r(true);}},300); setTimeout(()=>{clearInterval(t);r(/Listening on/.test(log));},240000);});
console.log("server up:", up); if(!up){console.log(log.slice(-600)); process.exit(1);}
const browser = await chromium.launch();
try {
  const page = await (await browser.newContext()).newPage();
  await page.goto(`http://localhost:${port}/`, {timeout:30000});
  await page.waitForSelector(".monaco-workbench", {timeout:60000});
  // Open the Extensions view and look for the installed extension.
  await page.keyboard.press("Control+Shift+X");
  await new Promise(r=>setTimeout(r,4000));
  const present = await page.waitForFunction(()=>/Dracula/i.test(document.body.innerText), null, {timeout:25000}).then(()=>true).catch(()=>false);
  console.log("arbitrary extension installed + present in workbench:", present);
  console.log(present?"GAL-OK":"GAL-NO");
  if(!present){ await page.screenshot({path:"/tmp/gal.png"}); }
} finally { await browser.close(); srv.kill("SIGKILL"); }
