// Compares what a native Codex sent the fake upstream with what the pool sent
// it for the same turn. Usage: node compare-wire.mjs <capture-dir>
//
// The directory holds {native,client,pool}-{http,ws}.jsonl (fake-upstream.mjs
// logs), tls-http.txt and tls-ws.txt (tshark ClientHello fields: source ip,
// server name, JA4, ALPN) and pool-ip.txt. "client" is the pool's Codex
// provider config sent straight to the fake: what Codex hands the pool.
//
// Two questions, kept apart because they have different owners:
//   1. Does the pool change anything? The pool's TLS ClientHello must be one a
//      native Codex sends, its HTTP/2 SETTINGS, request order, protocol,
//      header names, order and values and body encoding must equal the native
//      Codex's, and the body it forwards must equal the body Codex gave it.
//      Any difference here fails the run.
//   2. Does Codex send the pool what it sends its own backend? Codex sends a
//      custom provider a reduced body (SPEC.md 2.3.2). That is reported as the
//      client gap, with every differing field, but it is Codex's doing and the
//      pool cannot repair it by forwarding.
//   3. The native endpoint (SPEC.md 2.3.3) exists to close that gap: Codex's
//      built-in provider aimed at the pool. Through it the pool's upstream
//      traffic must equal a native Codex's outright, body included; any
//      difference fails the run.
// Values minted per run (UUIDs, the fake's own response ids and turn states,
// timestamps) are masked before comparing, and nothing else is: a difference
// that has to be masked to pass is a difference.
import fs from "node:fs";
import path from "node:path";

const dir = process.argv[2];
const read = (name) => fs.readFileSync(path.join(dir, name), "utf8");
const load = (name) => read(name).split("\n").filter(Boolean).map((line) => JSON.parse(line));

// content-length follows the compressed body, whose bytes differ wherever a
// masked id differs; sec-websocket-key is a per-handshake nonce.
const ignoredHeaders = new Set(["content-length", "sec-websocket-key"]);
const mask = (value) => String(value)
  .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, "<uuid>")
  .replace(/\b(resp_fake|fc|call|msg)_\d+\b/g, "$1_<n>")
  .replace(/\bts-\d+\b/g, "ts-<n>")
  .replace(/\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z\b/g, "<time>")
  // Epoch seconds or milliseconds, with the fraction Codex writes in
  // internal_chat_message_metadata_passthrough.create_time.
  .replace(/\b1\d{9}(\d{3})?(\.\d+)?\b/g, "<epoch>")
  // exec_command output carries a random chunk id per call.
  .replace(/Chunk ID: [0-9a-f]+/g, "Chunk ID: <chunk>");

// Codex builds client_metadata from a HashMap, so its key order changes from
// one Codex run to the next even with nothing in between; the two bodies being
// compared always come from two runs. Only that object is compared without
// regard to order. The body's own key order is still compared, so a pool that
// re-encoded the body would still fail.
const canonical = (body) => {
  if (!body || typeof body !== "object" || !body.client_metadata || typeof body.client_metadata !== "object") return body;
  const sorted = Object.fromEntries(Object.entries(body.client_metadata).sort(([a], [b]) => a.localeCompare(b)));
  return { ...body, client_metadata: sorted };
};

const problems = [];
const notes = [];
const gaps = [];
const fail = (message) => problems.push(message);

function compareHeaders(label, native, pool) {
  const nativeOrder = (native.rawHeaderOrder || []).map((name) => name.toLowerCase());
  const poolOrder = (pool.rawHeaderOrder || []).map((name) => name.toLowerCase());
  if (nativeOrder.join(" ") !== poolOrder.join(" ")) {
    fail(`${label}: header order differs\n    native: ${nativeOrder.join(" ")}\n    pool:   ${poolOrder.join(" ")}`);
  }
  const names = new Set([...Object.keys(native.headers || {}), ...Object.keys(pool.headers || {})]);
  for (const name of names) {
    if (ignoredHeaders.has(name)) continue;
    const left = native.headers[name];
    const right = pool.headers[name];
    if (left === undefined || right === undefined || mask(left) !== mask(right)) {
      const show = (value) => value === undefined ? "(absent)" : name === "authorization" ? "<credential>" : mask(value).slice(0, 160);
      fail(`${label}: header ${name} differs: native=${show(left)} pool=${show(right)}`);
    }
  }
}

function bodyDifference(left, right) {
  left = canonical(left);
  right = canonical(right);
  if (mask(JSON.stringify(left)) === mask(JSON.stringify(right))) return null;
  const keys = new Set([...Object.keys(left || {}), ...Object.keys(right || {})]);
  const differing = [...keys].filter((key) => mask(JSON.stringify(left?.[key])) !== mask(JSON.stringify(right?.[key])));
  const order = JSON.stringify(Object.keys(left || {})) === JSON.stringify(Object.keys(right || {})) ? "" : " (key order differs)";
  return `${order} in ${differing.join(", ") || "key order only"}`;
}

// The pool must forward exactly what Codex gave it.
function compareForwarded(label, client, pool) {
  const difference = bodyDifference(client, pool);
  if (difference) fail(`${label}: the pool changed the body${difference}`);
}

// What Codex sends a custom provider and not its own backend, field by field.
function describeGap(label, native, client) {
  const difference = bodyDifference(native, client);
  if (!difference) return;
  const lines = [`${label}: body differs${difference}`];
  const names = (body) => (body?.tools || []).map((tool) => tool.name || tool.type);
  const missingTools = names(native).filter((name) => !names(client).includes(name));
  const extraTools = names(client).filter((name) => !names(native).includes(name));
  if (missingTools.length || extraTools.length) lines.push(`    tools only native: ${missingTools.join(", ") || "-"}; only custom provider: ${extraTools.join(", ") || "-"}`);
  const metadataKeys = (body) => Object.keys(body?.client_metadata || {});
  const missingMetadata = metadataKeys(native).filter((key) => !metadataKeys(client).includes(key));
  if (missingMetadata.length) lines.push(`    client_metadata only native: ${missingMetadata.join(", ")}`);
  const itemKeys = (body) => new Set((body?.input || []).flatMap((item) => Object.keys(item || {})));
  const missingItemKeys = [...itemKeys(native)].filter((key) => !itemKeys(client).has(key));
  if (missingItemKeys.length) lines.push(`    input item fields only native: ${missingItemKeys.join(", ")}`);
  gaps.push(lines.join("\n"));
}

function compareHTTP() {
  const native = load("native-http.jsonl");
  const pool = load("pool-http.jsonl");
  const nativeRequests = native.filter((entry) => entry.kind === "http");
  const clientRequests = load("client-http.jsonl").filter((entry) => entry.kind === "http");
  const poolRequests = pool.filter((entry) => entry.kind === "http");
  if (clientRequests.length !== poolRequests.length) fail(`http: Codex sent the pool's provider ${clientRequests.length} requests direct, the pool forwarded ${poolRequests.length}`);
  if (!nativeRequests.length) fail("http: native Codex sent no Responses request; the capture did not run");
  if (nativeRequests.length !== poolRequests.length) {
    fail(`http: native sent ${nativeRequests.length} Responses requests, the pool ${poolRequests.length}`);
  }
  for (let i = 0; i < Math.min(nativeRequests.length, poolRequests.length); i++) {
    const left = nativeRequests[i];
    const right = poolRequests[i];
    const label = `http request ${i + 1}`;
    if (left.proto !== right.proto) fail(`${label}: protocol native=${left.proto} pool=${right.proto}`);
    if (left.path !== right.path) fail(`${label}: path native=${left.path} pool=${right.path}`);
    if (left.encoding !== right.encoding) fail(`${label}: content-encoding native=${left.encoding || "none"} pool=${right.encoding || "none"}`);
    compareHeaders(label, left, right);
    if (clientRequests[i]) {
      compareForwarded(label, clientRequests[i].body, right.body);
      describeGap(label, left.body, clientRequests[i].body);
    }
  }
  const poolIP = read("pool-ip.txt").trim();
  const settings = (entries, fromPool) => entries
    .filter((entry) => entry.kind === "h2-settings" && String(entry.from).endsWith(poolIP) === fromPool)
    .map((entry) => JSON.stringify(entry.settings));
  const nativeSettings = [...new Set(settings(native, false))];
  const poolSettings = [...new Set(settings(pool, true))];
  if (!nativeSettings.length || !poolSettings.length) fail("http: no HTTP/2 SETTINGS were recorded");
  else if (nativeSettings.join() !== poolSettings.join()) fail(`http: HTTP/2 SETTINGS differ: native=${nativeSettings.join(" ")} pool=${poolSettings.join(" ")}`);
  notes.push(`http: ${nativeRequests.length} requests compared`);
}

function compareWS() {
  const native = load("native-ws.jsonl");
  const pool = load("pool-ws.jsonl");
  for (const [label, entries] of [["native", native], ["pool", pool]]) {
    if (entries.some((entry) => entry.kind === "http")) fail(`ws: ${label} fell back to HTTP`);
  }
  const nativeHandshakes = native.filter((entry) => entry.kind === "ws-handshake");
  const poolHandshakes = pool.filter((entry) => entry.kind === "ws-handshake");
  if (!nativeHandshakes.length) fail("ws: native Codex opened no WebSocket; the capture did not run");
  if (nativeHandshakes.length !== poolHandshakes.length) fail(`ws: native opened ${nativeHandshakes.length} sockets, the pool ${poolHandshakes.length}`);
  for (let i = 0; i < Math.min(nativeHandshakes.length, poolHandshakes.length); i++) {
    compareHeaders(`ws handshake ${i + 1}`, nativeHandshakes[i], poolHandshakes[i]);
  }
  const nativeMessages = native.filter((entry) => entry.kind === "ws-message");
  const clientMessages = load("client-ws.jsonl").filter((entry) => entry.kind === "ws-message");
  const poolMessages = pool.filter((entry) => entry.kind === "ws-message");
  if (nativeMessages.length !== poolMessages.length) fail(`ws: native sent ${nativeMessages.length} messages, the pool ${poolMessages.length}`);
  if (clientMessages.length !== poolMessages.length) fail(`ws: Codex sent the pool's provider ${clientMessages.length} messages direct, the pool forwarded ${poolMessages.length}`);
  for (let i = 0; i < poolMessages.length; i++) {
    if (clientMessages[i]) compareForwarded(`ws message ${i + 1}`, clientMessages[i].body, poolMessages[i].body);
    if (clientMessages[i] && nativeMessages[i]) describeGap(`ws message ${i + 1}`, nativeMessages[i].body, clientMessages[i].body);
  }
  notes.push(`ws: ${nativeHandshakes.length} handshakes and ${nativeMessages.length} messages compared`);
}

// A native Codex opens more connections than the pool does (the model list,
// analytics on ab.chatgpt.com, its WebSocket attempts), some from another TLS
// stack. Every ClientHello the pool sends must be one Codex sends to the same
// server; the pool need not send every kind Codex does.
function compareTLS(name) {
  const poolIP = read("pool-ip.txt").trim();
  const lines = read(name).split("\n").filter(Boolean).map((line) => line.split("\t"));
  const hello = (fromPool) => [...new Set(lines.filter(([ip]) => (ip === poolIP) === fromPool).map(([, server, ja4, alpn]) => `${server} ${ja4} alpn=${alpn || "-"}`))];
  const codex = hello(false);
  const pool = hello(true);
  if (!codex.length || !pool.length) { fail(`${name}: no ClientHello captured (codex ${codex.length}, pool ${pool.length})`); return; }
  const foreign = pool.filter((entry) => !codex.includes(entry));
  if (foreign.length) fail(`${name}: the pool sent a ClientHello Codex never sends: ${foreign.join(" | ")} (codex: ${codex.join(" | ")})`);
  else notes.push(`${name}: ${pool.join(" | ")}`);
}

// Native endpoint: what reached upstream through the pool must be what a
// native Codex sent, header for header and body for body.
function compareNativeEndpoint(mode) {
  const kind = mode === "http" ? "http" : "ws-message";
  const native = load(`native-${mode}.jsonl`);
  const pool = load(`native-pool-${mode}.jsonl`);
  if (mode === "ws" && pool.some((entry) => entry.kind === "http")) fail("native endpoint ws: the pool fell back to HTTP");
  const left = native.filter((entry) => entry.kind === kind);
  const right = pool.filter((entry) => entry.kind === kind);
  if (!right.length) { fail(`native endpoint ${mode}: nothing reached upstream through the pool`); return; }
  if (left.length !== right.length) fail(`native endpoint ${mode}: native sent ${left.length}, the pool ${right.length}`);
  for (let i = 0; i < Math.min(left.length, right.length); i++) {
    const label = `native endpoint ${mode} ${kind === "http" ? "request" : "message"} ${i + 1}`;
    if (kind === "http") {
      if (left[i].proto !== right[i].proto) fail(`${label}: protocol native=${left[i].proto} pool=${right[i].proto}`);
      if (left[i].path !== right[i].path) fail(`${label}: path native=${left[i].path} pool=${right[i].path}`);
      if (left[i].encoding !== right[i].encoding) fail(`${label}: content-encoding native=${left[i].encoding || "none"} pool=${right[i].encoding || "none"}`);
      compareHeaders(label, left[i], right[i]);
    }
    const difference = bodyDifference(left[i].body, right[i].body);
    if (difference) fail(`${label}: body differs from a native Codex${difference}`);
  }
  if (mode === "ws") {
    const handshakes = [native, pool].map((entries) => entries.filter((entry) => entry.kind === "ws-handshake"));
    if (handshakes[0].length !== handshakes[1].length) fail(`native endpoint ws: native opened ${handshakes[0].length} sockets, the pool ${handshakes[1].length}`);
    for (let i = 0; i < Math.min(handshakes[0].length, handshakes[1].length); i++) compareHeaders(`native endpoint ws handshake ${i + 1}`, handshakes[0][i], handshakes[1][i]);
  }
  notes.push(`native endpoint ${mode}: ${right.length} ${kind === "http" ? "requests" : "messages"} equal to a native Codex`);
}

// Codex builds a fresh client for every request (core/src/client.rs
// build_api_transport), so each Responses request and each socket arrives on
// its own connection. Whatever pattern the native Codex shows, the pool's
// must show too.
function compareConnections(nativeFile, poolFile) {
  const pattern = (file) => {
    const requests = load(file).filter((entry) => (entry.kind === "http" || entry.kind === "ws-handshake") && entry.conn != null);
    const distinct = new Set(requests.map((entry) => entry.conn)).size;
    return { requests: requests.length, distinct };
  };
  const native = pattern(nativeFile);
  const pool = pattern(poolFile);
  const shape = (p) => (p.requests && p.distinct === p.requests ? "one connection per request" : `${p.distinct} connections for ${p.requests} requests`);
  if (shape(native) !== shape(pool)) fail(`${poolFile}: ${shape(pool)}, but a native Codex opens ${shape(native)}`);
  else notes.push(`${poolFile}: ${shape(pool)}, as a native Codex`);
}

compareHTTP();
compareWS();
compareNativeEndpoint("http");
compareNativeEndpoint("ws");
compareConnections("native-http.jsonl", "pool-http.jsonl");
compareConnections("native-ws.jsonl", "pool-ws.jsonl");
compareConnections("native-http.jsonl", "native-pool-http.jsonl");
compareConnections("native-ws.jsonl", "native-pool-ws.jsonl");
compareTLS("tls-http.txt");
compareTLS("tls-ws.txt");
for (const note of notes) console.log(`ok   ${note}`);
for (const problem of problems) console.log(`FAIL ${problem}`);
if (gaps.length) {
  console.log("\nClient gap: Codex sends a custom provider (the pool) a different body than it sends its own backend.");
  console.log("The pool forwards what it is given; closing this is a client configuration question (SPEC.md 2.3.2).");
  for (const gap of gaps) console.log(`GAP  ${gap}`);
}
if (problems.length) {
  console.log(`\n${problems.length} difference(s) introduced by the pool.`);
  process.exit(1);
}
console.log("\nThe pool changes nothing: transport and headers match a native Codex, and bodies are forwarded as Codex sent them.");
