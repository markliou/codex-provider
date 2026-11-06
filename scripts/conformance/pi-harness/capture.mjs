// Fake upstream for measuring what a client sends. Records every request
// (raw header order, body) and answers in minimal Responses/OAuth shapes.
import https from "node:https";
import http from "node:http";
import fs from "node:fs";
const out = process.env.OUT || "/out/capture.jsonl";
function sse(res) {
  res.writeHead(200, { "content-type": "text/event-stream" });
  const id = "resp_capture";
  const msg = { id: "msg_capture", type: "message", role: "assistant", status: "completed",
    content: [{ type: "output_text", text: "hi", annotations: [] }] };
  const base = { id, object: "response", created_at: 1, model: "gpt-5.5", status: "in_progress", output: [] };
  const ev = (type, data) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
  ev("response.created", { sequence_number: 0, response: base });
  ev("response.output_item.added", { sequence_number: 1, output_index: 0, item: { ...msg, status: "in_progress", content: [] } });
  ev("response.content_part.added", { sequence_number: 2, item_id: msg.id, output_index: 0, content_index: 0, part: { type: "output_text", text: "", annotations: [] } });
  ev("response.output_text.delta", { sequence_number: 3, item_id: msg.id, output_index: 0, content_index: 0, delta: "hi" });
  ev("response.output_text.done", { sequence_number: 4, item_id: msg.id, output_index: 0, content_index: 0, text: "hi" });
  ev("response.content_part.done", { sequence_number: 5, item_id: msg.id, output_index: 0, content_index: 0, part: msg.content[0] });
  ev("response.output_item.done", { sequence_number: 6, output_index: 0, item: msg });
  ev("response.completed", { sequence_number: 7, response: { ...base, status: "completed", output: [msg],
    usage: { input_tokens: 10, input_tokens_details: { cached_tokens: 0 }, output_tokens: 1, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 11 } } });
  res.end();
}
function handler(req, res) {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    fs.appendFileSync(out, JSON.stringify({ host: req.headers.host, method: req.method, url: req.url,
      httpVersion: req.httpVersion, rawHeaders: req.rawHeaders, body: body.toString("utf8") }) + "\n");
    if (req.url.endsWith("/responses")) return sse(res);
    if (req.url.includes("/oauth/token")) {
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify({ access_token: "capture-access-2", refresh_token: "capture-refresh-2",
        id_token: "capture-id", token_type: "Bearer", expires_in: 3600,
        scope: "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct" }));
    }
    if (req.url.endsWith("/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify({ object: "list", data: [{ id: "gpt-5.5", object: "model", created: 1, owned_by: "openai" }] }));
    }
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { message: "not found" } }));
  });
}
https.createServer({ key: fs.readFileSync("/certs/server.key"), cert: fs.readFileSync("/certs/server.crt") }, handler).listen(443);
http.createServer(handler).listen(8080);
