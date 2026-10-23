// Fake upstream for the integration tests (scripts/test/integration-*.sh): a
// Responses provider for API-key accounts on /v1/responses and the Codex
// backend for device accounts on /backend-api/codex/responses and
// /backend-api/wham/usage. GET /requests reports what it was sent so a test
// can assert on routing. A translation of the Go mock it replaces; answers are
// the shapes the Go one sent, field for field.
import http from "node:http";

const responseText = "CODEX_PROVIDER_CONFIG_TOML_OK";

const deviceFailoverText = "DEVICE_AUTH_FAILOVER_B";

const subagentParentText = "SUBAGENT_PARENT_OK";

const subagentChildText = "SUBAGENT_CHILD_OK";

const server = {
  last: null,
  events: [],
  spawnIssued: false,
  subagentIntegration: process.env.CODEX_MOCK_SUBAGENT_INTEGRATION === "true",
  routingCacheMode: process.env.CODEX_MOCK_ROUTING_CACHE_INTEGRATION === "true",
  routingPrimaryHits: 0,
};

const routes = {
  "GET /healthz": (_request, response) => writeJSON(response, 200, { ok: true }),
  "GET /requests": (_request, response) =>
    writeJSON(response, 200, { last: server.last, events: server.events }),
  "POST /v1/responses": handleResponse,
  // The Codex backend path a native Codex CLI signed in with ChatGPT uses.
  "POST /backend-api/codex/responses": handleDeviceAuthResponse,
  "GET /backend-api/wham/usage": handleDeviceAuthUsage,
};

http
  .createServer(async (request, response) => {
    const path = new URL(request.url, "http://mock").pathname;
    const route = routes[`${request.method} ${path}`];
    if (!route) {
      const known = Object.keys(routes).some((key) => key.endsWith(` ${path}`));
      response.writeHead(known ? 405 : 404, { "Content-Type": "text/plain; charset=utf-8" });
      response.end(known ? "Method Not Allowed\n" : "404 page not found\n");
      return;
    }
    try {
      await route(request, response);
    } catch (error) {
      console.error(error);
      if (!response.headersSent) response.writeHead(500);
      response.end();
    }
  })
  .listen(4010, () => console.log("mock upstream listening on :4010"));

async function handleResponse(request, response) {
  const body = parseObject(await readBody(request));
  if (body === undefined) {
    writeJSON(response, 400, { error: { message: "invalid JSON" } });
    return;
  }
  if (server.subagentIntegration) {
    await handleSubagentIntegrationResponse(request, response, body);
    return;
  }
  if (server.routingCacheMode) {
    handleRoutingCacheResponse(request, response, body);
    return;
  }
  if (header(request, "authorization") !== "Bearer upstream-test-key") {
    writeJSON(response, 401, { error: { message: "missing upstream credential" } });
    return;
  }
  server.last = body;
  server.events.push({ kind: "provider", status: 200 });
  const model = stringField(body, "model");
  const completed = completedResponse(model, responseText);
  if (body.stream !== true) {
    writeJSON(response, 200, completed);
    return;
  }
  writeStreamingResponse(response, model, completed, responseText);
}

function handleRoutingCacheResponse(request, response, body) {
  let account = "";
  switch (header(request, "authorization")) {
    case "Bearer routing-primary-key":
      account = "primary";
      break;
    case "Bearer routing-secondary-key":
      account = "secondary";
      break;
    default:
      writeJSON(response, 401, { error: { message: "missing routing-cache test credential" } });
      return;
  }

  let status = 200;
  if (account === "primary") {
    server.routingPrimaryHits++;
    if (server.routingPrimaryHits === 3) {
      status = 429;
    }
  }
  server.events.push({ kind: "routing-cache", account, status });
  server.last = body;
  const hit = server.routingPrimaryHits;

  if (status === 429) {
    response.writeHead(status, { "Retry-After": "1", "Content-Length": "0" });
    response.end();
    return;
  }

  let inputTokens = 2400;
  let cachedTokens = 0;
  let cacheWriteTokens = 1800;
  let responseID = "resp_routing_secondary";
  if (account === "primary") {
    inputTokens = 2000;
    cacheWriteTokens = 400;
    responseID = "resp_routing_primary_1";
    if (hit === 2) {
      cachedTokens = 1500;
      cacheWriteTokens = 100;
      responseID = "resp_routing_primary_2";
    }
  }
  const model = stringField(body, "model");
  const completed = completedResponseWithID(responseID, model, "ROUTING_CACHE_OK");
  completed.usage = {
    input_tokens: inputTokens,
    input_tokens_details: {
      cached_tokens: cachedTokens,
      cache_write_tokens: cacheWriteTokens,
    },
    output_tokens: 1,
    total_tokens: inputTokens + 1,
  };
  writeJSON(response, 200, completed);
}

async function handleSubagentIntegrationResponse(request, response, body) {
  let account = "";
  switch (header(request, "authorization")) {
    case "Bearer upstream-parent-key":
      account = "parent-account";
      break;
    case "Bearer upstream-preferred-key":
      account = "preferred-account";
      break;
    default:
      writeJSON(response, 401, { error: { message: "missing subagent test credential" } });
      return;
  }
  const metadata = requestCodexMetadata(body);
  const threadId = metadataString(metadata, "thread_id");
  const parentThreadId = metadataString(metadata, "parent_thread_id");
  const subagentKind = metadataString(metadata, "subagent_kind");
  const isSubagent = parentThreadId !== "" || subagentKind !== "";
  const promptCacheKey = stringField(body, "prompt_cache_key");
  const event = { kind: "subagent-integration", account, threadId, parentThreadId, subagentKind, promptCacheKey };

  const spawnIssued = server.spawnIssued;
  if (isSubagent) {
    event.role = "child";
    server.events.push(event);
    server.last = body;
    const model = stringField(body, "model");
    const completed = completedResponseWithID("resp_subagent_child", model, subagentChildText);
    writeStreamingResponse(response, model, completed, subagentChildText);
    return;
  }
  if (!spawnIssued && account === "preferred-account") {
    event.role = "parent";
    event.status = 429;
    server.events.push(event);
    server.last = body;
    response.writeHead(429, { "Retry-After": "1", "Content-Length": "0" });
    response.end();
    return;
  }
  if (!spawnIssued) {
    event.role = "parent";
    event.status = 200;
    const [namespace, hasForkTurns] = spawnAgentTool(body.tools);
    event.spawnSchemaHasForkTurns = hasForkTurns;
    server.events.push(event);
    server.last = body;
    server.spawnIssued = true;
    await sleep(1500);
    writeStreamingNamespacedFunctionCall(
      response,
      "resp_spawn",
      "call_spawn",
      namespace,
      "spawn_agent",
      `{"message":"Reply with the exact token SUBAGENT_CHILD_OK and nothing else.","task_name":"child_probe","fork_turns":"none"}`,
    );
    return;
  }
  event.role = "parent";
  event.status = 200;
  const [spawnOutput, sawSpawnOutput] = requestFunctionOutput(body, "call_spawn");
  event.sawSpawnOutput = sawSpawnOutput;
  event.spawnOutput = spawnOutput;
  server.events.push(event);
  server.last = body;
  await sleep(2000);
  const model = stringField(body, "model");
  const completed = completedResponseWithID("resp_subagent_parent", model, subagentParentText);
  writeStreamingResponse(response, model, completed, subagentParentText);
}

function requestCodexMetadata(body) {
  const clientMetadata = isObject(body.client_metadata) ? body.client_metadata : null;
  const raw = clientMetadata?.["x-codex-turn-metadata"];
  if (typeof raw === "string") {
    try {
      const metadata = JSON.parse(raw);
      if (metadata === null || isObject(metadata)) {
        return metadata;
      }
    } catch {
      // Not JSON: fall back to the client metadata itself.
    }
  }
  return clientMetadata;
}

function metadataString(metadata, key) {
  const value = metadata?.[key];
  return typeof value === "string" ? value.trim() : "";
}

// spawnAgentTool finds spawn_agent in the request's tools and reports the
// namespace it sits in and whether its schema offers fork_turns. Codex 0.163
// offers it inside the "collaboration" namespace tool; a call to it must name
// that namespace, or Codex rejects it as an unsupported call.
function spawnAgentTool(raw) {
  for (const tool of Array.isArray(raw) ? raw : []) {
    if (!isObject(tool)) {
      continue;
    }
    if (tool.type === "namespace") {
      const namespace = typeof tool.name === "string" ? tool.name : "";
      const found = findSpawnAgent(tool.tools);
      if (found) {
        return [namespace, found.hasForkTurns];
      }
      continue;
    }
    const found = findSpawnAgent([tool]);
    if (found) {
      return ["", found.hasForkTurns];
    }
  }
  return ["", false];
}

function findSpawnAgent(raw) {
  for (const tool of Array.isArray(raw) ? raw : []) {
    if (!isObject(tool) || tool.name !== "spawn_agent") {
      continue;
    }
    const properties = tool.parameters?.properties;
    return { tool, hasForkTurns: isObject(properties) && "fork_turns" in properties };
  }
  return null;
}

function requestFunctionOutput(body, callID) {
  for (const item of Array.isArray(body.input) ? body.input : []) {
    if (isObject(item) && item.type === "function_call_output" && item.call_id === callID) {
      return [typeof item.output === "string" ? item.output : "", true];
    }
  }
  return ["", false];
}

function writeStreamingNamespacedFunctionCall(response, responseID, callID, namespace, name, args) {
  startEventStream(response);
  writeEvent(response, "response.created", { type: "response.created", response: { id: responseID } });
  const item = { type: "function_call", call_id: callID, name, arguments: args };
  if (namespace !== "") {
    item.namespace = namespace;
  }
  writeEvent(response, "response.output_item.done", { type: "response.output_item.done", item });
  writeEvent(response, "response.completed", {
    type: "response.completed",
    response: {
      id: responseID,
      output: [item],
      usage: { input_tokens: 2048, input_tokens_details: { cached_tokens: 0 }, output_tokens: 64, total_tokens: 2112 },
    },
  });
  response.end();
}

function writeStreamingResponse(response, model, completed, text) {
  startEventStream(response);
  writeEvent(response, "response.created", { type: "response.created", response: inProgressResponse(completed.id, model) });
  const message = { id: "msg_mock", type: "message", status: "in_progress", role: "assistant", content: [] };
  writeEvent(response, "response.output_item.added", { type: "response.output_item.added", output_index: 0, item: message });
  writeEvent(response, "response.content_part.added", {
    type: "response.content_part.added",
    item_id: "msg_mock",
    output_index: 0,
    content_index: 0,
    part: { type: "output_text", text: "", annotations: [] },
  });
  writeEvent(response, "response.output_text.delta", {
    type: "response.output_text.delta",
    item_id: "msg_mock",
    output_index: 0,
    content_index: 0,
    delta: text,
  });
  writeEvent(response, "response.output_text.done", {
    type: "response.output_text.done",
    item_id: "msg_mock",
    output_index: 0,
    content_index: 0,
    text,
  });
  writeEvent(response, "response.content_part.done", {
    type: "response.content_part.done",
    item_id: "msg_mock",
    output_index: 0,
    content_index: 0,
    part: { type: "output_text", text, annotations: [] },
  });
  writeEvent(response, "response.output_item.done", { type: "response.output_item.done", output_index: 0, item: completed.output[0] });
  writeEvent(response, "response.completed", { type: "response.completed", response: completed });
  response.end("data: [DONE]\n\n");
}

async function handleDeviceAuthResponse(request, response) {
  const accountID = header(request, "chatgpt-account-id");
  const accessToken = header(request, "authorization");
  let status = 401;
  let text = "";
  switch (accountID) {
    case "test-acct-a":
      if (accessToken === "Bearer <test-device-access-a>") {
        status = 429;
      }
      break;
    case "test-acct-b":
      if (accessToken === "Bearer <test-device-access-b>") {
        status = 200;
        text = deviceFailoverText;
      }
      break;
  }
  server.events.push({ kind: "device", account: accountID, status });
  if (status === 429) {
    response.writeHead(status, { "Retry-After": "1", "Content-Length": "0" });
    response.end();
    return;
  }
  if (status !== 200) {
    writeJSON(response, status, { error: { message: "device auth rejected" } });
    return;
  }
  const body = await deviceAuthRequest(request);
  if (!body) {
    writeJSON(response, 400, { error: { message: "invalid JSON" } });
    return;
  }
  const model = stringField(body, "model");
  const completed = completedResponse(model, text);
  if (body.stream === true) {
    writeStreamingResponse(response, model, completed, text);
    return;
  }
  writeJSON(response, 200, completed);
}

// deviceAuthRequest reads the fields the mock needs. The pool sends the body
// zstd-compressed, as a native Codex does, so for a compressed body the model
// comes from the x-codex-routing-hint header and streaming from the Accept
// header, which a Codex request always sets; the mock needs no decoder.
async function deviceAuthRequest(request) {
  if (header(request, "content-encoding").toLowerCase() === "zstd") {
    const model = header(request, "x-codex-routing-hint").split(";", 1)[0].replace(/^model=/, "");
    return model !== "" ? { model, stream: header(request, "accept").includes("text/event-stream") } : null;
  }
  return parseObject(await readBody(request)) ?? null;
}

function handleDeviceAuthUsage(request, response) {
  const accountID = header(request, "chatgpt-account-id");
  if (accountID !== "test-acct-a" && accountID !== "test-acct-b") {
    writeJSON(response, 401, { error: { code: "invalid_account" } });
    return;
  }
  // The weekly window is already counting down. An untouched window (0% used,
  // reset a full window away) would make the pool spend a quota-priming
  // request on each account, and the failover test would count those as
  // client traffic.
  writeJSON(response, 200, {
    plan_type: "team",
    rate_limit: {
      allowed: true,
      primary_window: { used_percent: 0, limit_window_seconds: 18000, reset_after_seconds: 18000 },
      secondary_window: { used_percent: 10, limit_window_seconds: 604800, reset_after_seconds: 500000 },
    },
  });
}

function inProgressResponse(responseID, model) {
  return { id: responseID, object: "response", created_at: now(), status: "in_progress", model, output: [] };
}

function completedResponse(model, text) {
  return completedResponseWithID("resp_mock", model, text);
}

function completedResponseWithID(responseID, model, text) {
  return {
    id: responseID,
    object: "response",
    created_at: now(),
    status: "completed",
    model,
    output: [
      {
        id: "msg_mock",
        type: "message",
        status: "completed",
        role: "assistant",
        content: [{ type: "output_text", text, annotations: [] }],
      },
    ],
    usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
  };
}

function startEventStream(response) {
  response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
}

function writeEvent(response, event, value) {
  response.write(`event: ${event}\ndata: ${JSON.stringify(value)}\n\n`);
}

function writeJSON(response, status, value) {
  response.writeHead(status, { "Content-Type": "application/json" });
  response.end(`${JSON.stringify(value)}\n`);
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    request.on("error", reject);
  });
}

// The request body as a JSON object, or undefined when it is not one (the Go
// mock's decoder into a map refused anything else); a JSON null reads as an
// empty request, as it did there.
function parseObject(text) {
  try {
    const value = JSON.parse(text);
    if (value === null) {
      return {};
    }
    return isObject(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

function header(request, name) {
  const value = request.headers[name];
  return Array.isArray(value) ? value[0] : (value ?? "");
}

function stringField(object, key) {
  return typeof object?.[key] === "string" ? object[key] : "";
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function now() {
  return Math.floor(Date.now() / 1000);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
