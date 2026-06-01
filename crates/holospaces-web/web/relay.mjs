// CC-16 (browser) — the egress relay: a content-blind TCP-over-WebSocket proxy.
//
// The browser peer's userspace TCP/IP NAT cannot open a raw socket from a tab, so
// it tunnels each guest TCP connection's payload over a WebSocket to this relay,
// which opens the real TCP socket and pumps bytes both ways (ADR-014). The relay
// is the egress *gateway* — the network analogue of the Pages cold-start gateway:
// it carries opaque byte streams and terminates no holospaces semantics.
//
// Pure Node (http + net + crypto) — a minimal RFC 6455 server, no dependencies,
// matching the repo's vendored-not-installed discipline. The framing multiplexes
// connections by id: →relay OPEN(1)/DATA(2)/CLOSE(3); ←relay OPENED(0x11)/
// DATA(0x12)/CLOSED(0x13)/FAILED(0x14).
//
// A REDIRECT env (e.g. "10.0.2.9:8080=127.0.0.1:9100") port-forwards a
// guest-visible address to a host one — the same guestfwd the native StdEgress
// witness uses, so the same guest software reaches a controlled local server.
import http from "node:http";
import net from "node:net";
import crypto from "node:crypto";

const OP_OPEN = 0x01, OP_DATA = 0x02, OP_CLOSE = 0x03;
const OP_OPENED = 0x11, OP_RDATA = 0x12, OP_CLOSED = 0x13, OP_FAILED = 0x14;
const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// Parse the redirect table (guest ip:port → host ip:port).
const redirects = new Map();
for (const r of (process.env.REDIRECT || "").split(",").filter(Boolean)) {
  const [from, to] = r.split("=");
  redirects.set(from, to);
}

// Build a single unmasked binary WebSocket frame (server→client).
function wsFrame(payload) {
  const len = payload.length;
  let header;
  if (len < 126) {
    header = Buffer.from([0x82, len]);
  } else if (len < 65536) {
    header = Buffer.from([0x82, 126, (len >> 8) & 0xff, len & 0xff]);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x82;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(len), 2);
  }
  return Buffer.concat([header, payload]);
}

const server = http.createServer((_req, res) => res.writeHead(426).end("WebSocket only"));

server.on("upgrade", (req, socket) => {
  const key = req.headers["sec-websocket-key"];
  const accept = crypto.createHash("sha1").update(key + WS_GUID).digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\n" +
      "Upgrade: websocket\r\nConnection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );

  const conns = new Map(); // id → net.Socket
  const send = (buf) => socket.write(wsFrame(buf));
  const ctrl = (op, id) => {
    const b = Buffer.alloc(5);
    b[0] = op;
    b.writeUInt32BE(id >>> 0, 1);
    return send(b);
  };

  // Demultiplex one application frame from the client.
  const onAppFrame = (msg) => {
    if (msg.length < 5) return;
    const op = msg[0];
    const id = msg.readUInt32BE(1);
    if (op === OP_OPEN) {
      let host = `${msg[5]}.${msg[6]}.${msg[7]}.${msg[8]}`;
      let port = msg.readUInt16BE(9);
      const key2 = `${host}:${port}`;
      if (redirects.has(key2)) {
        const [rh, rp] = redirects.get(key2).split(":");
        host = rh;
        port = Number(rp);
      }
      const sock = net.connect(port, host);
      conns.set(id, sock);
      sock.on("connect", () => ctrl(OP_OPENED, id));
      sock.on("data", (d) => {
        const b = Buffer.alloc(5 + d.length);
        b[0] = OP_RDATA;
        b.writeUInt32BE(id >>> 0, 1);
        d.copy(b, 5);
        send(b);
      });
      sock.on("error", () => ctrl(OP_FAILED, id));
      sock.on("close", () => {
        ctrl(OP_CLOSED, id);
        conns.delete(id);
      });
    } else if (op === OP_DATA) {
      conns.get(id)?.write(msg.subarray(5));
    } else if (op === OP_CLOSE) {
      conns.get(id)?.destroy();
      conns.delete(id);
    }
  };

  // Minimal RFC 6455 frame reader (client→server frames are masked).
  let buf = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const opcode = buf[0] & 0x0f;
      const masked = (buf[1] & 0x80) !== 0;
      let len = buf[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        off = 10;
      }
      const maskLen = masked ? 4 : 0;
      if (buf.length < off + maskLen + len) return;
      let payload = buf.subarray(off + maskLen, off + maskLen + len);
      if (masked) {
        const mask = buf.subarray(off, off + 4);
        const out = Buffer.alloc(len);
        for (let i = 0; i < len; i++) out[i] = payload[i] ^ mask[i & 3];
        payload = out;
      }
      buf = buf.subarray(off + maskLen + len);
      if (opcode === 0x8) {
        socket.end();
        return;
      } else if (opcode === 0x9) {
        socket.write(Buffer.concat([Buffer.from([0x8a, 0]), payload])); // pong
      } else if (opcode === 0x2 || opcode === 0x1) {
        onAppFrame(payload);
      }
    }
  });

  const teardown = () => {
    for (const s of conns.values()) s.destroy();
    conns.clear();
  };
  socket.on("close", teardown);
  socket.on("error", teardown);
});

const port = Number(process.env.RELAY_PORT || 0);
export function startRelay() {
  return new Promise((resolve) => {
    server.listen(port, "127.0.0.1", () => resolve(server));
  });
}

// When run directly, start and print the port (for manual use).
if (import.meta.url === `file://${process.argv[1]}`) {
  startRelay().then((s) => console.log(`relay listening on ${s.address().port}`));
}
