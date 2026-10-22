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
// Which connection each request arrived on, so the comparison can tell a
// client that opens one connection per request (as Codex does) from one that
// reuses connections.
const connections = new WeakMap();
let connectionSeq = 0;
const connectionOf = (key) => {
  if (!key) return null;
  if (!connections.has(key)) connections.set(key, ++connectionSeq);
  return connections.get(key);
};
const requestConnection = (req) => connectionOf((req.stream && req.stream.session) || req.socket);
const record = (entry) => fs.appendFileSync(LOG, JSON.stringify({ t: new Date().toISOString(), ...entry }) + "\n");
const remote = (req) => String((req.socket && req.socket.remoteAddress) || "").replace(/^::ffff:/, "");
const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
// What a token refresh answers: the long-lived token mkauth.mjs writes for the
// same fake identity, byte for byte, so a refreshed pool and a native Codex
// present the same credential.
const refreshedToken = `${b64({ alg: "none", typ: "JWT" })}.${b64({ exp: 4102444800, iat: 1700000000, email: "acct-capture@example.invalid", "https://api.openai.com/auth": { chatgpt_account_id: "acct-capture", chatgpt_plan_type: "pro", chatgpt_user_id: "user-acct-capture" } })}.unsigned`;

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

// SCENARIO selects how the fake answers, for wire-capture.sh's scenario
// phases:
//   subagent  the parent is told to spawn one child (spawn_agent), the child
//             answers, and the parent then finishes
//   compact   every answer reports a large usage, so Codex compacts mid-turn;
//             a compaction request is answered with a compaction item
//   failover  the first account the pool (FAIL_FROM, its address) sends a
//             Responses request or socket for is refused as spent, so the
//             pool moves the request to its other account
const SCENARIO = process.env.SCENARIO || "";
const FAIL_FROM = process.env.FAIL_FROM || "";
let failedAccount = "";

// A subagent's requests name their parent thread (x-codex-parent-thread-id,
// the turn metadata's parent_thread_id) or carry x-openai-subagent.
function isSubagent(body, headers) {
  if (headers && (headers["x-openai-subagent"] || headers["x-codex-parent-thread-id"])) return true;
  let metadata = {};
  try { metadata = JSON.parse((body.client_metadata || {})["x-codex-turn-metadata"] || "{}"); } catch {}
  return Boolean(metadata.parent_thread_id || metadata.subagent_kind);
}
function isCompaction(body) {
  if ((body.input || []).some((item) => item && item.type === "compaction_trigger")) return true;
  let metadata = {};
  try { metadata = JSON.parse((body.client_metadata || {})["x-codex-turn-metadata"] || "{}"); } catch {}
  return metadata.request_kind === "compaction";
}
// Codex 0.163 offers spawn_agent inside the collaboration namespace tool.
function offersSpawnAgent(body) {
  return (body.tools || []).some((tool) => tool && (tool.name === "spawn_agent" || (tool.type === "namespace" && (tool.tools || []).some((inner) => inner && inner.name === "spawn_agent"))));
}
function hasCompacted(body) {
  return (body.input || []).some((item) => item && (item.type === "compaction" || item.type === "compaction_summary"));
}
// The spawn_agent calls this fake has issued. Over the WebSocket Codex sends
// only the new items after previous_response_id, so the call's output arrives
// without the call itself; without this record the parent was told to spawn
// again after every answer and never finished.
const spawnCalls = new Set();
function hasCallOutput(body, name) {
  const calls = new Set((body.input || []).filter((item) => item && item.type === "function_call" && item.name === name).map((item) => item.call_id));
  if (name === "spawn_agent") for (const id of spawnCalls) calls.add(id);
  return (body.input || []).some((item) => item && /_output$/.test(item.type || "") && calls.has(item.call_id));
}
// Whether the pool's request for this account must be refused (failover).
function refuseForFailover(remoteAddress, account) {
  if (SCENARIO !== "failover" || !FAIL_FROM || remoteAddress !== FAIL_FROM || !account) return false;
  if (!failedAccount) failedAccount = account;
  return account === failedAccount;
}

function events(body, headers) {
  const n = ++seq;
  const id = `resp_fake_${n}`;
  const model = body.model || "unknown";
  const out = [];
  out.push({ type: "response.created", response: { id, object: "response", status: "in_progress", model } });
  if (SEND_METADATA) out.push({ type: "response.metadata", response_id: id, metadata: { "x-codex-turn-state": `ts-${n}`, "openai-model": model } });
  let item;
  const tools = (body.tools || []).map((tool) => tool.name || tool.type);
  if (body.generate === false) {
    item = null;
  } else if (SCENARIO === "compact" && isCompaction(body)) {
    item = { type: "compaction", id: `cmp_${n}`, encrypted_content: "capture-compaction" };
  } else if (SCENARIO === "subagent" && isSubagent(body, headers)) {
    item = { type: "message", id: `msg_${n}`, role: "assistant", status: "completed", content: [{ type: "output_text", text: "CHILD_OK", annotations: [] }] };
  } else if (SCENARIO === "subagent" && offersSpawnAgent(body) && !hasCallOutput(body, "spawn_agent")) {
    const namespaced = !tools.includes("spawn_agent");
    spawnCalls.add(`call_${n}`);
    item = { type: "function_call", id: `fc_${n}`, call_id: `call_${n}`, name: "spawn_agent", ...(namespaced ? { namespace: "collaboration" } : {}), arguments: JSON.stringify({ message: "Reply with CHILD_OK.", task_name: "child_probe", fork_turns: "none" }), status: "completed" };
  } else if (SCENARIO === "subagent") {
    item = { type: "message", id: `msg_${n}`, role: "assistant", status: "completed", content: [{ type: "output_text", text: "PARENT_OK", annotations: [] }] };
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
  // Large only until the conversation has been compacted once, so Codex
  // compacts one time rather than after every answer.
  const input = SCENARIO === "compact" && !hasCompacted(body) && !isCompaction(body) ? 50000 : 10;
  out.push({ type: "response.completed", response: { id, object: "response", status: "completed", model, output: item ? [item] : [], usage: { input_tokens: input, input_tokens_details: { cached_tokens: 0 }, output_tokens: 5, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: input + 5 } } });
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
  if (refuseForFailover(remote(req), req.headers["chatgpt-account-id"])) {
    record({ kind: "ws-refused", from: remote(req), account: req.headers["chatgpt-account-id"], path: req.url });
    socket.end("HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}");
    return;
  }
  record({ kind: "ws-handshake", from: remote(req), conn: connectionOf(socket), method: req.method, path: req.url, headers: req.headers, rawHeaderOrder: req.rawHeaders.filter((_, i) => i % 2 === 0) });
  // 426 is the backend's "use HTTP": Codex falls back on it, and the pool
  // passes it on without holding it against the account. A 404 here once
  // cooled the pool's account and spoiled the phases after it.
  if (process.env.REJECT_WS === "1") { socket.end("HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\n\r\n"); return; }
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
        const { n, out } = events(body, req.headers);
        record({ kind: "ws-message", from: remote(req), conn: connectionOf(socket), path: req.url, n, body });
        for (const event of out) wsSend(socket, JSON.stringify(event));
      }
    }
  });
  socket.on("error", () => {});
}

const server = http2.createSecureServer({ allowHTTP1: true, key: fs.readFileSync("/certs/server.key"), cert: fs.readFileSync("/certs/server.pem") }, (req, res) => {
  // Between a scenario's native run and its pool run, so both see the same
  // ids (resp_fake_1, call_1, ...) and the same first refused account.
  if (req.url === "/__conformance/reset") {
    seq = 0;
    failedAccount = "";
    spawnCalls.clear();
    res.writeHead(204);
    res.end();
    return;
  }
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    let buffer = Buffer.concat(chunks);
    const encoding = req.headers["content-encoding"] || "";
    if (encoding === "zstd") buffer = zlib.zstdDecompressSync(buffer);
    const raw = buffer.toString("utf8");
    let body = null;
    try { body = raw ? JSON.parse(raw) : null; } catch { body = { raw: raw.slice(0, 2000) }; }
    if (req.method === "POST" && /\/responses$/.test(req.url.split("?")[0]) && refuseForFailover(remote(req), req.headers["chatgpt-account-id"])) {
      record({ kind: "http-refused", from: remote(req), account: req.headers["chatgpt-account-id"], path: req.url });
      res.writeHead(429, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { type: "usage_limit_reached", message: "The usage limit has been reached", plan_type: "pro" } }));
      return;
    }
    if (req.method === "POST" && /\/responses$/.test(req.url.split("?")[0])) {
      const { n, out } = events(body || {}, req.headers);
      record({ kind: "http", from: remote(req), conn: requestConnection(req), proto: req.httpVersion, method: req.method, path: req.url, n, headers: req.headers, rawHeaderOrder: req.rawHeaders.filter((_, i) => i % 2 === 0), encoding, wireBytes: Buffer.concat(chunks).length, body });
      res.writeHead(200, { "content-type": "text/event-stream", "x-codex-turn-state": `ts-${n}`, "openai-model": (body && body.model) || "fake", "x-reasoning-included": "true" });
      for (const event of out) res.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
      res.end();
      return;
    }
    record({ kind: "http-other", from: remote(req), method: req.method, path: req.url, headers: req.headers, rawHeaderOrder: req.rawHeaders.filter((_, i) => i % 2 === 0), rawBody: raw.length <= 4096 ? raw : null, bodyKeys: body && typeof body === "object" ? Object.keys(body) : null });
    if (/\/oauth\/token$/.test(req.url.split("?")[0])) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ id_token: refreshedToken, access_token: refreshedToken, refresh_token: "<capture-placeholder>" })); return; }
    // PRIME_WINDOW=1 reports a five-hour window that has just reset and not
    // started counting, which the pool primes.
    if (process.env.PRIME_WINDOW === "1" && /\/wham\/usage$/.test(req.url.split("?")[0])) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ plan_type: "pro", rate_limit: { allowed: true, limit_reached: false, primary_window: { used_percent: 0, limit_window_seconds: 18000, reset_after_seconds: 18000, reset_at: Math.floor(Date.now() / 1000) + 18000 } } })); return; }
    // RESET_CREDITS=1 lists the reset credit that usage below reports and
    // answers its redemption, for the reset phase: the listing and the consume
    // answer are Codex's own shapes (backend-client client/rate_limit_resets_tests.rs and app-server
    // tests/suite/v2/rate_limit_reset_credits.rs, verbatim).
    if (process.env.RESET_CREDITS === "1") {
      const route = req.url.split("?")[0];
      if (/\/wham\/rate-limit-reset-credits$/.test(route)) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ credits: [{ id: "credit-capture", reset_type: "codex_rate_limits", status: "available", granted_at: "2026-06-17T00:00:00Z", expires_at: "2026-07-17T00:00:00Z" }], available_count: 1, total_earned_count: 1 })); return; }
      if (/\/wham\/rate-limit-reset-credits\/consume$/.test(route)) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ code: "reset", windows_reset: 2 })); return; }
    }
    // Otherwise anchored windows (used and with a reset time), so the pool has
    // no window to prime while the capture runs.
    if (/\/wham\/usage$/.test(req.url.split("?")[0])) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ plan_type: "pro", rate_limit: { allowed: true, limit_reached: false, primary_window: { used_percent: 10, limit_window_seconds: 18000, reset_after_seconds: 3600, reset_at: Math.floor(Date.now() / 1000) + 3600 }, secondary_window: { used_percent: 20, limit_window_seconds: 604800, reset_after_seconds: 86400, reset_at: Math.floor(Date.now() / 1000) + 86400 } }, rate_limit_reset_credits: { available_count: 1 } })); return; }
    if (/\/wham\/rate-limit-reset-credits$/.test(req.url.split("?")[0])) { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ credits: [] })); return; }
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
