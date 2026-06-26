(() => {
  const state = { csrfToken: sessionStorage.getItem("codexPoolCsrf") || "", data: null, refreshTimer: null, deviceAuthTimer: null, deviceAuthPollTimer: null, currentLoginJobId: "", mode: "public" };
  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => document.querySelectorAll(selector);
  const loginView = $("#login-view");
  const dashboardView = $("#dashboard-view");
  const refreshIntervalMs = 30 * 1000;

  const escapeHTML = (value) => String(value ?? "").replace(/[&<>'"]/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" }[character]));
  const displayTime = (value) => {
    if (!value || value === "0001-01-01T00:00:00Z") return "No activity";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? "No activity" : date.toLocaleString();
  };
  const statusLabel = (status) => ({ ready: "Ready", low: "Low quota", cooldown: "Cooldown", error: "Error", disabled: "Disabled", standby: "Out of pool", duplicate: "Duplicate", missing_auth: "Login needed" }[status] || "Unknown");
  const activeBadge = (active) => active ? '<span class="badge active">Active</span>' : "";
  const cacheHitRate = (input, cached) => {
    const total = Number(input) || 0;
    if (total <= 0) return null;
    return Math.max(0, Math.min(1, (Number(cached) || 0) / total));
  };
  const cacheHitMarkup = (health) => {
    const rate = cacheHitRate(health.cacheInputTokens, health.cacheCachedTokens);
    if (rate === null) return '<div class="cache-hit"><span class="cache-empty">No data</span></div>';
    const pct = (rate * 100).toFixed(1);
    const tone = rate >= 0.6 ? "good" : rate >= 0.3 ? "fair" : "poor";
    return `<div class="cache-hit"><span class="cache-rate ${tone}">${pct}%</span><span class="cache-detail">${formatTokens(health.cacheCachedTokens)} / ${formatTokens(health.cacheInputTokens)} tok</span></div>`;
  };
  const formatTokens = (value) => {
    const n = Number(value) || 0;
    if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
    if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
    if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
    return String(n);
  };

  function notify(message, error = false) {
    if (!error) return;
    const serviceStatus = $("#service-status");
    if (serviceStatus) serviceStatus.textContent = message;
  }

  function formatRemaining(ms) {
    const totalSeconds = Math.max(0, Math.ceil(ms / 1000));
    const minutes = String(Math.floor(totalSeconds / 60)).padStart(2, "0");
    const seconds = String(totalSeconds % 60).padStart(2, "0");
    return `${minutes}:${seconds}`;
  }

  function startDeviceAuthCountdown(expiresAt) {
    const countdown = $("#device-auth-countdown");
    const deadline = expiresAt ? new Date(expiresAt).getTime() : Date.now() + 15 * 60 * 1000;
    const tick = () => {
      const remaining = deadline - Date.now();
      countdown.textContent = formatRemaining(remaining);
      countdown.classList.toggle("expired", remaining <= 0);
    };
    window.clearInterval(state.deviceAuthTimer);
    tick();
    state.deviceAuthTimer = window.setInterval(tick, 1000);
  }

  function showDeviceAuth(job) {
    const dialog = $("#device-auth-dialog");
    const url = $("#device-auth-url");
    const code = $("#device-auth-code");
    if (job.verificationUrl) {
      url.textContent = job.verificationUrl;
      url.href = job.verificationUrl;
    } else {
      url.textContent = "";
      url.removeAttribute("href");
    }
    code.textContent = job.userCode || "";
    startDeviceAuthCountdown(job.codeExpiresAt);
    if (!dialog.open) dialog.showModal();
  }

  async function cancelDeviceAuthJob(jobId) {
    if (!jobId) return;
    try {
      await api(`/jobs/${encodeURIComponent(jobId)}/cancel`, { method: "POST" });
    } catch (error) {
      notify(error.message, true);
    }
  }

  function closeDeviceAuth(cancelJob = false) {
    const dialog = $("#device-auth-dialog");
    window.clearInterval(state.deviceAuthTimer);
    window.clearTimeout(state.deviceAuthPollTimer);
    state.deviceAuthTimer = null;
    state.deviceAuthPollTimer = null;
    const jobId = state.currentLoginJobId;
    state.currentLoginJobId = "";
    if (dialog.open) dialog.close();
    if (cancelJob && jobId) cancelDeviceAuthJob(jobId);
  }

  async function api(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (options.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    if (options.method && options.method !== "GET") headers.set("X-CSRF-Token", state.csrfToken);
    const response = await fetch(`/admin/api${path}`, { credentials: "same-origin", ...options, headers });
    if (response.status === 401) { showPublicDashboard(); throw new Error("Your session has expired"); }
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error?.message || `Request failed (${response.status})`);
    return body;
  }

  async function publicApi(path, options = {}) {
    const headers = new Headers(options.headers || {});
    const response = await fetch(`/admin/api/public-dashboard${path}`, { credentials: "same-origin", ...options, headers });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error?.message || `Request failed (${response.status})`);
    return body;
  }

  function showLogin(message = "") {
    dashboardView.hidden = true;
    loginView.hidden = false;
    $$(".management-only, .public-only").forEach((element) => { element.hidden = true; });
    $("#login-error").textContent = message;
    $("#login-error").hidden = !message;
    window.clearInterval(state.refreshTimer);
  }

  function showDashboard() {
    state.mode = "management";
    loginView.hidden = true;
    dashboardView.hidden = false;
    $$(".management-only").forEach((element) => { element.hidden = false; });
    $$(".public-only").forEach((element) => { element.hidden = true; });
    $("#dashboard-eyebrow").textContent = "MANAGE";
    $("#dashboard-title").textContent = "Account pool";
    refresh();
    window.clearInterval(state.refreshTimer);
    state.refreshTimer = window.setInterval(() => refresh(true), refreshIntervalMs);
  }

  async function showPublicDashboard() {
    // Product contract: the control page opens in public mode. Do not replace
    // this with an immediate login screen; password auth unlocks management mode
    // on the same page, while public status stays visible by default.
    state.mode = "public";
    $("#dashboard-eyebrow").textContent = "SERVICE STATUS";
    $("#dashboard-title").textContent = "Pool status";
    window.clearInterval(state.refreshTimer);
    const ok = await refreshPublic(true);
    if (!ok && state.mode === "public") {
      showLogin();
      return;
    }
    loginView.hidden = true;
    dashboardView.hidden = false;
    $$(".management-only").forEach((element) => { element.hidden = true; });
    $$(".public-only").forEach((element) => { element.hidden = false; });
    state.refreshTimer = window.setInterval(() => refreshPublic(true), refreshIntervalMs);
  }

  function renderSummary(summary, publicMode = false, cacheAggregate = null) {
    const items = publicMode ? [
      ["Total accounts", summary.total || 0, ""],
      ["Ready", summary.ready || 0, ""],
      ["Limited", summary.low || 0, "low"],
      ["Out of pool", summary.standby || 0, "missing_auth"],
      ["Unavailable", summary.unavailable || 0, "error"],
    ] : [
      ["Total accounts", summary.total || 0, ""],
      ["Ready", summary.ready || 0, ""],
      ["Low quota", summary.low || 0, "low"],
      ["Errors", summary.error || 0, "error"],
      ["Needs attention", summary.missing_auth || 0, "missing_auth"],
      ["Cache hit", cacheAggregate, "cache"],
    ];
    $("#summary-grid").innerHTML = items.map(([label, value, tone]) => {
      const display = tone === "cache"
        ? (value === null || value === undefined ? "No data" : `${(value * 100).toFixed(1)}%`)
        : value;
      return `<div class="summary-item ${tone}"><div class="eyebrow">${label}</div><span class="summary-value">${display}</span></div>`;
    }).join("");
  }

  function renderSettings(serviceState) {
    const preserveSwitch = $("#preserve-pro-quota-switch");
    if (!preserveSwitch) return;
    preserveSwitch.checked = Boolean(serviceState?.preserveProQuota);
    preserveSwitch.disabled = false;
  }

  function displayUnixTime(value) {
    if (!value) return "";
    const date = new Date(Number(value) * 1000);
    return Number.isNaN(date.getTime()) ? "" : date.toLocaleString();
  }

  function displayResetCountdown(value) {
    const seconds = Math.max(0, Math.ceil(Number(value) - Date.now() / 1000));
    if (!Number.isFinite(seconds)) return "";
    if (seconds === 0) return "now";
    const days = Math.floor(seconds / 86400);
    const hours = Math.floor((seconds % 86400) / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    if (days) return `${days}d ${hours}h`;
    if (hours) return `${hours}h ${minutes}m`;
    return `${Math.max(1, minutes)}m`;
  }

  function quotaPercent(value) {
    const percentage = Number(value ?? 100);
    return Number.isFinite(percentage) ? Math.min(100, Math.max(0, percentage)) : 0;
  }

  function quotaTrackMarkup(value, label) {
    const remaining = quotaPercent(value);
    const tone = remaining <= 0 ? "empty" : remaining <= 20 ? "low" : "";
    return `<progress class="quota-track ${tone}" value="${remaining}" max="100" aria-label="${escapeHTML(label)} quota remaining">${remaining}%</progress>`;
  }

  function quotaWindowMarkup(label, window) {
    if (!window || !window.present) return "";
    const value = quotaPercent(window.percentage);
    const reset = displayUnixTime(window.resetAt);
    const countdown = displayResetCountdown(window.resetAt);
    return `<div class="quota-window"><div class="quota-line"><span>${label} quota</span><strong>${value}% left</strong></div>${quotaTrackMarkup(value, label)}${reset ? `<div class="quota-reset" title="${escapeHTML(reset)}">Resets in ${escapeHTML(countdown || "soon")}</div>` : ""}</div>`;
  }

  function quotaMarkup(value, quota, quotaError, usageUpdatedAt) {
    const refreshError = quotaError ? `<span class="quota-error" title="${escapeHTML(quotaError.message)}">Quota update unavailable</span>` : "";
    if (quota) {
      const windows = [quotaWindowMarkup("5h", quota.hourly), quotaWindowMarkup("Week", quota.weekly)].filter(Boolean).join("");
      const updated = usageUpdatedAt && usageUpdatedAt !== "0001-01-01T00:00:00Z" ? `<div class="quota-updated">Updated ${escapeHTML(displayTime(usageUpdatedAt))}</div>` : "";
      return `<div class="quota quota-detailed">${windows || '<span class="quota-unknown">Quota unavailable</span>'}${updated}${refreshError}</div>`;
    }
    if (quotaError) return refreshError;
    if (value === null || value === undefined) return '<span class="quota-unknown">Not reported</span>';
    return `<div class="quota"><div class="quota-line"><span>Quota</span><strong>${value}% left</strong></div>${quotaTrackMarkup(value, "Quota")}</div>`;
  }

  function authLabel(value) {
    if (value === "codex_device_auth") return "Codex sign-in";
    if (value === "provider_api_key") return "Provider API key";
    return value ? value.replaceAll("_", " ") : "Codex sign-in";
  }

  function actionButton(action, id, label, tone = "secondary") {
    return `<button class="button ${tone}" type="button" data-account-action="${action}" data-account-id="${escapeHTML(id)}">${label}</button>`;
  }

  function accountMetadataLine(account, includeID = false) {
    const metadata = account.credentialMetadata || account;
    const parts = [];
    const planDisplay = metadata.planDisplayName || metadata.planType;
    if (metadata.planType && metadata.planType !== "unknown") parts.push(planDisplay);
    const planSegments = String(planDisplay || "").split(" · ").map((part) => part.trim()).filter(Boolean);
    if (metadata.organizationName && !planSegments.includes(metadata.organizationName)) parts.push(metadata.organizationName);
    if (metadata.email) parts.push(metadata.email);
    if (includeID && account.id) parts.push(account.id);
    return parts.join(" · ");
  }

  function renderAccounts(accounts, healthByID) {
    $("#accounts-head").innerHTML = "<tr><th>Account</th><th>Status</th><th>Quota</th><th>Routing</th><th>Cache hit</th><th>Last activity</th><th>Action</th></tr>";
    $("#account-count").textContent = `${accounts.length} configured`;
    const body = $("#accounts-body");
    if (!accounts.length) {
      body.innerHTML = '<tr><td colspan="7"><div class="empty-state">No accounts configured</div></td></tr>';
      return;
    }
    body.innerHTML = accounts.map((account) => {
      const health = healthByID.get(account.id) || { status: "standby", statusReason: "No health data" };
      const activity = health.status === "error" ? health.lastFailureAt : health.lastSuccessAt;
      const route = account.inPool ? "In pool" : "Out of pool";
      const displayName = account.displayName || account.label || account.id || "Credential";
      const metadata = accountMetadataLine(account, false);
      const actions = actionButton("delete", account.id, "Remove", "danger");
      return `<tr data-account-row="${escapeHTML(account.id)}">
        <td><div class="account-name">${escapeHTML(displayName)}${metadata ? `<span class="account-id">${escapeHTML(metadata)}</span>` : ""}</div></td>
        <td><div class="status-stack"><span class="badge ${escapeHTML(health.status)}">${statusLabel(health.status)}</span>${activeBadge(health.active)}</div></td>
        <td>${quotaMarkup(health.remainingQuota ?? account.remainingQuota, health.quota, health.quotaError, health.usageUpdatedAt)}</td>
        <td><div class="route"><strong>${escapeHTML(authLabel(account.authType))}</strong><br>${escapeHTML(route)}</div></td>
        <td>${cacheHitMarkup(health)}</td>
        <td><div class="activity">${displayTime(activity)}${health.consecutiveFailure ? `<br>${health.consecutiveFailure} consecutive failure${health.consecutiveFailure === 1 ? "" : "s"}` : ""}</div></td>
        <td><div class="row-actions">${actions}</div></td>
      </tr>`;
    }).join("");
  }

  function renderPublicAccounts(accounts) {
    $("#accounts-head").innerHTML = "<tr><th>Account</th><th>Status</th><th>Quota</th><th>Pool</th><th>Action</th></tr>";
    $("#account-count").textContent = `${accounts.length} visible`;
    const body = $("#accounts-body");
    if (!accounts.length) {
      body.innerHTML = '<tr><td colspan="5"><div class="empty-state">No accounts available</div></td></tr>';
      return;
    }
    body.innerHTML = accounts.map((account) => {
      const displayName = account.displayName || account.label || "Credential";
      const metadata = account.detail || "";
      const tone = account.statusTone || account.status || "standby";
      const label = account.statusLabel || statusLabel(account.status);
      const quota = account.quotaUnavailable ? '<span class="quota-unknown">Quota unavailable</span>' : quotaMarkup(account.remainingQuota, account.quota, null, null);
      const action = account.poolRef && account.poolAction
        ? `<button class="button ${account.poolAction === "pool-remove" ? "warn" : "secondary"}" type="button" data-public-pool-action="${escapeHTML(account.poolAction)}" data-pool-ref="${escapeHTML(account.poolRef)}">${escapeHTML(account.poolActionLabel || "Update")}</button>`
        : "";
      return `<tr>
      <td><div class="account-name">${escapeHTML(displayName)}${metadata ? `<span class="account-id">${escapeHTML(metadata)}</span>` : ""}</div></td>
      <td><div class="status-stack"><span class="badge ${escapeHTML(tone)}">${escapeHTML(label)}</span>${activeBadge(account.active)}</div></td>
      <td>${quota}</td>
      <td><div class="route"><strong>${escapeHTML(account.poolLabel || "Unavailable")}</strong></div></td>
      <td><div class="row-actions">${action}</div></td>
    </tr>`;
    }).join("");
  }

  function maskRouteKey(value) {
    const key = String(value || "").trim();
    if (!key) return "";
    if (key.length <= 28) return key;
    return `${key.slice(0, 16)}...${key.slice(-8)}`;
  }

  function renderSticky(sessions, accounts = []) {
    const accountsByID = new Map(accounts.map((account) => [account.id, account]));
    $("#sticky-count").textContent = sessions.length === 1 ? "1 active route" : `${sessions.length} active routes`;
    $("#sticky-list").innerHTML = sessions.length ? sessions.map((session) => {
      const account = accountsByID.get(session.accountId);
      const accountName = account?.displayName || account?.label || "Assigned credential";
      const routeName = session.modelId || "Default model";
      // Active routes are management diagnostics. Show a masked route key so the
      // owner can tell sessions apart, but keep the full key out of visible text
      // because it may include project or client-provided session hints.
      const routeKey = maskRouteKey(session.key);
      const sessionDetail = routeKey ? ` · Session ${escapeHTML(routeKey)}` : "";
      const expires = session.expiresAt && session.expiresAt !== "0001-01-01T00:00:00Z" ? ` · Expires ${escapeHTML(displayTime(session.expiresAt))}` : "";
      // A populated failoverFrom means this route was moved off another account
      // (e.g. after a 429/5xx). That switch starts the new account's prompt cache
      // cold, so flag it as a hit-rate diagnostic.
      let switched = "";
      if (session.failoverFrom && session.failoverFrom !== session.accountId) {
        const from = accountsByID.get(session.failoverFrom);
        const fromName = from?.displayName || from?.label || session.failoverFrom;
        switched = ` <span class="sticky-switched" title="Routing switched accounts; prompt cache restarted cold">↪ switched from ${escapeHTML(fromName)}</span>`;
      }
      return `<div class="sticky-item"><div><div class="sticky-key">${escapeHTML(routeName)}</div><div class="sticky-meta">${escapeHTML(accountName)}${sessionDetail} · Last used ${escapeHTML(displayTime(session.lastSuccessAt))}${expires}${switched}</div></div><button class="button secondary" type="button" data-sticky-key="${escapeHTML(session.key)}">Clear</button></div>`;
    }).join("") : '<div class="empty-state">No active routes</div>';
  }

  async function refresh(silent = false) {
    try {
      const [stateResponse, accountsResponse, healthResponse, sessionsResponse] = await Promise.all([api("/state"), api("/accounts"), api("/accounts/health"), api("/sticky-sessions")]);
      const serviceState = stateResponse.state;
      const healthByID = new Map(healthResponse.accounts.map((item) => [item.accountId, item]));
      state.data = { serviceState, accounts: accountsResponse.accounts, healthByID, sessions: sessionsResponse.sessions };
      let cacheInput = 0, cacheCached = 0;
      for (const item of healthResponse.accounts) {
        cacheInput += Number(item.cacheInputTokens) || 0;
        cacheCached += Number(item.cacheCachedTokens) || 0;
      }
      renderSettings(serviceState);
      renderSummary(serviceState.summary || {}, false, cacheHitRate(cacheInput, cacheCached));
      renderAccounts(state.data.accounts, healthByID);
      renderSticky(state.data.sessions, state.data.accounts);
      $("#service-status").textContent = "Service online";
    } catch (error) {
      if (!silent) notify(error.message, true);
      $("#service-status").textContent = "Service unavailable";
    }
  }

  async function refreshPublic(silent = false) {
    try {
      const response = await fetch("/admin/api/public-dashboard", { credentials: "same-origin" });
      const body = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(body.error?.message || `Request failed (${response.status})`);
      renderSummary(body.dashboard.summary || {}, true);
      renderPublicAccounts(body.dashboard.accounts || []);
      return true;
    } catch (error) {
      if (!silent) notify(error.message, true);
      return false;
    }
  }

  async function handleAccountAction(button) {
    const action = button.dataset.accountAction;
    const id = button.dataset.accountId;
    try {
      if (action === "delete") {
        if (!window.confirm(`Remove account ${id}?`)) return;
        await api(`/accounts/${encodeURIComponent(id)}`, { method: "DELETE" });
      } else {
        await api(`/accounts/${encodeURIComponent(id)}/${action}`, { method: "POST" });
      }
      notify("Account updated");
      refresh(true);
    } catch (error) { notify(error.message, true); }
  }

  async function handlePublicPoolAction(button) {
    const action = button.dataset.publicPoolAction;
    const ref = button.dataset.poolRef;
    button.disabled = true;
    try {
      await publicApi(`/accounts/${encodeURIComponent(ref)}/${action}`, { method: "POST" });
      await refreshPublic(true);
    } catch (error) {
      button.disabled = false;
      notify(error.message, true);
    }
  }

  async function startDeviceAuth(accountId) {
    const response = await api(`/accounts/${encodeURIComponent(accountId)}/login`, { method: "POST" });
    state.currentLoginJobId = response.job.jobId;
    watchLoginJob(response.job.jobId);
  }

  async function createAccountAndStartLogin() {
    try {
      const response = await api("/accounts", { method: "POST", body: JSON.stringify({ authType: "codex_device_auth", priority: 100, enabled: true, inPool: true }) });
      await refresh(true);
      await startDeviceAuth(response.account.id);
    } catch (error) {
      notify(error.message, true);
    }
  }

  async function updatePreserveProQuota(event) {
    const input = event.currentTarget;
    const previous = state.data?.serviceState?.preserveProQuota ?? false;
    input.disabled = true;
    try {
      const response = await api("/settings", { method: "POST", body: JSON.stringify({ preserveProQuota: input.checked }) });
      state.data = { ...(state.data || {}), serviceState: response.state };
      renderSettings(response.state);
      $("#service-status").textContent = "Settings updated";
      refresh(true);
    } catch (error) {
      input.checked = Boolean(previous);
      input.disabled = false;
      notify(error.message, true);
    }
  }

  async function watchLoginJob(jobId) {
    let attempts = 0;
    const tick = async () => {
      if (state.currentLoginJobId !== jobId) return;
      attempts += 1;
      try {
        const response = await api(`/jobs/${encodeURIComponent(jobId)}`);
        const job = response.job;
        if (state.currentLoginJobId !== jobId) return;
        if (job.status === "waiting_for_user" && (job.verificationUrl || job.userCode)) {
          showDeviceAuth(job);
        }
        if (job.status === "completed") {
          closeDeviceAuth(false);
          notify("Sign-in completed");
          refresh(true);
          return;
        }
        if (job.status === "failed" || job.status === "cancelled") {
          closeDeviceAuth(false);
          if (job.status === "cancelled") {
            refresh(true);
            return;
          }
          notify("Sign-in failed", true);
          refresh(true);
          return;
        }
        if (attempts < 180) state.deviceAuthPollTimer = window.setTimeout(tick, 5000);
      } catch (error) {
        notify(error.message, true);
      }
    };
    tick();
  }

  $("#login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    try {
      const response = await fetch("/admin/api/login", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password: form.get("password") }) });
      const body = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(body.error?.message || "Unable to sign in");
      state.csrfToken = body.csrfToken;
      sessionStorage.setItem("codexPoolCsrf", state.csrfToken);
      $("#password").value = "";
      showDashboard();
    } catch (error) { $("#login-error").textContent = error.message; $("#login-error").hidden = false; }
  });

  $("#refresh-button").addEventListener("click", () => refresh());
  $("#sign-in-button").addEventListener("click", () => showLogin());
  $("#logout-button").addEventListener("click", async () => { try { await api("/logout", { method: "POST" }); } catch (_) {} sessionStorage.removeItem("codexPoolCsrf"); state.csrfToken = ""; showPublicDashboard(); });
  $("#add-account-button").addEventListener("click", createAccountAndStartLogin);
  $("#preserve-pro-quota-switch").addEventListener("change", updatePreserveProQuota);
  $("#close-device-auth-button").addEventListener("click", () => closeDeviceAuth(true));
  $("#accounts-body").addEventListener("click", (event) => {
    const publicButton = event.target.closest("[data-public-pool-action]");
    if (publicButton) {
      handlePublicPoolAction(publicButton);
      return;
    }
    const button = event.target.closest("[data-account-action]");
    if (button) handleAccountAction(button);
  });
  $("#sticky-list").addEventListener("click", async (event) => { const button = event.target.closest("[data-sticky-key]"); if (!button) return; try { await api(`/sticky-sessions/${encodeURIComponent(button.dataset.stickyKey)}`, { method: "DELETE" }); notify("Route cleared"); refresh(true); } catch (error) { notify(error.message, true); } });

  showPublicDashboard();
})();
