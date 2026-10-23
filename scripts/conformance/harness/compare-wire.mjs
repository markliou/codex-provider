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
//   4. The pool's own calls (token refresh, usage, account check, reset
//      credits) must be the requests a native Codex makes for the same thing:
//      the same header names in the same order, the same body, a Codex
//      User-Agent, over a ClientHello Codex sends.
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

function compareHeaders(label, native, pool, alsoIgnored = new Set()) {
  const nativeOrder = (native.rawHeaderOrder || []).map((name) => name.toLowerCase());
  const poolOrder = (pool.rawHeaderOrder || []).map((name) => name.toLowerCase());
  if (nativeOrder.join(" ") !== poolOrder.join(" ")) {
    fail(`${label}: header order differs\n    native: ${nativeOrder.join(" ")}\n    pool:   ${poolOrder.join(" ")}`);
  }
  const names = new Set([...Object.keys(native.headers || {}), ...Object.keys(pool.headers || {})]);
  for (const name of names) {
    if (ignoredHeaders.has(name) || alsoIgnored.has(name)) continue;
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

// The pool must forward exactly what Codex gave it. The one value it changes,
// the installation id, is a UUID and so masked here; compareInstallationIDs
// checks it.
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
// A phase with no native Codex in it (the prime) is held to the ClientHellos
// native Codex sent in the other phases.
function compareTLS(name, referenceNames = [name]) {
  const poolIP = read("pool-ip.txt").trim();
  const parse = (file) => read(file).split("\n").filter(Boolean).map((line) => line.split("\t"));
  const describe = (lines) => [...new Set(lines.map(([, server, ja4, alpn]) => `${server} ${ja4} alpn=${alpn || "-"}`))];
  const codex = describe(referenceNames.flatMap(parse).filter(([ip]) => ip !== poolIP));
  const pool = describe(parse(name).filter(([ip]) => ip === poolIP));
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

// The pool's own calls, from own.jsonl: the pool started against the fake
// (refreshing its expiring credential, then reading usage, account check and
// reset credits), then a native Codex with an expiring credential refreshed
// it and ran a turn. Values that differ by design are the credential (the pool uses
// the token it just refreshed) and the agent before any Codex has used the
// pool (SPEC.md 2.3.2): only its shape is checked.
function compareOwn() {
  const poolIP = read("pool-ip.txt").trim();
  const entries = load("own.jsonl").filter((entry) => entry.kind === "http-other");
  const route = (entry) => `${entry.method} ${String(entry.path).split("?")[0]}`;
  const order = (entry) => (entry.rawHeaderOrder || []).map((name) => name.toLowerCase()).filter((name) => !name.startsWith(":") && name !== "host" && name !== "content-length").join(" ");
  const codexAgent = (value) => /^codex[a-z_-]*\/\d+\.\d+\.\d+ \(.+\)/i.test(String(value || ""));
  const native = entries.filter((entry) => entry.from !== poolIP);
  const pool = entries.filter((entry) => entry.from === poolIP);
  if (!pool.length) { fail("own calls: the pool made no calls of its own during the capture"); return; }
  const nativeRefresh = native.find((entry) => route(entry) === "POST /oauth/token");
  const poolRefresh = pool.find((entry) => route(entry) === "POST /oauth/token");
  if (!nativeRefresh) fail("own calls: the native Codex did not refresh its expiring credential");
  if (!poolRefresh) fail("own calls: the pool did not refresh its expiring credential");
  if (nativeRefresh && poolRefresh) {
    if (order(nativeRefresh) !== order(poolRefresh)) fail(`own calls: token refresh header order differs\n    native: ${order(nativeRefresh)}\n    pool:   ${order(poolRefresh)}`);
    for (const name of ["content-type", "accept"]) {
      if (nativeRefresh.headers[name] !== poolRefresh.headers[name]) fail(`own calls: token refresh ${name} native=${nativeRefresh.headers[name]} pool=${poolRefresh.headers[name]}`);
    }
    if (!codexAgent(poolRefresh.headers["user-agent"]) || !poolRefresh.headers.originator) fail(`own calls: token refresh does not present a Codex agent: ${poolRefresh.headers["user-agent"]}`);
    // Before any Codex has used the pool its agent is the pool's own, in the
    // bundled CLI's shape; it must still have no conversation suffix.
    if (/\) [^()]+ \([^()]*\)$/.test(String(poolRefresh.headers["user-agent"]))) fail(`own calls: token refresh carries a conversation User-Agent suffix: ${poolRefresh.headers["user-agent"]}`);
    if (mask(nativeRefresh.rawBody) !== mask(poolRefresh.rawBody)) fail("own calls: token refresh body differs from a native Codex's");
    else notes.push("own calls: token refresh equal to a native Codex's");
  }
  const reference = native.find((entry) => route(entry) === "GET /backend-api/wham/accounts/check");
  if (!reference) { fail("own calls: no native account check to compare the pool's backend calls with"); return; }
  for (const path of ["GET /backend-api/wham/usage", "GET /backend-api/wham/accounts/check", "GET /backend-api/wham/rate-limit-reset-credits"]) {
    const call = pool.find((entry) => route(entry) === path);
    if (!call) { fail(`own calls: the pool made no ${path}`); continue; }
    if (order(call) !== order(reference)) fail(`own calls: ${path} header order differs from Codex's backend client\n    codex: ${order(reference)}\n    pool:  ${order(call)}`);
    if (!codexAgent(call.headers["user-agent"])) fail(`own calls: ${path} does not present a Codex agent: ${call.headers["user-agent"]}`);
    if (call.headers["chatgpt-account-id"] !== reference.headers["chatgpt-account-id"]) fail(`own calls: ${path} names another account`);
    for (const browser of ["origin", "referer", "sec-fetch-mode", "x-openai-target-path", "oai-language"]) {
      if (call.headers[browser]) fail(`own calls: ${path} carries the browser header ${browser}`);
    }
  }
  const unexpected = pool.filter((entry) => !["POST /oauth/token", "GET /backend-api/wham/usage", "GET /backend-api/wham/accounts/check", "GET /backend-api/wham/rate-limit-reset-credits"].includes(route(entry)));
  for (const entry of unexpected) fail(`own calls: the pool called ${route(entry)}, which this check does not know`);
  notes.push(`own calls: ${pool.length} pool calls checked against Codex's`);
}

// A prime is one turn of the bundled Codex CLI in the pool's container, so its
// Responses request must be a native Codex's: the pinned Codex's agent and the
// header set and order a native Codex's socket or request carries.
function comparePrime() {
  const poolIP = read("pool-ip.txt").trim();
  const entries = load("prime.jsonl").filter((entry) => entry.from === poolIP);
  const turn = entries.find((entry) => entry.kind === "ws-handshake" || entry.kind === "http");
  if (!turn) { fail(`prime: the pool's Codex sent no Responses request (pool log: ${read("prime.log").trim() || "nothing"})`); return; }
  const agent = String(turn.headers["user-agent"] || "");
  if (!/^codex_exec\/\d+\.\d+\.\d+ \(/.test(agent)) fail(`prime: the request does not carry the bundled Codex's agent: ${agent}`);
  const native = load(turn.kind === "ws-handshake" ? "native-ws.jsonl" : "native-http.jsonl").find((entry) => entry.kind === turn.kind);
  if (native) {
    const names = (entry) => (entry.rawHeaderOrder || []).map((name) => name.toLowerCase()).join(" ");
    if (names(native) !== names(turn)) fail(`prime: header order differs from a native Codex's\n    native: ${names(native)}\n    prime:  ${names(turn)}`);
  }
  if (entries.some((entry) => entry.kind === "http-other" && String(entry.path).split("?")[0] === "/oauth/token")) fail("prime: the prime's Codex tried to refresh the account's credential");
  notes.push(`prime: ${turn.kind} from the bundled Codex (${agent.split(" ")[0]})`);
}

// Scenario phases (subagent, compaction, failover): the requests that reached
// upstream through the pool's native endpoint must be a native Codex's, role
// by role (parent and child threads interleave differently from run to run,
// so each role is compared in its own order).
function compareScenario(label, { requireChild = false, requireCompaction = false, failover = false } = {}) {
  const native = load(`native-${label}.jsonl`);
  const pool = load(`native-pool-${label}.jsonl`);
  const childHeaders = (headers) => Boolean(headers && (headers["x-openai-subagent"] || headers["x-codex-parent-thread-id"]));
  const roleOf = (entries) => {
    const sockets = new Map(entries.filter((entry) => entry.kind === "ws-handshake").map((entry) => [entry.conn, childHeaders(entry.headers) ? "child" : "parent"]));
    return (entry) => entry.kind === "ws-message" ? (sockets.get(entry.conn) || "parent") : (childHeaders(entry.headers) ? "child" : "parent");
  };
  const requests = (entries) => entries.filter((entry) => ["http", "ws-handshake", "ws-message"].includes(entry.kind));
  const nativeRequests = requests(native);
  const poolRequests = requests(pool);
  const nativeRole = roleOf(native);
  const poolRole = roleOf(pool);
  if (!nativeRequests.length) { fail(`${label}: the native Codex sent nothing; the scenario did not run`); return; }
  if (requireChild && !nativeRequests.some((entry) => nativeRole(entry) === "child")) fail(`${label}: the native Codex spawned no subagent; the scenario did not run`);
  if (requireCompaction && !nativeRequests.some((entry) => entry.kind !== "ws-handshake" && /compaction/.test(JSON.stringify((entry.body || {}).input || []) + JSON.stringify((entry.body || {}).client_metadata || {})))) fail(`${label}: the native Codex sent no compaction request; the scenario did not run`);
  const ignored = new Set();
  if (failover) {
    const refused = pool.filter((entry) => entry.kind === "http-refused" || entry.kind === "ws-refused");
    if (!refused.length) { fail(`${label}: no request was refused; the failover did not run`); return; }
    const refusedAccount = refused[0].account;
    if (!poolRequests.length || poolRequests.some((entry) => entry.kind !== "ws-message" && entry.headers && entry.headers["chatgpt-account-id"] === refusedAccount)) fail(`${label}: a request still reached the refused account after the failover`);
    // A different account by design: its credential and account id differ.
    ignored.add("authorization");
    ignored.add("chatgpt-account-id");
  }
  for (const role of ["parent", "child"]) {
    for (const kind of ["http", "ws-handshake", "ws-message"]) {
      const left = nativeRequests.filter((entry) => entry.kind === kind && nativeRole(entry) === role);
      const right = poolRequests.filter((entry) => entry.kind === kind && poolRole(entry) === role);
      if (left.length !== right.length) fail(`${label}: native ${role} sent ${left.length} ${kind}, the pool ${right.length}`);
      for (let i = 0; i < Math.min(left.length, right.length); i++) {
        const name = `${label} ${role} ${kind} ${i + 1}`;
        if (kind !== "ws-message") compareHeaders(name, left[i], right[i], ignored);
        if (kind !== "ws-handshake") {
          const difference = bodyDifference(left[i].body, right[i].body);
          if (difference) fail(`${name}: body differs from a native Codex${difference}`);
        }
      }
    }
  }
  const children = nativeRequests.filter((entry) => nativeRole(entry) === "child").length;
  notes.push(`${label}: ${poolRequests.length} requests equal to a native Codex's${children ? ` (${children} from the subagent)` : ""}`);
}

// Each pool account presents its own installation id in place of the
// client's (upstream/src/pool/installation_id.rs, SPEC.md 2.3.2): every id the pool sent is one
// of its accounts', and no id a Codex sent on its own is.
function compareInstallationIDs() {
  const poolIP = read("pool-ip.txt").trim();
  const accounts = new Set(read("account-installation-ids.txt").split("\n").map((line) => line.trim()).filter(Boolean));
  if (!accounts.size) { fail("installation id: the pool's accounts have none"); return; }
  const fromTurnMetadata = (value) => { try { return JSON.parse(value).installation_id; } catch { return undefined; } };
  const idsIn = (entry) => {
    const meta = entry.body && typeof entry.body === "object" ? entry.body.client_metadata : null;
    return [
      meta?.["x-codex-installation-id"],
      meta && fromTurnMetadata(meta["x-codex-turn-metadata"]),
      entry.headers && fromTurnMetadata(entry.headers["x-codex-turn-metadata"]),
    ].filter(Boolean);
  };
  let sent = 0;
  const used = new Set();
  for (const name of fs.readdirSync(dir).filter((file) => file.endsWith(".jsonl"))) {
    for (const entry of load(name)) {
      for (const id of idsIn(entry)) {
        if (entry.from === poolIP) {
          sent++;
          used.add(id);
          if (!accounts.has(id)) fail(`installation id: ${name}: the pool sent ${id}, which is no account's`);
        } else if (accounts.has(id)) {
          fail(`installation id: ${name}: a Codex of its own sent an account's id ${id}`);
        }
      }
    }
  }
  if (!sent) fail("installation id: the pool sent none");
  else notes.push(`installation id: ${sent} sent by the pool, all its accounts' own (${used.size} of ${accounts.size} accounts)`);
}

// A reset credit is spent as Codex's /usage reset spends it (upstream/src/pool/reset_credit.rs):
// the pool's redemption from the Console against a native Codex app-server's
// (account/rateLimitResetCredit/consume, which the TUI picker calls): the same
// header set and order, the body Codex's keys in Codex's order, the listed
// credit named, a UUID redeem request id, and one request, not a retry.
function compareReset() {
  const poolIP = read("pool-ip.txt").trim();
  const consume = (entry) => entry.kind === "http-other" && /\/rate-limit-reset-credits\/consume$/.test(String(entry.path).split("?")[0]);
  const entries = load("reset.jsonl").filter(consume);
  const native = entries.find((entry) => entry.from !== poolIP);
  const pool = entries.filter((entry) => entry.from === poolIP);
  const tail = (name) => { try { return read(name).trim().slice(-300); } catch { return ""; } };
  if (!native) { fail(`reset: the native Codex spent no credit (${tail("native-reset.codex.log")})`); return; }
  if (!pool.length) { fail(`reset: the pool spent no credit (${tail("reset-pool.log")})`); return; }
  if (pool.length !== 1) fail(`reset: the pool sent ${pool.length} redemptions for one press`);
  const order = (entry) => (entry.rawHeaderOrder || []).map((name) => name.toLowerCase()).filter((name) => !name.startsWith(":") && name !== "host" && name !== "content-length").join(" ");
  if (order(native) !== order(pool[0])) fail(`reset: header order differs\n    native: ${order(native)}\n    pool:   ${order(pool[0])}`);
  for (const name of ["content-type", "accept", "chatgpt-account-id"]) {
    if (native.headers[name] !== pool[0].headers[name]) fail(`reset: ${name} native=${native.headers[name]} pool=${pool[0].headers[name]}`);
  }
  if (!/^codex[a-z_-]*\/\d+\.\d+\.\d+ \(.+\)/i.test(String(pool[0].headers["user-agent"] || ""))) fail(`reset: the pool does not present a Codex agent: ${pool[0].headers["user-agent"]}`);
  const parse = (entry) => { try { return JSON.parse(entry.rawBody); } catch { return null; } };
  const nativeBody = parse(native);
  const poolBody = parse(pool[0]);
  if (!nativeBody || !poolBody) { fail("reset: a redemption body is not JSON"); return; }
  if (Object.keys(nativeBody).join(",") !== Object.keys(poolBody).join(",")) fail(`reset: body keys differ: native ${Object.keys(nativeBody)} pool ${Object.keys(poolBody)}`);
  if (poolBody.credit_id !== "credit-capture") fail(`reset: the pool named credit ${poolBody.credit_id}, not the one listed`);
  const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  if (!uuid.test(String(poolBody.redeem_request_id))) fail(`reset: the pool's redeem request id is not a v4 UUID as Codex's: ${poolBody.redeem_request_id}`);
  if (mask(native.rawBody) !== mask(pool[0].rawBody)) fail(`reset: body differs\n    native: ${mask(native.rawBody)}\n    pool:   ${mask(pool[0].rawBody)}`);
  else notes.push("reset: the pool's credit redemption equal to a native Codex's");
}

compareOwn();
compareTLS("tls-own.txt");
compareReset();
compareInstallationIDs();
comparePrime();
compareTLS("tls-prime.txt", ["tls-own.txt", "tls-http.txt", "tls-ws.txt"]);
compareHTTP();
compareWS();
compareNativeEndpoint("http");
compareNativeEndpoint("ws");
compareConnections("native-http.jsonl", "pool-http.jsonl");
compareConnections("native-ws.jsonl", "pool-ws.jsonl");
compareConnections("native-http.jsonl", "native-pool-http.jsonl");
compareConnections("native-ws.jsonl", "native-pool-ws.jsonl");
compareScenario("subagent-http", { requireChild: true });
compareScenario("subagent-ws", { requireChild: true });
compareScenario("compact", { requireCompaction: true });
compareScenario("failover-http", { failover: true });
compareScenario("failover-ws", { failover: true });
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
console.log("\nThe pool changes nothing: transport and headers match a native Codex, and bodies are forwarded as Codex sent them, apart from each account presenting its own installation id.");
