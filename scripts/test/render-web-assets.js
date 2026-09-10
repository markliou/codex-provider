// Executes every dashboard renderer on the public refresh path against
// representative payloads.
//
// refreshPublic() wraps all of them in one try/catch and returns false on any
// throw, and showPublicDashboard() reads that as "public status unavailable"
// and falls back to the login screen. A ReferenceError in any single renderer
// therefore takes the whole control page down, which is exactly how one
// undeclared identifier made the page look like it was demanding a password.
//
// `node --check` only parses and cannot see an undeclared identifier inside a
// template literal, and the Go suite never executes this bundle, so without
// this script nothing in the pipeline executes browser code at all.
const fs = require("fs");
const source = fs.readFileSync("cmd/codex-pool/web/app.js", "utf8");

// Every top-level declaration inside the bundle IIFE, in source order, so a
// renderer can reach any helper it calls without hand-maintaining a list.
const declaration = /^  (?:function ([A-Za-z_$][\w$]*)\s*\(|const ([A-Za-z_$][\w$]*)\s*=)/gm;
const starts = [];
for (let match = declaration.exec(source); match; match = declaration.exec(source)) {
  starts.push({ index: match.index, name: match[1] || match[2] });
}
if (starts.length === 0) throw new Error("no top-level declarations found; extractor is out of date");
const body = starts
  .map((entry, i) => source.slice(entry.index, i + 1 < starts.length ? starts[i + 1].index : source.lastIndexOf("})();")))
  .join("\n");

const elements = new Map();
const element = (key) => {
  if (!elements.has(key)) {
    elements.set(key, {
      innerHTML: "", textContent: "", hidden: false, disabled: false, value: "", dataset: {},
      classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
      setAttribute() {}, removeAttribute() {}, getAttribute: () => null,
      addEventListener() {}, removeEventListener() {}, appendChild() {}, querySelector: () => null,
      querySelectorAll: () => [], closest: () => null, focus() {}, scrollIntoView() {},
    });
  }
  return elements.get(key);
};
const storage = { getItem: () => null, setItem() {}, removeItem() {} };
const documentStub = {
  querySelector: (selector) => element(selector),
  querySelectorAll: () => [],
  createElement: () => element("created"),
  addEventListener() {}, removeEventListener() {},
  documentElement: element("html"), body: element("body"),
};
const windowStub = {
  setTimeout: () => 0, clearTimeout() {}, setInterval: () => 0, clearInterval() {},
  matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
  location: { href: "", reload() {} }, addEventListener() {}, removeEventListener() {},
};

const bundle = new Function(
  "document", "window", "sessionStorage", "localStorage", "fetch", "navigator", "console",
  body + "\nreturn { renderSummary, renderQuotaCapacity, renderThroughput, renderCacheWindow, renderPublicAccounts, renderRoutingCacheEvents };"
)(documentStub, windowStub, storage, storage, () => Promise.reject(new Error("no network in this guard")), { clipboard: null }, console);

const quotaWindow = (label, minutes, remaining) => ({
  role: "primary", label, percentage: remaining, usedPercent: 100 - remaining, remainingPercent: remaining,
  resetAt: Math.floor(Date.now() / 1000) + 3600, windowMinutes: minutes, observed: true, present: true,
});
const account = (overrides) => Object.assign({
  displayName: "ac***nt@example.test", detail: "Business · org", ownerNote: "note", statusTone: "ready",
  statusLabel: "Ready", outOfPool: false, poolLabel: "In pool", poolRef: "ref", poolAction: "pool-remove",
  poolActionLabel: "Leave pool", remainingQuota: 50, quotaUnavailable: false, quotaFreshness: "fresh",
  lastSuccessfulRefreshAt: new Date().toISOString(), quotaMetering: "chatgpt_subscription", active: true,
  seatType: "premium", seatTypeInferred: true, cacheWindow: {},
  quota: { windows: [quotaWindow("5h", 300, 40), quotaWindow("Week", 10080, 80)], additionalLimits: [], credits: null, individualLimit: null, resetCredits: { availableCount: 3, expiresAt: Math.floor(Date.now() / 1000) + 86400 } },
}, overrides);

const capacity = [
  { label: "5h", windowMinutes: 300, remainingPercent: 63, reportingAccounts: 3, routableAccounts: 4, uncappedAccounts: 1, exhaustedAccounts: 0 },
  { label: "Week", windowMinutes: 10080, remainingPercent: 63.25, reportingAccounts: 4, routableAccounts: 4, uncappedAccounts: 0, exhaustedAccounts: 2 },
];

const cases = [
  ["full payload", () => {
    bundle.renderSummary({ total: 8, ready: 3, low: 1, cooldown: 1, standby: 2, duplicate: 1, unavailable: 0 }, true);
    bundle.renderQuotaCapacity(capacity);
    bundle.renderThroughput({ current: { requestCount: 10, successRate: 1, averageLatencyMs: 1200, p50LatencyMs: 900, p95LatencyMs: 3000, cacheHitRate: 0.9, outputTokensPerSecond: 40, windowSeconds: 600 }, series: [], bucketIntervalSeconds: 60, seriesIntervalSeconds: 600, retentionHours: 48, activeRequests: 1 });
    bundle.renderCacheWindow({ requestCount: 100, cacheHitRequestCount: 90, coldRequestCount: 10, inputTokens: 1000, cachedTokens: 900, main: {}, subagent: {} });
    bundle.renderPublicAccounts([account({}), account({ quotaUnavailable: true, quota: null, seatType: "standard", seatTypeInferred: false })]);
    bundle.renderRoutingCacheEvents([]);
  }],
  // A payload from an older build, or one where a refresh reported nothing, must
  // degrade rather than throw.
  ["sparse payload", () => {
    bundle.renderSummary({}, true);
    bundle.renderQuotaCapacity([{ label: "Week", remainingPercent: 50, reportingAccounts: 2 }]);
    bundle.renderThroughput({});
    bundle.renderCacheWindow({});
    bundle.renderPublicAccounts([account({ detail: "", ownerNote: "", quota: null, remainingQuota: null, cacheWindow: null })]);
    bundle.renderRoutingCacheEvents([]);
  }],
  ["empty and missing", () => {
    bundle.renderSummary({}, true);
    bundle.renderQuotaCapacity([]);
    bundle.renderQuotaCapacity(undefined);
    bundle.renderThroughput(undefined);
    bundle.renderCacheWindow(undefined);
    bundle.renderPublicAccounts([]);
    bundle.renderRoutingCacheEvents(undefined);
  }],
];

for (const [name, run] of cases) {
  for (const el of elements.values()) el.innerHTML = "";
  run();
  for (const [selector, el] of elements) {
    if (/undefined|NaN/.test(el.innerHTML)) {
      throw new Error(`${name}: ${selector} rendered undefined/NaN: ${el.innerHTML.slice(0, 200)}`);
    }
  }
  console.log(`ok  ${name}`);
}
console.log("render-web-assets: every public-path renderer executed cleanly");
