// Checks the pool's record of how Codex reads upstream against a Codex source
// tree. Usage: node check-codex-shapes.mjs <codex-root> <fixture.json> <watchlist.txt>
//
// It fails when:
//   - a watched Codex path no longer exists (follow the rename, update the list);
//   - a fixture's marker is no longer in the Codex file it names (the shape
//     moved or changed: re-copy it from Codex), including the assumptions the
//     pool's own requests and the native endpoint rest on;
//   - Codex's error parsers name a code the fixture does not cover (Codex
//     learned a failure the pool would read as "unknown": classify it in
//     terminalFailureClass and add its shape to the fixture).
import fs from "node:fs";
import path from "node:path";

const [root, fixturePath, watchlistPath] = process.argv.slice(2);
if (!root || !fixturePath || !watchlistPath) {
  console.error("usage: check-codex-shapes.mjs <codex-root> <fixture.json> <watchlist.txt>");
  process.exit(2);
}
const fixture = JSON.parse(fs.readFileSync(fixturePath, "utf8"));
const problems = [];

const watched = fs.readFileSync(watchlistPath, "utf8").split("\n")
  .map((line) => line.trim()).filter((line) => line && !line.startsWith("#"))
  .map((line) => line.split(/\s+/)[1]);
for (const entry of watched) {
  if (!fs.existsSync(path.join(root, entry))) problems.push(`watched path is gone: ${entry}`);
}

for (const shape of [...fixture.failures, ...fixture.lifecycle, ...(fixture.assumptions || [])]) {
  const file = path.join(root, shape.source);
  if (!fs.existsSync(file)) { problems.push(`${shape.name}: source file is gone: ${shape.source}`); continue; }
  if (!fs.readFileSync(file, "utf8").includes(shape.marker)) problems.push(`${shape.name}: marker no longer in ${shape.source}: ${shape.marker}`);
}

// The files where Codex turns an upstream error into a decision. Only the
// non-test part is read, and only lines that compare or declare a code.
const parsers = [
  "codex-rs/codex-api/src/sse/responses_error.rs",
  "codex-rs/codex-api/src/sse/responses.rs",
  "codex-rs/codex-api/src/api_bridge.rs",
  "codex-rs/codex-api/src/error.rs",
  "codex-rs/codex-api/src/endpoint/responses_websocket.rs",
];
const known = new Set(fixture.failures.map((shape) => shape.code).filter(Boolean));
const ignored = new Set(Object.keys(fixture.ignoredCodexLiterals || {}));
const found = new Map();
for (const parser of parsers) {
  const file = path.join(root, parser);
  if (!fs.existsSync(file)) { problems.push(`error parser is gone: ${parser}`); continue; }
  let source = fs.readFileSync(file, "utf8");
  const tests = source.search(/#\[cfg\(test\)\]/);
  if (tests >= 0) source = source.slice(0, tests);
  for (const line of source.split("\n")) {
    if (/^\s*\/\//.test(line) || !/Some\(|^\s*\|\s*"|_CODE: &str|== Some\(|=> /.test(line)) continue;
    for (const match of line.matchAll(/"([a-z][a-z0-9]*(?:_[a-z0-9]+)+)"/g)) {
      if (!found.has(match[1])) found.set(match[1], parser);
    }
  }
}
for (const [code, parser] of found) {
  if (!known.has(code) && !ignored.has(code)) problems.push(`Codex parses "${code}" (${parser}) but the fixture has no shape for it`);
}

console.log(`checked ${watched.length} watched paths, ${fixture.failures.length + fixture.lifecycle.length} shapes, ${(fixture.assumptions || []).length} assumptions, ${found.size} codes Codex parses`);
if (problems.length) {
  for (const problem of problems) console.log(`FAIL ${problem}`);
  process.exit(1);
}
console.log("Codex shapes: ok");
