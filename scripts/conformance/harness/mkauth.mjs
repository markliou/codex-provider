// Writes a ChatGPT-mode auth.json for the capture: unsigned tokens for a fake
// identity that only the fake upstream ever sees. A native Codex and the pool
// both load files from here, so any credential header they send must be equal.
//   node mkauth.mjs            a current credential
//   node mkauth.mjs expiring   an access token that expires in two minutes,
//                              inside the five-minute window in which both
//                              Codex and the pool refresh before using it
// An optional second argument names another fake account (default
// acct-capture), for the failover scenario's second pool account.
const mode = process.argv[2] || "current";
const account = process.argv[3] || "acct-capture";
const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
const token = (exp) => `${b64({ alg: "none", typ: "JWT" })}.${b64({
  exp,
  iat: 1700000000,
  email: `${account}@example.invalid`,
  "https://api.openai.com/auth": { chatgpt_account_id: account, chatgpt_plan_type: "pro", chatgpt_user_id: `user-${account}` },
})}.unsigned`;
const longLived = token(4102444800);
const now = Date.now();
process.stdout.write(JSON.stringify({
  auth_mode: "chatgpt",
  OPENAI_API_KEY: null,
  tokens: {
    id_token: longLived,
    access_token: mode === "expiring" ? token(Math.floor(now / 1000) + 120) : longLived,
    refresh_token: "<capture-placeholder>",
    account_id: account,
  },
  last_refresh: new Date(now).toISOString(),
}));
