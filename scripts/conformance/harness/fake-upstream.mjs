// Fake chatgpt.com for scripts/conformance/wire-capture.sh. It records every
// HTTP request, WebSocket handshake and message, and each HTTP/2 client's
// SETTINGS to CAPTURE_LOG, and answers Responses requests with a tool call
// first and a final message once a tool output comes back, so a native Codex
// and a Codex behind the pool both run a two-request turn. It speaks HTTP/2
// with HTTP/1.1 allowed, as the real backend does; REJECT_WS=1 refuses the
// WebSocket upgrade so a native Codex falls back to HTTP.
//
// It answers in the shapes Codex expects, but it is a recorder, not a model of
// the backend's answers: how the pool reads upstream is checked against
// Codex's own source by codex-conformance.sh.
import http2 from "node:http2";
import fs from "node:fs";
import crypto from "node:crypto";
import zlib from "node:zlib";

const LOG = process.env.CAPTURE_LOG || "/logs/capture.jsonl";
const SEND_METADATA = process.env.SEND_METADATA !== "0";
let seq = 0;
const record = (entry) => fs.appendFileSync(LOG, JSON.stringify({ t: new Date().toISOString(), ...entry }) + "\n");

function pickTool(body) {
  const names = (body.tools || []).map((t) => t.name || t.type);
  for (const name of ["exec_command", "shell_command", "shell", "local_shell"]) if (names.includes(name)) return name;
  return null;
}
function toolArgs(name) {
  if (name === "exec_command") return { cmd: "echo hi" };
  if (name === "shell_command") return { command: "echo hi" };
  return { command: ["echo", "hi"] };
}
function hasToolOutput(body) {
  return (body.input || []).some((item) => item && /_output$/.test(item.type || ""));
}

function events(body) {
  const n = ++seq;
  const id = `resp_fake_${n}`;
  const model = body.model || "unknown";
  const out = [];
  out.push({ type: "response.created", response: { id, object: "response", status: "in_progress", model } });
  if (SEND_METADATA) out.push({ type: "response.metadata", response_id: id, metadata: { "x-codex-turn-state": `ts-${n}`, "openai-model": model } });
  let item;
  if (body.generate === false) {
    item = null;
  } else if (!hasToolOutput(body) && pickTool(body)) {
    const name = pickTool(body);
    item = { type: "function_call", id: `fc_${n}`, call_id: `call_${n}`, name, arguments: JSON.stringify(toolArgs(name)), status: "completed" };
  } else {
    item = { type: "message", id: `msg_${n}`, role: "assistant", status: "completed", content: [{ type: "output_text", text: "done", annotations: [] }] };
  }
  if (item) {
    out.push({ type: "response.output_item.added", output_index: 0, item });
    out.push({ type: "response.output_item.done", output_index: 0, item });
  }
  out.push({ type: "response.completed", response: { id, object: "response", status: "completed", model, output: item ? [item] : [], usage: { input_tokens: 10, input_tokens_details: { cached_tokens: 0 }, output_tokens: 5, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 15 } } });
  return { n, out };
}

function wsSend(socket, text) {
  const payload = Buffer.from(text);
  let header;
  if (payload.length < 126) header = Buffer.from([0x81, payload.length]);
  else if (payload.length < 65536) { header = Buffer.alloc(4); header[0] = 0x81; header[1] = 126; header.writeUInt16BE(payload.length, 2); }
  else { header = Buffer.alloc(10); header[0] = 0x81; header[1] = 127; header.writeBigUInt64BE(BigInt(payload.length), 2); }
  socket.write(Buffer.concat([header, payload]));
}

function handleUpgrade(req, socket) {
  record({ kind: "ws-handshake", method: req.method, path: req.url, headers: req.headers, rawHeaderOrder: req.rawHeaders.filter((_, i) => i % 2 === 0) });
  if (process.env.REJECT_WS === "1") { socket.end("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n"); return; }
  const accept = crypto.createHash("sha1").update(req.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\nx-codex-turn-state: ts-ws\r\nopenai-model: fake\r\n\r\n`);
  let buffer = Buffer.alloc(0);
  let fragments = [];
  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      if (buffer.length < 2) return;
      const fin = (buffer[0] & 0x80) !== 0;
      const opcode = buffer[0] & 0x0f;
      const masked = (buffer[1] & 0x80) !== 0;
      let len = buffer[1] & 0x7f;
      let offset = 2;
      if (len === 126) { if (buffer.length < 4) return; len = buffer.readUInt16BE(2); offset = 4; }
      else if (len === 127) { if (buffer.length < 10) return; len = Number(buffer.readBigUInt64BE(2)); offset = 10; }
      const maskLen = masked ? 4 : 0;
      if (buffer.length < offset + maskLen + len) return;
      const mask = masked ? buffer.subarray(offset, offset + 4) : null;
      const data = Buffer.from(buffer.subarray(offset + maskLen, offset + maskLen + len));
      if (mask) for (let i = 0; i < data.length; i++) data[i] ^= mask[i % 4];
      buffer = buffer.subarray(offset + maskLen + len);
      if (opcode === 0x8) { socket.end(); return; }
      if (opcode === 0x9) { socket.write(Buffer.from([0x8a, 0])); continue; }
      if (opcode === 0x1 || opcode === 0x2 || opcode === 0x0) {
        fragments.push(data);
        if (!fin) continue;
        const text = Buffer.concat(fragments).toString("utf8");
        fragments = [];
        let body = null;
        try { body = JSON.parse(text); } catch { body = { raw: text.slice(0, 2000) }; }
        const { n, out } = events(body);
        record({ kind: "ws-message", path: req.url, n, body });
        for (const event of out) wsSend(socket, JSON.stringify(event));
      }
    }
  });
  socket.on("error", () => {});
}

const server = http2.createSecureServer({ allowHTTP1: true, key: fs.readFileSync("/certs/server.key"), cert: fs.readFileSync("/certs/server.pem") }, (req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    let buffer = Buffer.concat(chunks);
    const encoding = req.headers["content-encoding"] || "";
    if (encoding === "zstd") buffer = zlib.zstdDecompressSync(buffer);
    const raw = buffer.toString("utf8");
    let body = null;
    try { body = raw ? JSON.parse(raw) : null; } catch { body = { raw: raw.slice(0, 2000) }; }
    if (req.method === "POST" && /\/responses$/.test(req.url.split("?")[0])) {
      const { n, out } = events(body || {});
      record({ kind: "http", proto: req.httpVersion, method: req.method, path: req.url, n, headers: req.headers, rawHeaderOrder: req.rawHeaders.filter((_, i) => i % 2 === 0), encoding, wireBytes: Buffer.concat(chunks).length, body });
      res.writeHead(200, { "content-type": "text/event-stream", "x-codex-turn-state": `ts-${n}`, "openai-model": (body && body.model) || "fake", "x-reasoning-included": "true" });
      for (const event of out) res.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
      res.end();
      return;
    }
    record({ kind: "http-other", method: req.method, path: req.url, headers: req.headers, bodyKeys: body && typeof body === "object" ? Object.keys(body) : null });
    if (/accounts\/check/.test(req.url)) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ accounts: [{ id: "acct-capture", plan_type: "pro", workspace_backend_origin: "NO_CONSTRAINT", account_routing_override: "NO_CONSTRAINT", structure: "personal" }], account_ordering: ["acct-capture"], default_account_id: "acct-capture" })); return; }
    if (/\/models/.test(req.url)) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ models: [] })); return; }
    res.writeHead(404, { "content-type": "application/json" });
    res.end("{}");
  });
});
server.on("upgrade", handleUpgrade);
server.on("session", (session) => {
  session.on("remoteSettings", (settings) => record({ kind: "h2-settings", from: session.socket && session.socket.remoteAddress, settings }));
});
server.listen(443, () => console.log("fake upstream listening"));
