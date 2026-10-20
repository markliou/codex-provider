// Writes a ChatGPT-mode auth.json for the capture: unsigned tokens for a fake
// identity that only the fake upstream ever sees. A native Codex and the pool
// both load this same file, so any credential header they send must be equal.
const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
const claims = {
  exp: 4102444800,
  iat: 1700000000,
  email: "capture@example.invalid",
  "https://api.openai.com/auth": { chatgpt_account_id: "acct-capture", chatgpt_plan_type: "pro", chatgpt_user_id: "user-capture" },
};
const jwt = `${b64({ alg: "none", typ: "JWT" })}.${b64(claims)}.unsigned`;
process.stdout.write(JSON.stringify({
  auth_mode: "chatgpt",
  OPENAI_API_KEY: null,
  tokens: { id_token: jwt, access_token: jwt, refresh_token: "<capture-placeholder>", account_id: "acct-capture" },
  last_refresh: new Date().toISOString(),
}));
