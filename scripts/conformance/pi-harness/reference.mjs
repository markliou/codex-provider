// Turns a capture (capture.jsonl) into the Pi reference the pool is held to
// (upstream/testdata/pi-reference.json): the headers in Pi's order and the
// body, with the credential and session values replaced by placeholders so
// nothing of a capture's tokens or ids is kept.
import fs from "node:fs";
const [capturePath, release] = process.argv.slice(2);
const lines = fs.readFileSync(capturePath, "utf8").trim().split("\n").map((line) => JSON.parse(line));
const pairs = (raw) => {
  const out = [];
  for (let i = 0; i < raw.length; i += 2) {
    const name = raw[i].toLowerCase();
    let value = raw[i + 1];
    if (name === "authorization") value = value.replace(/^Bearer .*/, "Bearer <access>");
    if (name === "session_id" || name === "x-client-request-id") value = "<session>";
    if (name === "content-length") value = "<length>";
    out.push([name, value]);
  }
  return out;
};
const pick = (predicate) => lines.find(predicate);
const signedIn = pick((l) => l.url === "/v1/responses" && !l.host.includes(":"));
const token = pick((l) => l.url === "/api/accounts/oauth/token");
const viaPool = pick((l) => l.url === "/v1/responses" && l.host.includes(":"));
if (!signedIn || !token || !viaPool) {
  console.error("capture is missing a request");
  process.exit(1);
}
const body = (text) => {
  const value = JSON.parse(text);
  if (value.prompt_cache_key) value.prompt_cache_key = "<session>";
  return { fieldOrder: Object.keys(value), value };
};
const reference = {
  source: "scripts/conformance/pi-capture.sh: @earendil-works/pi-coding-agent run against a recording upstream",
  piRelease: release,
  responses: { headers: pairs(signedIn.rawHeaders), body: body(signedIn.body) },
  token: {
    headers: pairs(token.rawHeaders),
    form: [...new URLSearchParams(token.body).keys()],
  },
  viaPool: { headers: pairs(viaPool.rawHeaders), body: body(viaPool.body) },
};
process.stdout.write(JSON.stringify(reference, null, 2) + "\n");
