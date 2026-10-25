// An in-house app calling the pool's /v1 the way an OpenAI SDK does: its own
// user agent and headers, the public API's fields, string content, a system
// prompt in `instructions`. The pool must complete its turn into the request a
// Codex custom-provider client sends, with nothing of the app left in it, and
// must refuse, before upstream, the same turn with a parameter no Codex sends
// (upstream/src/pool/codex_request.rs, SPEC.md 2.3.4).
//   node app-client.mjs <model>
const base = "http://conformance-pool:8317/v1";
const model = process.argv[2];
// compare-wire.mjs fails if this string reaches upstream anywhere.
const marker = "conformance-app";
const headers = {
  "content-type": "application/json",
  authorization: `Bearer ${process.env.CONFORMANCE_CLIENT_KEY}`,
  "user-agent": `${marker}/1.0`,
  "x-stainless-lang": "js",
  "x-openai-app": marker,
  "openai-beta": marker,
};
const turn = {
  model,
  instructions: "Answer briefly.",
  input: [{ role: "user", content: "Say hi." }],
  tools: [{ type: "function", name: "lookup", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] } }],
  reasoning: { effort: "xhigh" },
  stream: true,
  user: marker,
  metadata: { app: marker },
  prompt_cache_key: marker,
};
const send = (body) => fetch(`${base}/responses`, { method: "POST", headers, body: JSON.stringify(body) });
const answered = await send(turn);
await answered.text();
console.log(`app turn: ${answered.status}`);
const refused = await send({ ...turn, temperature: 0.2 });
console.log(`app refused: ${refused.status} ${(await refused.text()).slice(0, 300)}`);
