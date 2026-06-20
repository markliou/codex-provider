(() => {
  const state = { csrfToken: sessionStorage.getItem("codexPoolCsrf") || "", data: null, refreshTimer: null, mode: "public" };
  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => document.querySelectorAll(selector);
  const loginView = $("#login-view");
  const dashboardView = $("#dashboard-view");
  const toast = $("#toast");

  const escapeHTML = (value) => String(value ?? "").replace(/[&<>'"]/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" }[character]));
  const displayTime = (value) => {
    if (!value || value === "0001-01-01T00:00:00Z") return "No activity";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? "No activity" : date.toLocaleString();
  };
  const statusLabel = (status) => ({ ready: "Ready", low: "Low quota", cooldown: "Cooldown", error: "Error", disabled: "Disabled", standby: "Standby", missing_auth: "Login needed" }[status] || "Unknown");

  function notify(message, error = false) {
    toast.textContent = message;
    toast.classList.toggle("error", error);
    toast.hidden = false;
    window.clearTimeout(notify.timer);
    notify.timer = window.setTimeout(() => { toast.hidden = true; }, 4200);
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
    $("#dashboard-eyebrow").textContent = "OPERATIONAL OVERVIEW";
    $("#dashboard-title").textContent = "Account pool";
    refresh();
    window.clearInterval(state.refreshTimer);
    state.refreshTimer = window.setInterval(() => refresh(true), 30000);
  }

  function showPublicDashboard() {
    state.mode = "public";
    loginView.hidden = true;
    dashboardView.hidden = false;
    $$(".management-only").forEach((element) => { element.hidden = true; });
    $$(".public-only").forEach((element) => { element.hidden = false; });
    $("#dashboard-eyebrow").textContent = "PUBLIC STATUS";
    $("#dashboard-title").textContent = "Pool status";
    refreshPublic();
    window.clearInterval(state.refreshTimer);
    state.refreshTimer = window.setInterval(() => refreshPublic(true), 30000);
  }

  function renderSummary(summary) {
    const items = [
      ["Total accounts", summary.total || 0, ""],
      ["Ready", summary.ready || 0, ""],
      ["Low quota", summary.low || 0, "low"],
      ["Cooling down", summary.cooldown || 0, "cooldown"],
      ["Errors", summary.error || 0, "error"],
      ["Need login", summary.missing_auth || 0, "missing_auth"],
    ];
    $("#summary-grid").innerHTML = items.map(([label, value, tone]) => `<div class="summary-item ${tone}"><div class="eyebrow">${label}</div><span class="summary-value">${value}</span></div>`).join("");
  }

  function quotaMarkup(value) {
    if (value === null || value === undefined) return '<span class="quota-unknown">Not reported</span>';
    const tone = value <= 0 ? "empty" : value <= 20 ? "low" : "";
    return `<div class="quota"><div class="quota-line"><span>Remaining</span><strong>${value}%</strong></div><div class="quota-track"><div class="quota-fill ${tone}" style="width:${value}%"></div></div></div>`;
  }

  function actionButton(action, id, label, tone = "secondary") {
    return `<button class="button ${tone}" type="button" data-account-action="${action}" data-account-id="${escapeHTML(id)}">${label}</button>`;
  }

  function renderAccounts(accounts, healthByID) {
    $("#accounts-head").innerHTML = "<tr><th>Account</th><th>Status</th><th>Quota</th><th>Routing</th><th>Last activity</th><th>Action</th></tr>";
    $("#account-count").textContent = `${accounts.length} configured`;
    const body = $("#accounts-body");
    if (!accounts.length) {
      body.innerHTML = '<tr><td colspan="6"><div class="empty-state">No accounts configured</div></td></tr>';
      return;
    }
    body.innerHTML = accounts.map((account) => {
      const health = healthByID.get(account.id) || { status: "standby", statusReason: "No health data" };
      const reason = health.status === "error" ? health.lastFailureReason || health.statusReason : health.statusReason;
      const activity = health.status === "error" ? health.lastFailureAt : health.lastSuccessAt;
      const route = `${account.inPool ? "In pool" : "Out of pool"} · priority ${account.priority}`;
      const actions = [
        account.authType === "codex_device_auth" ? actionButton("login", account.id, "Login") : "",
        account.enabled ? actionButton("disable", account.id, "Disable", "warn") : actionButton("enable", account.id, "Enable"),
        account.inPool ? actionButton("pool-remove", account.id, "Remove pool") : actionButton("pool-add", account.id, "Add pool"),
        actionButton("quota", account.id, "Quota"),
        health.cooldowns?.length ? actionButton("cooldowns/clear", account.id, "Clear cooldown") : "",
        actionButton("delete", account.id, "Remove", "danger"),
      ].join("");
      return `<tr>
        <td><div class="account-name">${escapeHTML(account.label || account.id)}<span class="account-id">${escapeHTML(account.id)}</span></div></td>
        <td><div class="status-stack"><span class="badge ${escapeHTML(health.status)}">${statusLabel(health.status)}</span><span class="status-reason" title="${escapeHTML(reason)}">${escapeHTML(reason)}</span></div></td>
        <td>${quotaMarkup(health.remainingQuota ?? account.remainingQuota)}</td>
        <td><div class="route"><strong>${escapeHTML(account.authType || "codex_device_auth")}</strong><br>${escapeHTML(route)}</div></td>
        <td><div class="activity">${displayTime(activity)}${health.consecutiveFailure ? `<br>${health.consecutiveFailure} consecutive failure${health.consecutiveFailure === 1 ? "" : "s"}` : ""}</div></td>
        <td><div class="row-actions">${actions}</div></td>
      </tr>`;
    }).join("");
  }

  function renderPublicAccounts(accounts) {
    $("#accounts-head").innerHTML = "<tr><th>Account</th><th>Status</th><th>Quota</th><th>Models</th></tr>";
    $("#account-count").textContent = `${accounts.length} visible`;
    const body = $("#accounts-body");
    if (!accounts.length) {
      body.innerHTML = '<tr><td colspan="4"><div class="empty-state">No accounts available</div></td></tr>';
      return;
    }
    body.innerHTML = accounts.map((account) => `<tr>
      <td><div class="account-name">${escapeHTML(account.label)}</div></td>
      <td><span class="badge ${escapeHTML(account.status)}">${statusLabel(account.status)}</span></td>
      <td>${quotaMarkup(account.remainingQuota)}</td>
      <td><div class="route">${escapeHTML((account.allowedModels || []).join(", ") || "All configured models")}</div></td>
    </tr>`).join("");
  }

  function renderSticky(sessions) {
    $("#sticky-count").textContent = `${sessions.length} active`;
    $("#sticky-list").innerHTML = sessions.length ? sessions.map((session) => `<div class="sticky-item"><div><div class="sticky-key" title="${escapeHTML(session.key)}">${escapeHTML(session.key)}</div><div class="sticky-meta">${escapeHTML(session.accountId)} · ${displayTime(session.lastSuccessAt)}</div></div><button class="button secondary" type="button" data-sticky-key="${escapeHTML(session.key)}">Clear</button></div>`).join("") : '<div class="empty-state">No active sticky sessions</div>';
  }

  function renderTraffic(serviceState) {
    const items = [["Requests", serviceState.requestCount || 0], ["Succeeded", serviceState.successCount || 0], ["Failed", serviceState.failureCount || 0], ["Default model", serviceState.defaultModel || "-" ]];
    $("#traffic-stats").innerHTML = items.map(([label, value]) => `<div><dt>${label}</dt><dd>${escapeHTML(value)}</dd></div>`).join("");
  }

  async function refresh(silent = false) {
    try {
      const [stateResponse, accountsResponse, healthResponse, sessionsResponse] = await Promise.all([api("/state"), api("/accounts"), api("/accounts/health"), api("/sticky-sessions")]);
      const serviceState = stateResponse.state;
      const healthByID = new Map(healthResponse.accounts.map((item) => [item.accountId, item]));
      state.data = { serviceState, accounts: accountsResponse.accounts, healthByID, sessions: sessionsResponse.sessions };
      renderSummary(serviceState.summary || {});
      renderAccounts(state.data.accounts, healthByID);
      renderSticky(state.data.sessions);
      renderTraffic(serviceState);
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
      renderSummary(body.dashboard.summary || {});
      renderPublicAccounts(body.dashboard.accounts || []);
    } catch (error) {
      if (!silent) notify(error.message, true);
    }
  }

  async function handleAccountAction(button) {
    const action = button.dataset.accountAction;
    const id = button.dataset.accountId;
    try {
      if (action === "delete") {
        if (!window.confirm(`Remove account ${id}?`)) return;
        await api(`/accounts/${encodeURIComponent(id)}`, { method: "DELETE" });
      } else if (action === "login") {
        const response = await api(`/accounts/${encodeURIComponent(id)}/login`, { method: "POST" });
        notify("Device login started");
        watchLoginJob(response.job.jobId);
      } else if (action === "quota") {
        const current = state.data.healthByID.get(id)?.remainingQuota;
        const value = window.prompt("Remaining quota percentage (0-100)", current ?? "");
        if (value === null) return;
        const quota = Number(value);
        if (!Number.isInteger(quota) || quota < 0 || quota > 100) throw new Error("Quota must be an integer from 0 to 100");
        await api(`/accounts/${encodeURIComponent(id)}/quota/set`, { method: "POST", body: JSON.stringify({ remainingQuota: quota }) });
      } else {
        await api(`/accounts/${encodeURIComponent(id)}/${action}`, { method: "POST" });
      }
      notify("Account updated");
      refresh(true);
    } catch (error) { notify(error.message, true); }
  }

  async function watchLoginJob(jobId) {
    let attempts = 0;
    const tick = async () => {
      attempts += 1;
      try {
        const response = await api(`/jobs/${encodeURIComponent(jobId)}`);
        const job = response.job;
        if (job.status === "waiting_for_user") {
          const parts = [job.message || "Open the verification URL and enter the code."];
          if (job.verificationUrl) parts.push(job.verificationUrl);
          if (job.userCode) parts.push(`Code: ${job.userCode}`);
          notify(parts.join("  "));
        }
        if (job.status === "completed") {
          notify("Device login completed");
          refresh(true);
          return;
        }
        if (job.status === "failed") {
          notify(job.error || job.message || "Device login failed", true);
          refresh(true);
          return;
        }
        if (attempts < 180) window.setTimeout(tick, 5000);
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
      const response = await fetch("/admin/api/login", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ username: form.get("username"), password: form.get("password") }) });
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
  $("#add-account-button").addEventListener("click", () => { $("#account-form").reset(); $("#account-form-error").hidden = true; $("#account-dialog").showModal(); });
  $("#close-dialog-button").addEventListener("click", () => $("#account-dialog").close());
  $("#cancel-account-button").addEventListener("click", () => $("#account-dialog").close());
  $("#account-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const quota = String(form.get("remainingQuota") || "").trim();
    const payload = { id: String(form.get("id")).trim(), label: String(form.get("label")).trim(), authType: "codex_device_auth", allowedModels: String(form.get("allowedModels")).split(",").map((value) => value.trim()).filter(Boolean), priority: Number(form.get("priority")), enabled: form.get("enabled") === "on", inPool: form.get("inPool") === "on" };
    if (quota) payload.remainingQuota = Number(quota);
    try {
      await api("/accounts", { method: "POST", body: JSON.stringify(payload) });
      $("#account-dialog").close(); notify("Account added"); refresh(true);
    } catch (error) { $("#account-form-error").textContent = error.message; $("#account-form-error").hidden = false; }
  });
  $("#accounts-body").addEventListener("click", (event) => { const button = event.target.closest("[data-account-action]"); if (button) handleAccountAction(button); });
  $("#sticky-list").addEventListener("click", async (event) => { const button = event.target.closest("[data-sticky-key]"); if (!button) return; try { await api(`/sticky-sessions/${encodeURIComponent(button.dataset.stickyKey)}`, { method: "DELETE" }); notify("Sticky session cleared"); refresh(true); } catch (error) { notify(error.message, true); } });

  showPublicDashboard();
})();
