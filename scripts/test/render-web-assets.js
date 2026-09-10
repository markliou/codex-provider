// Executes dashboard renderers against representative payloads. Any throw here
// would reach production as a blank status page falling back to login.
const fs = require("fs");
const src = fs.readFileSync("cmd/codex-pool/web/app.js", "utf8");

function grab(name) {
  const start = src.indexOf("function " + name + "(");
  if (start < 0) throw new Error("renderer not found: " + name);
  const rest = src.slice(start + 1);
  const end = rest.search(/\n  (function |const |\/\/)/);
  return src.slice(start, end < 0 ? src.length : start + 1 + end) + "\n";
}

const escapeHTML = (value) =>
  String(value ?? "").replace(/[&<>'"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" }[c]));
const store = {};
const $ = (sel) => ({ set innerHTML(v) { store[sel] = v; }, get innerHTML() { return store[sel] || ""; } });

const helpers = ["quotaPercent", "quotaTone", "quotaTrackMarkup", "renderQuotaCapacity"];
const bundle = new Function(
  "escapeHTML",
  "$",
  helpers.map(grab).join("\n") + "\nreturn { renderQuotaCapacity };"
)(escapeHTML, $);

const cases = [
  { name: "mixed capped and uncapped windows", input: [
    { label: "5h", windowMinutes: 300, remainingPercent: 63, reportingAccounts: 3, routableAccounts: 4, uncappedAccounts: 1, exhaustedAccounts: 0 },
    { label: "Week", windowMinutes: 10080, remainingPercent: 63.25, reportingAccounts: 4, routableAccounts: 4, uncappedAccounts: 0, exhaustedAccounts: 2 },
  ] },
  // Older builds and partial payloads must not throw; the page must degrade.
  { name: "payload without the newer counts", input: [{ label: "Week", remainingPercent: 50, reportingAccounts: 2 }] },
  { name: "empty", input: [] },
  { name: "not an array", input: undefined },
];

for (const testCase of cases) {
  store["#quota-capacity"] = "";
  bundle.renderQuotaCapacity(testCase.input);
  const html = store["#quota-capacity"];
  if (/undefined|NaN/.test(html)) {
    throw new Error(`${testCase.name}: renderer produced undefined/NaN: ${html}`);
  }
  console.log(`ok  ${testCase.name}`);
}
console.log("render-web-assets: all renderers executed cleanly");
