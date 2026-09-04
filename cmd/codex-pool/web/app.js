(() => {
  const themeStorageKey = "codexPoolAdminTheme";
  const themeMetaColors = Object.freeze({
    coastal: "#143740",
    forest: "#19483b",
    indigo: "#2e356f",
    ember: "#5a2d28",
    slate: "#263548",
  });
  const themeNames = new Set(Object.keys(themeMetaColors));
  const state = { csrfToken: sessionStorage.getItem("codexPoolCsrf") || "", data: null, refreshTimer: null, deviceAuthTimer: null, deviceAuthPollTimer: null, currentLoginJobId: "", currentPublicRepairRef: "", mode: "public" };
  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => document.querySelectorAll(selector);
  const loginView = $("#login-view");
  const dashboardView = $("#dashboard-view");
  // Exact token usage is committed only after the upstream response reports
  // usage, normally at response completion. Keep the original low-frequency
  // automatic refresh; page reloads and the management Refresh button bypass
  // caches and query current provider state immediately.
  const dashboardRefreshIntervalMs = 30 * 1000;

  // Theme is intentionally browser-local presentation state. It helps an
  // operator tell pool tabs apart without changing routing, authentication,
  // account state, or any server-side pool configuration. Invalid or
  // unavailable storage must fall back silently so the dashboard still loads.
  const applyTheme = (requestedTheme, persist = false) => {
    const theme = themeNames.has(requestedTheme) ? requestedTheme : "coastal";
    document.documentElement.dataset.theme = theme;
    const select = $("#theme-select");
    if (select) select.value = theme;
    const themeMeta = document.querySelector('meta[name="theme-color"]');
    if (themeMeta) themeMeta.content = themeMetaColors[theme];
    if (persist) {
      try {
        window.localStorage.setItem(themeStorageKey, theme);
      } catch (_) {}
    }
    return theme;
  };

  const initializeTheme = () => {
    let storedTheme = "";
    try {
      storedTheme = window.localStorage.getItem(themeStorageKey) || "";
    } catch (_) {}
    applyTheme(storedTheme);
  };

  initializeTheme();

  const escapeHTML = (value) => String(value ?? "").replace(/[&<>'"]/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" }[character]));
  const displayTime = (value) => {
    if (!value || value === "0001-01-01T00:00:00Z") return "No activity";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? "No activity" : date.toLocaleString();
  };
  const statusLabel = (status) => ({ ready: "Ready", low: "Low quota", protected: "Protected", exhausted: "Exhausted", cooldown: "Cooldown", error: "Error", disabled: "Disabled", standby: "Out of pool", duplicate: "Duplicate", missing_auth: "Login needed", authenticating: "Signing in" }[status] || "Unknown");
  const activeBadge = (active) => active ? '<span class="badge active">Active</span>' : "";
  const cacheHitRate = (input, cached) => {
    const total = Number(input) || 0;
    if (total <= 0) return null;
    return Math.max(0, Math.min(1, (Number(cached) || 0) / total));
  };
  // Per-account cells intentionally expose only cache reads. Automatic prompt
  // caching can work without meaningful cache-write telemetry, so restoring a
  // Write row would invite operators to treat upstream zeroes as cache health.
  // Request diagnostics remain available in the tooltip.
  const cacheHitMarkup = (obj, agentKind, resetId) => {
    const win = obj.cacheWindow || {};
    const agent = win[agentKind] || {};
    const input = Number(agent.inputTokens) || 0;
    const cached = Number(agent.cachedTokens) || 0;
    const reqs = Number(agent.requestCount) || 0;
    const cold = Number(agent.coldRequestCount) || 0;
    const usageObserved = Number(agent.usageObservedRequestCount) || 0;
    const readRate = cacheHitRate(input, cached);
    const resetBtn = resetId ? `<button class="cache-reset-btn" type="button" data-cache-reset="${escapeHTML(resetId)}" title="Recalculate this account's hit rate (reset its window)">↺</button>` : "";
    const readText = usageObserved || input > 0 ? `${formatTokens(cached)} / ${formatTokens(input)}` : "—";
    const readRatioMarkup = readRate === null
      ? '<span class="cache-empty">—</span>'
      : `<span class="cache-rate">${Math.round(readRate * 100)}%</span>`;
    const detail = `Cache read ratio ${readRate === null ? "unavailable" : `${Math.round(readRate * 100)}%`}. Token usage: ${readText}. Pool observed: ${reqs} requests, ${cold} cold eligible.`;
    return `<div class="cache-hit" title="${escapeHTML(detail)}">
      <span class="cache-token-row"><span class="cache-token-label">Read</span><span class="cache-token-value">${readText}</span><span class="cache-ratio-cell">${readRatioMarkup}${resetBtn}</span></span>
    </div>`;
  };
  const formatTokens = (value) => {
    const n = Number(value) || 0;
    if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
    if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
    if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
    return String(n);
  };
  const finiteMetric = (value) => {
    if (value === null || value === undefined || value === "") return null;
    const number = Number(value);
    return Number.isFinite(number) ? number : null;
  };
  const formatMetricRate = (value, suffix = "") => {
    const number = finiteMetric(value);
    if (number === null) return "—";
    if (Math.abs(number) >= 1000) return `${formatTokens(number)}${suffix}`;
    return `${number.toFixed(number >= 100 ? 0 : number >= 10 ? 1 : 2)}${suffix}`;
  };
  const formatDuration = (value) => {
    const milliseconds = finiteMetric(value);
    if (milliseconds === null) return "—";
    if (milliseconds < 1000) return `${Math.round(milliseconds)}ms`;
    if (milliseconds < 60000) return `${(milliseconds / 1000).toFixed(milliseconds < 10000 ? 1 : 0)}s`;
    return `${(milliseconds / 60000).toFixed(1)}m`;
  };

  const chartPercent = (value) => {
    const number = finiteMetric(value);
    return number === null ? "—" : `${(Math.max(0, Math.min(1, number)) * 100).toFixed(1)}%`;
  };

  const latestSeriesValue = (points, key) => {
    for (let index = points.length - 1; index >= 0; index--) {
      const value = finiteMetric(points[index]?.[key]);
      if (value !== null) return value;
    }
    return null;
  };

  const throughputChartWindows = [
    { hours: 1, label: "1 hour", shortLabel: "1h" },
    { hours: 6, label: "6 hours", shortLabel: "6h" },
    { hours: 12, label: "12 hours", shortLabel: "12h" },
    { hours: 24, label: "24 hours", shortLabel: "24h" },
    { hours: 48, label: "48 hours", shortLabel: "48h" },
  ];

  // Describe the actual observed span honestly instead of forcing it into the
  // coarse 1h..48h buckets. A short post-restart history should read as
  // "10m view", not be mislabeled "1h view" with the data crammed into a
  // sliver of the plot.
  const describeSpan = (spanMs) => {
    const minutes = Math.max(1, Math.round(spanMs / 60000));
    if (minutes < 90) return { hours: minutes / 60, label: `${minutes} min`, shortLabel: `${minutes}m` };
    const hours = Math.round(minutes / 60);
    return { hours, label: `${hours} hours`, shortLabel: `${hours}h` };
  };

  // The backend deliberately returns the complete 48-hour retention grid,
  // including leading empty buckets after a restart. Anchor the left edge to
  // the first real observation rather than a fixed now-minus-window cutoff:
  // otherwise a short history is stranded in the far-right sliver of an
  // otherwise empty plot and the line looks invisible. Every bucket inside the
  // span (including interior null buckets) is kept, so true 10-minute spacing
  // is preserved and sparse points are never redistributed evenly.
  const chartVisibleWindow = (points, series) => {
    const datedPoints = points.map((point) => ({
      point,
      timestamp: new Date(point?.at).getTime(),
    })).filter(({ timestamp }) => Number.isFinite(timestamp));
    if (!datedPoints.length) return { points: [], hasValues: false, ...throughputChartWindows[0] };

    const valuePoints = datedPoints.filter(({ point }) =>
      series.some((metric) => finiteMetric(point?.[metric.key]) !== null));
    if (!valuePoints.length) return { points: [], hasValues: false, ...throughputChartWindows[0] };

    const end = datedPoints[datedPoints.length - 1].timestamp;
    const firstValueTs = valuePoints[0].timestamp;
    const observedHours = Math.max(0, (end - firstValueTs) / (60 * 60 * 1000));
    const range = throughputChartWindows.find(({ hours }) => observedHours <= hours) || throughputChartWindows[throughputChartWindows.length - 1];
    const cutoff = Math.max(end - range.hours * 60 * 60 * 1000, firstValueTs);
    const cropped = datedPoints.filter(({ timestamp }) => timestamp >= cutoff).map(({ point }) => point);
    return {
      ...describeSpan(end - firstValueTs),
      hasValues: true,
      points: cropped,
    };
  };

  const chartTimeLabel = (value, windowHours) => {
    const at = new Date(value);
    if (Number.isNaN(at.getTime())) return "";
    if (windowHours <= 12) return at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    return at.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit" });
  };

  function throughputChartMarkup(points, config, visibleWindow) {
    const width = 820;
    const height = 248;
    const plot = { left: 63, right: 757, top: 22, bottom: 204 };
    const plotWidth = plot.right - plot.left;
    const plotHeight = plot.bottom - plot.top;
    const axisMetrics = (axis) => config.series.filter((metric) => metric.axis === axis);
    // Snap a dynamic axis maximum to a round step so that small fluctuations in
    // the trailing in-progress bucket do not rescale the whole plot on every
    // 30s refresh, which reads as the line jumping up and down.
    const niceCeil = (value) => {
      if (!(value > 0)) return 1;
      const magnitude = Math.pow(10, Math.floor(Math.log10(value)));
      const step = [1, 1.5, 2, 2.5, 3, 4, 5, 7.5, 10].find((candidate) => value <= candidate * magnitude);
      return (step || 10) * magnitude;
    };
    const axisMax = (axis) => {
      const fixed = axisMetrics(axis).map((metric) => metric.max).filter((value) => Number.isFinite(value));
      if (fixed.length) return Math.max(...fixed);
      let maximum = 0;
      axisMetrics(axis).forEach((metric) => {
        points.forEach((point) => {
          const value = finiteMetric(point?.[metric.key]);
          if (value !== null) maximum = Math.max(maximum, value);
        });
      });
      return maximum > 0 ? niceCeil(maximum * 1.08) : 1;
    };
    const leftMax = axisMax("left");
    const rightMax = axisMax("right");
    const xAt = (index) => plot.left + (points.length <= 1 ? 0 : (index / (points.length - 1)) * plotWidth);
    const yAt = (value, axis) => plot.bottom - (Math.max(0, value) / (axis === "right" ? rightMax : leftMax)) * plotHeight;
    const pathFor = (metric) => {
      let drawing = false;
      let path = "";
      points.forEach((point, index) => {
        const value = finiteMetric(point?.[metric.key]);
        if (value === null) {
          drawing = false;
          return;
        }
        const command = drawing ? "L" : "M";
        path += `${command}${xAt(index).toFixed(1)} ${yAt(value, metric.axis).toFixed(1)} `;
        drawing = true;
      });
      return path.trim();
    };
    const markersFor = (metric) => {
      const samples = [];
      points.forEach((point, index) => {
        const value = finiteMetric(point?.[metric.key]);
        if (value !== null) samples.push({ point, index, value });
      });
      // Preserve visible evidence for single/few-point histories without
      // turning a mature 48-hour series into hundreds of overlapping circles.
      const stride = Math.max(1, Math.ceil(samples.length / 72));
      return samples.filter((sample, index) => index % stride === 0 || index === samples.length - 1).map(({ point, index, value }) => {
        const at = new Date(point?.at);
        const title = `${metric.label}: ${metric.format(value)}${Number.isNaN(at.getTime()) ? "" : ` at ${at.toLocaleString()}`}`;
        return `<circle class="chart-series-point ${metric.colorClass}" cx="${xAt(index).toFixed(1)}" cy="${yAt(value, metric.axis).toFixed(1)}" r="2.6"><title>${escapeHTML(title)}</title></circle>`;
      }).join("");
    };
    const grid = [0, 0.25, 0.5, 0.75, 1].map((ratio) => {
      const y = plot.top + ratio * plotHeight;
      const leftValue = leftMax * (1 - ratio);
      const rightValue = rightMax * (1 - ratio);
      return `<line class="chart-grid-line" x1="${plot.left}" y1="${y}" x2="${plot.right}" y2="${y}"></line>
        <text class="chart-axis-label left" x="${plot.left - 9}" y="${y + 4}">${escapeHTML(config.leftFormat(leftValue))}</text>
        ${axisMetrics("right").length ? `<text class="chart-axis-label right" x="${plot.right + 9}" y="${y + 4}">${escapeHTML(config.rightFormat(rightValue))}</text>` : ""}`;
    }).join("");
    const labelIndexes = points.length ? [...new Set([0, Math.floor((points.length - 1) / 4), Math.floor((points.length - 1) / 2), Math.floor(((points.length - 1) * 3) / 4), points.length - 1])] : [];
    const xLabels = labelIndexes.map((index) => {
      const label = index === points.length - 1 ? "Now" : chartTimeLabel(points[index]?.at, visibleWindow.hours);
      return `<text class="chart-axis-label time" x="${xAt(index)}" y="${plot.bottom + 28}">${escapeHTML(label)}</text>`;
    }).join("");
    const paths = config.series.map((metric) => {
      const path = pathFor(metric);
      if (!path) return "";
      return `<path class="chart-series-line ${metric.colorClass}" d="${path}"></path>`;
    }).join("");
    const markers = config.series.map(markersFor).join("");
    const legend = config.series.map((metric) => {
      const value = latestSeriesValue(points, metric.key);
      return `<span class="chart-legend-item"><i class="${metric.colorClass}"></i><span>${escapeHTML(metric.label)}</span><strong>${escapeHTML(metric.format(value))}</strong></span>`;
    }).join("");
    return `<article class="throughput-chart-card">
      <div class="throughput-chart-heading"><div><div class="throughput-chart-title"><h3>${escapeHTML(config.title)}</h3><span class="chart-range-chip">${escapeHTML(visibleWindow.shortLabel)} view</span></div><p>${escapeHTML(config.description)}</p></div><div class="throughput-chart-legend">${legend}</div></div>
      <div class="throughput-chart-plot">
        <svg viewBox="0 0 ${width} ${height}" role="img" aria-label="${escapeHTML(config.title)} over the past ${escapeHTML(visibleWindow.label)}" preserveAspectRatio="none">
          ${grid}${xLabels}${paths}${markers}
        </svg>
      </div>
    </article>`;
  }

  function renderThroughput(throughput) {
    const points = Array.isArray(throughput?.series) ? throughput.series : [];
    const current = throughput?.current || {};
    const active = Number(throughput?.activeRequests) || 0;
    $("#throughput-active").textContent = `${active} active`;
    const currentItems = [
      ["Requests", formatMetricRate(current.requestsPerMinute), "req/min"],
      ["Output", formatMetricRate(current.outputTokensPerSecond), "tok/s"],
      ["Prompt cache", chartPercent(current.cacheHitRate), "hit rate"],
      ["Success", chartPercent(current.successRate), "completed"],
      ["p95 latency", formatDuration(current.p95LatencyMs), "provider"],
    ];
    $("#throughput-current").innerHTML = currentItems.map(([label, value, suffix]) => `<div class="throughput-current-stat"><span>${escapeHTML(label)}</span><strong>${escapeHTML(value)}</strong><small>${escapeHTML(suffix)}</small></div>`).join("");
    // This history is intentionally one focused correlation view. The current
    // strip and per-account rows retain operational rates, but adding separate
    // token-flow or latency charts here would recreate the dashboard clutter
    // that the operator explicitly removed.
    const chart = {
      title: "Output vs KV cache",
      description: "Compare delivered output throughput with cache reuse on the same 10-minute timeline.",
      leftFormat: (value) => formatMetricRate(value),
      rightFormat: chartPercent,
      series: [
        // Series colors live in app.css (.chart-series-output / .chart-series-prompt)
        // rather than inline styles: the admin CSP has no style-src
        // 'unsafe-inline', so an inline style="--series-color:…" is dropped and
        // the line/marker fall back to an unstyled black dot with no stroke.
        { key: "outputTokensPerSecond", label: "Output tok/s", axis: "left", colorClass: "chart-series-output", format: (value) => formatMetricRate(value) },
        { key: "cacheHitRate", label: "Prompt cache hit", axis: "right", colorClass: "chart-series-prompt", max: 1, format: chartPercent },
      ],
    };
    const visibleWindow = chartVisibleWindow(points, chart.series);
    $("#throughput-charts").innerHTML = visibleWindow.hasValues
      ? throughputChartMarkup(visibleWindow.points, chart, visibleWindow)
      : '<div class="throughput-chart-empty">No in-memory traffic history yet</div>';
  }

  function accountThroughputMarkup(throughput) {
    const win = throughput?.windows?.["5m"] || {};
    const requests = Number(win.requestCount) || 0;
    if (!requests) return '<span class="throughput-empty">No recent traffic</span>';
    return `<div class="account-throughput" title="Rolling 5 minute client-request throughput">
      <span><strong>${escapeHTML(formatMetricRate(win.requestsPerMinute))}</strong> req/min</span>
      <span><strong>${escapeHTML(formatMetricRate(win.outputTokensPerSecond))}</strong> output tok/s</span>
      <span><strong>${escapeHTML(formatDuration(win.p95LatencyMs))}</strong> p95 latency</span>
    </div>`;
  }

  function notify(message, error = false) {
    if (!error) return;
    const serviceStatus = $("#service-status");
    if (serviceStatus) serviceStatus.textContent = message;
  }

  function copyButtonValue(button) {
    const target = document.getElementById(button.dataset.copyTarget || "");
    return target?.textContent?.trim() || "";
  }

  function syncCopyButtons() {
    $$("[data-copy-target]").forEach((button) => {
      button.disabled = !copyButtonValue(button);
      button.textContent = button.dataset.copyLabel || "Copy";
      button.classList.remove("copied");
    });
  }

  async function writeClipboardText(text) {
    if (!text) throw new Error("Nothing to copy");
    if (navigator.clipboard?.writeText && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return;
    }
    // Some self-hosted admin pages run over plain HTTP where Clipboard API is
    // unavailable; keep this click-triggered fallback so onboarding still works.
    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.top = "-1000px";
    // While a modal <dialog> is open everything outside it is inert, so the
    // textarea must live inside the dialog or select()/copy silently fails.
    const host = document.querySelector("dialog[open]") || document.body;
    host.appendChild(textarea);
    textarea.select();
    try {
      if (!document.execCommand("copy")) throw new Error("Copy failed");
    } finally {
      textarea.remove();
    }
  }

  async function handleCopyButton(button) {
    const value = copyButtonValue(button);
    const label = button.dataset.copyLabel || "Copy";
    button.disabled = true;
    try {
      await writeClipboardText(value);
      button.textContent = "Copied";
      button.classList.add("copied");
      window.setTimeout(() => {
        button.textContent = label;
        button.classList.remove("copied");
        button.disabled = !copyButtonValue(button);
      }, 1200);
    } catch (error) {
      // #service-status sits behind the modal dialog, so surface the failure
      // on the button itself where the user can actually see it.
      button.textContent = "Copy failed";
      window.setTimeout(() => {
        button.textContent = label;
        button.disabled = !copyButtonValue(button);
      }, 1200);
      notify(error.message, true);
    }
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
    syncCopyButtons();
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
    state.currentPublicRepairRef = "";
    if (dialog.open) dialog.close();
    // Closing a public repair dialog only stops local polling. Public users do
    // not receive a cancellation endpoint; reopening Repair resumes the same
    // redacted job without exposing its internal ID.
    if (cancelJob && jobId && state.mode === "management") cancelDeviceAuthJob(jobId);
  }

  async function api(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (options.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    if (options.method && options.method !== "GET") headers.set("X-CSRF-Token", state.csrfToken);
    const response = await fetch(`/admin/api${path}`, { credentials: "same-origin", ...options, cache: "no-store", headers });
    if (response.status === 401) { showPublicDashboard(); throw new Error("Your session has expired"); }
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.error?.message || `Request failed (${response.status})`);
    return body;
  }

  async function publicApi(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (options.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    const response = await fetch(`/admin/api/public-dashboard${path}`, { credentials: "same-origin", ...options, cache: "no-store", headers });
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
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = null;
  }

  function scheduleDashboardRefresh(mode = state.mode) {
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = null;
    if (dashboardView.hidden || state.mode !== mode) return;
    state.refreshTimer = window.setTimeout(async () => {
      if (dashboardView.hidden || state.mode !== mode) return;
      if (mode === "management") {
        await refresh(true);
      } else {
        await refreshPublic(true);
      }
      scheduleDashboardRefresh(mode);
    }, dashboardRefreshIntervalMs);
  }

  function showDashboard() {
    state.mode = "management";
    loginView.hidden = true;
    dashboardView.hidden = false;
    $$(".management-only").forEach((element) => { element.hidden = false; });
    $$(".public-only").forEach((element) => { element.hidden = true; });
    $("#dashboard-eyebrow").textContent = "MANAGE";
    $("#dashboard-title").textContent = "Account pool";
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = null;
    refresh().finally(() => scheduleDashboardRefresh("management"));
  }

  async function showPublicDashboard() {
    // Product contract: the control page opens in public mode. Do not replace
    // this with an immediate login screen; password auth unlocks management mode
    // on the same page, while public status stays visible by default.
    state.mode = "public";
    $("#dashboard-eyebrow").textContent = "SERVICE STATUS";
    $("#dashboard-title").textContent = "Pool status";
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = null;
    const ok = await refreshPublic(true);
    if (!ok && state.mode === "public") {
      showLogin();
      return;
    }
    loginView.hidden = true;
    dashboardView.hidden = false;
    $$(".management-only").forEach((element) => { element.hidden = true; });
    $$(".public-only").forEach((element) => { element.hidden = false; });
    scheduleDashboardRefresh("public");
  }

  // Pool status cards. The aggregate cache hit rate lives in its own
  // #cache-window row, so it is intentionally not duplicated here. Every
  // account belongs to exactly one non-total card; do not merge Duplicate into
  // Out of pool, because duplicate credential copies may still be in the pool.
  function renderSummary(summary, publicMode = false) {
    const items = [
      ["Total accounts", summary.total || 0, ""],
      ["Ready", summary.ready || 0, ""],
      [publicMode ? "Limited" : "Low quota", summary.low || 0, "low"],
      ["Cooling down", summary.cooldown || 0, "cooldown"],
      ["Out of pool", summary.standby || 0, "missing_auth"],
      ["Duplicate", summary.duplicate || 0, "missing_auth"],
      ["Unavailable", summary.unavailable || 0, "error"],
    ];
    $("#summary-grid").innerHTML = items.map(([label, value, tone]) => `<div class="summary-item ${tone}"><div class="eyebrow">${label}</div><span class="summary-value">${value}</span></div>`).join("");
  }

  // The top "since reset" window deliberately exposes read effectiveness, not
  // cache-write telemetry. The backend still collects compatible write fields
  // for diagnostics, but automatic caching makes their zeroes operationally
  // ambiguous and unsuitable for the main dashboard.
  function renderCacheWindow(win) {
    const section = $("#cache-window");
    if (!section) return;
    if (!win) { section.hidden = true; return; }
    section.hidden = false;
    const input = Number(win.inputTokens) || 0;
    const cached = Number(win.cachedTokens) || 0;
    const reqs = Number(win.requestCount) || 0;
    const cold = Number(win.coldRequestCount) || 0;
    const cacheEligible = Number(win.cacheEligibleRequestCount) || 0;
    const usageObserved = Number(win.usageObservedRequestCount) || 0;
    const cacheHits = Number(win.cacheHitRequestCount) || 0;
    const rate = cacheHitRate(input, cached);
    $("#cache-window-hit").textContent = rate === null ? "No data" : `${(rate * 100).toFixed(1)}%`;
    $("#cache-window-request-hit").textContent = usageObserved ? `${((cacheHits / usageObserved) * 100).toFixed(1)}%` : "No data";
    $("#cache-window-cold-count").textContent = String(cold);
    $("#cache-window-cold").textContent = cacheEligible ? `${((cold / cacheEligible) * 100).toFixed(1)}%` : "No data";
    $("#cache-window-reqs").textContent = String(reqs);
    const agentCacheText = (agent) => {
      const agentInput = Number(agent?.inputTokens) || 0;
      const agentCached = Number(agent?.cachedTokens) || 0;
      const agentRate = cacheHitRate(agentInput, agentCached);
      return agentRate === null ? "No data" : `${(agentRate * 100).toFixed(1)}%`;
    };
    $("#cache-window-main").textContent = agentCacheText(win.main);
    $("#cache-window-subagent").textContent = agentCacheText(win.subagent);
    $("#cache-window-lineage-failover").textContent = String(Number(win.lineageFailoverCount) || 0);
    const resetAt = win.resetAt && win.resetAt !== "0001-01-01T00:00:00Z" ? win.resetAt : null;
    $("#cache-window-since").textContent = resetAt ? `since ${displayTime(resetAt)}` : "since service start (never reset)";
    $("#cache-window-reset").hidden = state.mode !== "management";
  }

  function renderSettings(serviceState) {
    const preserveSwitch = $("#preserve-pro-quota-switch");
    if (preserveSwitch) {
      preserveSwitch.checked = Boolean(serviceState?.preserveProQuota);
      preserveSwitch.disabled = false;
    }
    const routingPill = $("#routing-strategy-pill");
    if (routingPill) {
      const balanced = serviceState?.routingStrategy === "sticky_balanced";
      routingPill.textContent = balanced ? "Balanced sticky" : "Priority failover";
      routingPill.title = balanced
        ? "New sessions are distributed deterministically; active sessions stay on their assigned account."
        : "New sessions prefer the highest-priority account and use others only for failover.";
    }
  }

  function displayUnixTime(value) {
    if (!value) return "";
    const date = new Date(Number(value) * 1000);
    return Number.isNaN(date.getTime()) ? "" : date.toLocaleString();
  }

  function displayUnixDate(value) {
    if (!value) return "";
    const date = new Date(Number(value) * 1000);
    return Number.isNaN(date.getTime()) ? "" : date.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
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

  function quotaTone(remaining) {
    // Product signal: quota bars must become progressively warmer as remaining
    // capacity approaches zero. Do not collapse this to one decorative gradient;
    // low and critical windows need to read as increasingly urgent at a glance.
    if (remaining <= 0) return "empty";
    if (remaining <= 5) return "critical";
    if (remaining <= 20) return "low";
    if (remaining <= 40) return "watch";
    return "healthy";
  }

  function quotaTrackMarkup(value, label) {
    const remaining = quotaPercent(value);
    const tone = quotaTone(remaining);
    return `<progress class="quota-track ${tone}" value="${remaining}" max="100" aria-label="${escapeHTML(label)} quota remaining">${remaining}%</progress>`;
  }

  // Pool membership is a semantic account property, not a badge tone. In
  // particular, public Duplicate rows intentionally reuse the neutral standby
  // tone; only an explicit out-of-pool marker may mute their quota bars.
  const poolMembershipAttribute = (outOfPool) => outOfPool ? ' data-pool-membership="out"' : "";

  // Reported quota windows are AND-gated. Keep both consequences attached to
  // the relevant labels: the active zero-percent window gets a compact red
  // Exhausted marker, while a nonzero sibling names the window holding it. Both
  // notes use the alert color because the held sibling is unusable despite its
  // apparent headroom. Do not turn either signal into a separate status row;
  // that previously made multi-window accounts harder to scan.
  function quotaWindowMarkup(label, window, gate = {}) {
    if (!window || (!window.observed && !window.present)) return "";
    const durationLabel = window.label || label || "Window";
    if (!window.present) {
      return `<div class="quota-window quota-window-unreported"><div class="quota-line"><span>${escapeHTML(durationLabel)} quota</span><strong>Not reported</strong></div></div>`;
    }
    const rawValue = window.remainingPercent ?? window.percentage;
    const value = finiteMetric(rawValue);
    const reset = displayUnixTime(window.resetAt);
    const countdown = displayResetCountdown(window.resetAt);
    if (value === null) {
      return `<div class="quota-window quota-window-unreported"><div class="quota-line"><span>${escapeHTML(durationLabel)} quota</span><strong>Not reported</strong></div>${reset ? `<div class="quota-reset" title="${escapeHTML(reset)}"><span>Resets in</span><strong>${escapeHTML(countdown || "soon")}</strong></div>` : ""}</div>`;
    }
    const exhausted = Boolean(gate.exhausted);
    const blockers = Array.isArray(gate.blockedBy) ? gate.blockedBy.filter(Boolean) : [];
    const held = blockers.length > 0;
    const constraint = exhausted
      ? ' <em class="quota-window-exhausted">\u00b7 Exhausted</em>'
      : held
        ? ` <em class="quota-window-constraint">\u00b7 unavailable (${blockers.map((item) => `${escapeHTML(item)} exhausted`).join(" + ")})</em>`
        : "";
    return `<div class="quota-window${held ? " quota-window-held" : ""}"><div class="quota-line"><span>${escapeHTML(durationLabel)} quota${constraint}</span><strong>${quotaPercent(value)}% left</strong></div>${quotaTrackMarkup(value, durationLabel)}${reset ? `<div class="quota-reset" title="${escapeHTML(reset)}"><span>Resets in</span><strong>${escapeHTML(countdown || "soon")}</strong></div>` : ""}</div>`;
  }

  function quotaFreshnessMarkup(freshness, updatedAt) {
    const labels = {
      fresh: "Fresh",
      stale: "Stale",
      not_reported: "Not reported",
      refresh_unavailable: "Refresh unavailable",
    };
    const label = labels[freshness] || "Not reported";
    const updated = updatedAt && updatedAt !== "0001-01-01T00:00:00Z" ? displayTime(updatedAt) : "";
    const title = updated ? `Last successful refresh ${updated}` : "No successful quota refresh recorded";
    return `<div class="quota-fact quota-fact-telemetry" title="${escapeHTML(title)}"><span class="quota-fact-label">Telemetry:</span><strong class="quota-fact-value">${escapeHTML(label)}</strong>${updated ? `<span class="quota-fact-note">${escapeHTML(updated)}</span>` : ""}</div>`;
  }

  function quotaDetailsMarkup(content) {
    if (!content) return "";
    // Progressive disclosure is intentional here: the progress bars are the
    // operator's first-glance signal, while credits and telemetry are useful
    // diagnostic context. Keep every quota window open in the primary view,
    // but do not make secondary facts compete with those bars.
    return `<details class="quota-details"><summary>More details</summary><div class="quota-facts">${content}</div></details>`;
  }
  function quotaCreditsMarkup(credits) {
    if (!credits) return '<div class="quota-fact"><span class="quota-fact-label">Flexible credits:</span><strong class="quota-fact-value">Not reported</strong></div>';
    if (credits.unlimited) return '<div class="quota-fact"><span class="quota-fact-label">Flexible credits:</span><strong class="quota-fact-value">Unlimited</strong><span class="quota-fact-note">separate credits</span></div>';
    if (!credits.hasCredits) return '<div class="quota-fact"><span class="quota-fact-label">Flexible credits:</span><strong class="quota-fact-value">None reported</strong></div>';
    return `<div class="quota-fact"><span class="quota-fact-label">Flexible credits:</span><strong class="quota-fact-value">${escapeHTML(credits.balance || "Available")}</strong></div>`;
  }

  function resetCreditsMarkup(resetCredits) {
    if (resetCredits?.availableCount === null || resetCredits?.availableCount === undefined) return "";
    // OpenAI exposes each reset credit's expiry through a separate details
    // endpoint. Show only the nearest available expiry date: a full list or a
    // live countdown would add noise without changing the operator's decision.
    const expires = displayUnixDate(resetCredits.expiresAt);
    const exact = displayUnixTime(resetCredits.expiresAt);
    const note = expires
      ? `<span class="quota-fact-note"${exact ? ` title="${escapeHTML(`Expires ${exact}`)}"` : ""}>Expires ${escapeHTML(expires)}</span>`
      : "";
    return `<div class="quota-fact"><span class="quota-fact-label">Reset credits:</span><strong class="quota-fact-value">${escapeHTML(String(resetCredits.availableCount))}</strong>${note}</div>`;
  }

  function spendControlMarkup(limit) {
    if (!limit) return '<div class="quota-fact"><span class="quota-fact-label">Spend control:</span><strong class="quota-fact-value">Not reported</strong></div>';
    const amount = limit.limit ? `${limit.used || "0"} / ${limit.limit}` : "Reported";
    const remaining = limit.remainingPercent !== null && limit.remainingPercent !== undefined ? ` · ${quotaPercent(limit.remainingPercent)}% left` : "";
    return `<div class="quota-fact"><span class="quota-fact-label">Spend control:</span><strong class="quota-fact-value">${escapeHTML(limit.reached ? "Reached" : amount)}</strong>${remaining ? `<span class="quota-fact-note">${escapeHTML(remaining.slice(3))}</span>` : ""}</div>`;
  }

  function additionalLimitsMarkup(limits) {
    if (!Array.isArray(limits) || !limits.length) return "";
    const entries = limits.map((limit) => {
      const windows = Array.isArray(limit.windows) ? limit.windows : [limit.primary, limit.secondary].filter(Boolean);
      const detail = windows.map((window) => quotaWindowMarkup(limit.limitName || limit.limitId || "Additional", window)).join("");
      const name = limit.limitName || limit.limitId || "Reported limit";
      const reached = limit.exhausted ? '<span class="quota-additional-status">Reached</span>' : "";
      return `<div class="quota-additional-limit"><div class="quota-additional-heading"><strong>${escapeHTML(name)}</strong>${reached}</div><div class="quota-windows">${detail || '<div class="quota-detail">Window: Not reported</div>'}</div></div>`;
    }).join("");
    return `<div class="quota-additional-group"><div class="quota-section-label">Additional limits</div>${entries}</div>`;
  }
  function quotaMarkup(value, quota, quotaError, usageUpdatedAt, freshness, lastSuccessfulRefreshAt, metering) {
    const refreshError = quotaError ? `<span class="quota-error" title="${escapeHTML(quotaError.message)}">Quota update unavailable</span>` : "";
    if (metering === "api_metered" && !quota) {
      return '<div class="quota quota-detailed"><span class="quota-unknown">API-metered · ChatGPT quota not applicable</span></div>';
    }
    if (quota) {
      // Consume duration-bearing windows. Never overwrite one window with
      // another window's percentage/reset merely because the other is empty.
      const sourceWindows = Array.isArray(quota.windows) && quota.windows.length
        ? quota.windows
        : [quota.primary, quota.secondary].filter((window) => window && (window.observed || window.present));
      // A window is only exhausted when upstream actually reported it. An absent
      // window carries no evidence and must never be treated as a gate, or an
      // unreported bucket would mute every healthy window on the account.
      const windowRemaining = (entry) => entry && entry.present ? finiteMetric(entry.remainingPercent ?? entry.percentage) : null;
      // Match quotaExplicitlyBlocksRouting: zero-percent evidence stops gating
      // once its reported reset time has passed. Otherwise stale display data
      // would claim a sibling is unavailable after routing has already failed
      // open pending the next authoritative refresh.
      const windowBlocksRouting = (entry) => {
        if (windowRemaining(entry) !== 0) return false;
        const resetAt = finiteMetric(entry && entry.resetAt);
        return resetAt === null || Date.now() / 1000 < resetAt;
      };
      const gatingLabels = sourceWindows.filter(windowBlocksRouting).map((entry) => entry.label || "Window");
      const windows = sourceWindows.map((window) => {
        const blocking = windowBlocksRouting(window);
        return quotaWindowMarkup(window.label, window, {
          exhausted: blocking,
          blockedBy: blocking ? [] : gatingLabels,
        });
      }).filter(Boolean).join("");
      const reached = quota.rateLimitReachedType ? `<div class="quota-fact quota-fact-warning"><span class="quota-fact-label">Reached type:</span><strong class="quota-fact-value">${escapeHTML(quota.rateLimitReachedType.replaceAll("_", " "))}</strong></div>` : "";
      const resetCredits = resetCreditsMarkup(quota.resetCredits);
      // Keep every reported quota window visible: Pro/Spark and other windows
      // are distinct upstream limits, not duplicate renderings. Only the
      // supporting text is grouped so operators can scan bars first, then read
      // reset/credit/telemetry facts without losing any quota semantics.
      // Additional limits are model- or feature-scoped meters that rarely decide
      // anything at a glance, and rendering their nested window group beside the
      // subscription bars crowds the row that operators actually scan. They stay
      // individually rendered and unmerged, just behind the same disclosure as
      // the other secondary facts.
      const details = quotaDetailsMarkup(`${reached}${additionalLimitsMarkup(quota.additionalLimits)}${quotaCreditsMarkup(quota.credits)}${spendControlMarkup(quota.individualLimit)}${resetCredits}${quotaFreshnessMarkup(freshness, lastSuccessfulRefreshAt || usageUpdatedAt)}`);
      // Keep the decisive red Exhausted signal beside its window label. Do not
      // add a second account-level "Blocked" sentence below the bars; that
      // duplicates the signal and makes multi-window rows harder to scan.
      return `<div class="quota quota-detailed">${windows ? `<div class="quota-windows">${windows}</div>` : '<div class="quota-detail">Quota windows: Not reported</div>'}${details}${refreshError}</div>`;
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

  function actionButton(action, id, label, tone = "secondary", disabled = false) {
    return `<button class="button ${tone}" type="button" data-account-action="${action}" data-account-id="${escapeHTML(id)}"${disabled ? " disabled" : ""}>${label}</button>`;
  }

  // Cache headers stay intentionally source-neutral; only the actionable Read
  // ratio is presented inside each account cell.
  const cacheColumnHeader = (label) => `<span class="column-heading">${label}</span>`;
  const poolColumnHeader = (label) => `<span class="column-heading">${label}<small><span class="column-origin observed">POOL</span></small></span>`;

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

  function accountEntitlementMarkup(account) {
    const metadata = account.credentialMetadata || account;
    const family = metadata.planFamily || metadata.planType;
    const rawPlan = metadata.rawPlanType;
    const lines = [];
    if (family === "business") {
      const seat = metadata.seatType;
      if (seat === "standard" || seat === "premium") {
        lines.push(`Business ${seat === "premium" ? "Premium" : "Standard"}`);
        lines.push(`Seat type: ${seat === "premium" ? "Premium" : "Standard"}`);
      } else {
        lines.push("Seat type: Not reported");
      }
      lines.push(`Usage: ${metadata.planLimit ? `${metadata.planLimit} vs Standard` : "Not reported"}`);
      const noFiveHourCap = Array.isArray(metadata.quotaPolicy) && metadata.quotaPolicy.includes("no_five_hour_cap");
      lines.push(`5-hour policy: ${noFiveHourCap ? "No 5h limit" : "Not reported"}`);
    }
    if (rawPlan && rawPlan !== family) lines.push(`Raw plan: ${rawPlan}`);
    return lines.length ? `<span class="account-entitlement">${lines.map((line) => escapeHTML(line)).join("<br>")}</span>` : "";
  }

  function quotaProtectionMarkup(account, health) {
    const protection = health.quotaProtection;
    if (!protection?.supported) {
      return `<div class="quota-protection quota-protection-unavailable" title="Not available for API-key account"><span>Protection</span><strong>Unavailable</strong></div>`;
    }
    const protectionMessage = protection.message || (protection.blocked ? "Protected: threshold reached" : protection.enabled ? "Protection active" : "Protection disabled");
    const window = protection.effectiveWindow;
    const windowDetail = window?.present
      ? ` · ${window.label || "Window"} ${quotaPercent(window.remainingPercent ?? window.percentage)}% left`
      : "";
    const state = protection.blocked ? "Blocked" : protection.enabled ? "On" : "Off";
    return `<details class="quota-protection"><summary><span>Protection</span><strong>${escapeHTML(state)}</strong></summary>
      <div class="quota-protection-body">
        <div class="quota-detail"><strong>${escapeHTML(protectionMessage)}</strong>${escapeHTML(windowDetail)}</div>
        <div class="quota-protection-controls">
          <label><input type="checkbox" data-quota-protection-enabled="${escapeHTML(account.id)}"${protection.enabled ? " checked" : ""}> Protect quota</label>
          <label>Threshold <input class="quota-threshold-input" type="number" min="0" max="100" step="1" value="${quotaPercent(protection.threshold)}" data-quota-protection-threshold="${escapeHTML(account.id)}">%</label>
          <button class="button quiet" type="button" data-quota-protection-save="${escapeHTML(account.id)}">Save</button>
        </div>
        <div class="quota-detail">Duplicate slots share upstream capacity; this threshold applies only to this local slot.</div>
      </div>
    </details>`;
  }
  function ownerNoteInput(account, publicMode = false) {
    const ref = publicMode ? account.poolRef : account.id;
    if (!ref) return "";
    const attribute = publicMode ? "data-owner-note-ref" : "data-owner-note-account-id";
    return `<input class="account-owner-note" type="text" value="${escapeHTML(account.ownerNote || "")}" placeholder="Who owns this account?" maxlength="80" aria-label="Account owner note" ${attribute}="${escapeHTML(ref)}">`;
  }

  function renderAccounts(accounts, healthByID) {
    $("#accounts-head").innerHTML = `<tr><th>Account</th><th>Status</th><th>Quota</th><th>Routing</th><th class="cache-column">${cacheColumnHeader("Main cache")}</th><th class="cache-column">${cacheColumnHeader("Subagent cache")}</th><th class="routing-count-column">${poolColumnHeader("Affinity/Fallback")}</th><th class="throughput-column">Throughput (5m)</th><th>Last activity</th><th>Action</th></tr>`;
    $("#account-count").textContent = `${accounts.length} configured`;
    const body = $("#accounts-body");
    const activeLoginAccountId = accounts.find((account) => healthByID.get(account.id)?.loginJob)?.id || "";
    if (!accounts.length) {
      body.innerHTML = '<tr><td colspan="10"><div class="empty-state">No accounts configured</div></td></tr>';
      return;
    }
    body.innerHTML = accounts.map((account) => {
      const health = healthByID.get(account.id) || { status: "standby", statusReason: "No health data" };
      const activity = health.status === "error" ? health.lastFailureAt : health.lastSuccessAt;
      const route = account.inPool ? "In pool" : "Out of pool";
      const activeRoutes = Number(health.activeRouteCount) || 0;
      const routeCount = activeRoutes === 1 ? "1 active route" : `${activeRoutes} active routes`;
      const displayName = account.displayName || account.label || account.id || "Credential";
      const metadata = accountMetadataLine(account, false);
      // Re-authentication must reuse the existing local slot. Removing and
      // adding an account intentionally deletes its cache/affinity history, so
      // the repair action stays beside Remove and calls the existing slot login
      // endpoint instead of recreating credentials under a new id.
      const signingIn = health.status === "authenticating";
      const anotherAccountSigningIn = Boolean(activeLoginAccountId && activeLoginAccountId !== account.id);
      const repairTone = health.status === "missing_auth" || health.status === "error" ? "primary" : "secondary";
      const repair = account.authType === "codex_device_auth"
        ? actionButton("login", account.id, signingIn ? "Signing in…" : anotherAccountSigningIn ? "Sign-in busy" : "Repair sign-in", repairTone, signingIn || anotherAccountSigningIn)
        : "";
      const actions = repair + actionButton("delete", account.id, "Remove", "danger");
      const cacheWindow = health.cacheWindow || {};
      const affinityHits = Number(cacheWindow.parentAffinityHitCount) || 0;
      const affinityFallbacks = Number(cacheWindow.parentAffinityFallbackCount) || 0;
      return `<tr data-account-row="${escapeHTML(account.id)}"${poolMembershipAttribute(account.inPool === false)}>
        <td><div class="account-name">${escapeHTML(displayName)}${metadata ? `<span class="account-id">${escapeHTML(metadata)}</span>` : ""}${accountEntitlementMarkup(account)}${ownerNoteInput(account)}</div></td>
        <td><div class="status-stack"><span class="badge ${escapeHTML(health.status)}">${statusLabel(health.status)}</span>${activeBadge(health.active)}</div></td>
        <td>${quotaMarkup(health.remainingQuota ?? account.remainingQuota, health.quota, health.quotaError, health.usageUpdatedAt, health.quotaFreshness, health.lastSuccessfulRefreshAt, health.quotaMetering)}${quotaProtectionMarkup(account, health)}</td>
        <td><div class="route"><strong>${escapeHTML(authLabel(account.authType))}</strong><br>${escapeHTML(route)} · ${escapeHTML(routeCount)}</div></td>
        <td class="cache-column">${cacheHitMarkup(health, "main", account.id)}</td>
        <td class="cache-column">${cacheHitMarkup(health, "subagent")}</td>
        <td class="routing-count-column" title="${affinityHits} parent-affinity hits, ${affinityFallbacks} fallbacks">${affinityHits}/${affinityFallbacks}</td>
        <td class="throughput-column">${accountThroughputMarkup(health.throughput)}</td>
        <td><div class="activity">${displayTime(activity)}${health.consecutiveFailure ? `<br>${health.consecutiveFailure} consecutive failure${health.consecutiveFailure === 1 ? "" : "s"}` : ""}</div></td>
        <td><div class="row-actions">${actions}</div></td>
      </tr>`;
    }).join("");
  }

  function renderPublicAccounts(accounts) {
    $("#accounts-head").innerHTML = `<tr><th>Account</th><th>Status</th><th>Quota</th><th>Pool</th><th class="cache-column">${cacheColumnHeader("Main cache")}</th><th class="cache-column">${cacheColumnHeader("Subagent cache")}</th><th class="routing-count-column">${poolColumnHeader("Affinity/Fallback")}</th><th>Action</th></tr>`;
    $("#account-count").textContent = `${accounts.length} visible`;
    const body = $("#accounts-body");
    if (!accounts.length) {
      body.innerHTML = '<tr><td colspan="8"><div class="empty-state">No accounts available</div></td></tr>';
      return;
    }
    body.innerHTML = accounts.map((account) => {
      const displayName = account.displayName || account.label || "Credential";
      const metadata = account.detail || "";
      const tone = account.statusTone || account.status || "standby";
      const label = account.statusLabel || statusLabel(account.status);
      const quota = account.quotaUnavailable ? '<span class="quota-unknown">Quota unavailable</span>' : quotaMarkup(account.remainingQuota, account.quota, null, null, account.quotaFreshness, account.lastSuccessfulRefreshAt, account.quotaMetering);
      const action = account.poolRef && account.poolAction
        ? `<button class="button ${account.poolAction === "repair" ? "primary" : account.poolAction === "pool-remove" ? "warn" : "secondary"}" type="button" data-public-pool-action="${escapeHTML(account.poolAction)}" data-pool-ref="${escapeHTML(account.poolRef)}">${escapeHTML(account.poolActionLabel || "Update")}</button>`
        : "";
      const cacheWindow = account.cacheWindow || {};
      const affinityHits = Number(cacheWindow.parentAffinityHitCount) || 0;
      const affinityFallbacks = Number(cacheWindow.parentAffinityFallbackCount) || 0;
      return `<tr${poolMembershipAttribute(account.outOfPool === true)}>
      <td><div class="account-name">${escapeHTML(displayName)}${metadata ? `<span class="account-id">${escapeHTML(metadata)}</span>` : ""}${ownerNoteInput(account, true)}</div></td>
      <td><div class="status-stack"><span class="badge ${escapeHTML(tone)}">${escapeHTML(label)}</span>${activeBadge(account.active)}</div></td>
      <td>${quota}</td>
      <td><div class="route"><strong>${escapeHTML(account.poolLabel || "Unavailable")}</strong></div></td>
      <td class="cache-column">${cacheHitMarkup(account, "main")}</td>
      <td class="cache-column">${cacheHitMarkup(account, "subagent")}</td>
      <td class="routing-count-column" title="${affinityHits} parent-affinity hits, ${affinityFallbacks} fallbacks">${affinityHits}/${affinityFallbacks}</td>
      <td><div class="row-actions">${action}</div></td>
    </tr>`;
    }).join("");
  }

  const routingOutcomeLabel = (value) => ({
    sticky_reuse: "Sticky reuse",
    new_route_assignment: "New route",
    parent_affinity: "Parent affinity",
    parent_affinity_fallback: "Affinity fallback",
    quota_failover: "Quota failover",
    rate_limit_failover: "Rate-limit failover",
    stream_capacity_failover: "Stream capacity failover",
    auth_failover: "Auth failover",
    transport_failover: "Transport failover",
    repeated_5xx_failover: "Repeated 5xx failover",
    upstream_response_failed: "Upstream response failed",
  }[value] || "Routing result");

  function renderRoutingCacheEvents(events) {
    const rows = Array.isArray(events) ? events.slice(0, 50) : [];
    $("#routing-cache-count").textContent = rows.length === 1 ? "1 recent request" : `${rows.length} recent requests`;
    const body = $("#routing-cache-body");
    if (!rows.length) {
      body.innerHTML = '<tr><td colspan="6"><div class="empty-state">No recent cache observations</div></td></tr>';
      return;
    }
    body.innerHTML = rows.map((event) => {
      const read = event.usageObserved && event.cacheReadRate !== null && event.cacheReadRate !== undefined
        ? `${(Number(event.cacheReadRate) * 100).toFixed(1)}% · ${formatTokens(event.cachedTokens)}`
        : "—";
      const failover = event.failoverFromAccountLabel ? `<span class="event-secondary">from ${escapeHTML(event.failoverFromAccountLabel)}</span>` : "";
      const identifiers = [
        event.requestIdHash ? `request ${event.requestIdHash}` : "",
        event.stickyKeyHash ? `route ${event.stickyKeyHash}` : "",
        event.promptCacheKeyHash ? `cache ${event.promptCacheKeyHash}` : "",
      ].filter(Boolean).join(" · ");
      const cacheTone = event.cacheHit ? "hit" : event.coldCacheEligible ? "cold" : "";
      const terminal = event.terminalEvent
        ? `<span class="event-secondary">${escapeHTML(`${event.terminalEvent}${event.terminalFailureClass ? ` · ${event.terminalFailureClass}` : ""}${event.terminalErrorCode ? ` · ${event.terminalErrorCode}` : ""}`)}</span>`
        : "";
      return `<tr title="${escapeHTML(identifiers)}">
        <td>${escapeHTML(new Date(event.timestamp).toLocaleTimeString())}</td>
        <td><span class="agent-kind ${escapeHTML(event.agentKind)}">${escapeHTML(event.agentKind || "main")}</span></td>
        <td>${escapeHTML(event.accountLabel || "Credential")}</td>
        <td><span class="routing-outcome">${escapeHTML(routingOutcomeLabel(event.routingOutcome))}</span>${failover}<span class="event-secondary">${escapeHTML(event.routingSource || "fallback")}</span>${terminal}</td>
        <td class="event-cache ${cacheTone}">${escapeHTML(read)}</td>
        <td>${event.usageObserved ? escapeHTML(formatTokens(event.inputTokens)) : "—"}</td>
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
      const activeLoginJob = healthResponse.accounts.find((item) => item.loginJob)?.loginJob || null;
      state.data = { serviceState, accounts: accountsResponse.accounts, healthByID, sessions: sessionsResponse.sessions };
      renderSettings(serviceState);
      renderSummary(serviceState.summary || {});
      renderThroughput(serviceState.throughput);
      renderCacheWindow(serviceState.promptCacheWindow);
      renderAccounts(state.data.accounts, healthByID);
      renderRoutingCacheEvents(serviceState.routingCacheEvents);
      renderSticky(state.data.sessions, state.data.accounts);
      const addAccountButton = $("#add-account-button");
      if (addAccountButton) addAccountButton.disabled = Boolean(activeLoginJob);
      // Device-auth jobs are intentionally single-flight. Recover a live job
      // after page reload so its verification URL/code is not orphaned.
      if (activeLoginJob && !state.currentLoginJobId) {
        state.currentLoginJobId = activeLoginJob.jobId;
        if (activeLoginJob.status === "waiting_for_user" && (activeLoginJob.verificationUrl || activeLoginJob.userCode)) {
          showDeviceAuth(activeLoginJob);
        }
        watchLoginJob(activeLoginJob.jobId);
      }
      $("#service-status").textContent = serviceState.routingStrategy === "sticky_balanced" ? "Service online · balanced" : "Service online · failover";
    } catch (error) {
      if (!silent) notify(error.message, true);
      $("#service-status").textContent = "Service unavailable";
    }
  }

  async function refreshPublic(silent = false) {
    try {
      const response = await fetch("/admin/api/public-dashboard", { credentials: "same-origin", cache: "no-store" });
      const body = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(body.error?.message || `Request failed (${response.status})`);
      const accounts = body.dashboard.accounts || [];
      renderSummary(body.dashboard.summary || {}, true);
      renderThroughput(body.dashboard.throughput);
      renderCacheWindow(body.dashboard.promptCacheWindow);
      renderPublicAccounts(accounts);
      // Clear prior authenticated request details before rendering public mode.
      // Public status exposes pool-wide rolling throughput, but never per-request
      // or per-account routing/traffic correlation.
      renderRoutingCacheEvents([]);
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
      } else if (action === "login") {
        if (!window.confirm("Repair sign-in for this slot? Sign in with the same upstream account to preserve its cache and affinity history.")) return;
        await startDeviceAuth(id);
        await refresh(true);
        return;
      } else {
        await api(`/accounts/${encodeURIComponent(id)}/${action}`, { method: "POST" });
      }
      notify("Account updated");
      refresh(true);
    } catch (error) { notify(error.message, true); }
  }

  async function updateQuotaProtection(accountId) {
    const enabledInput = document.querySelector(`[data-quota-protection-enabled="${CSS.escape(accountId)}"]`);
    const thresholdInput = document.querySelector(`[data-quota-protection-threshold="${CSS.escape(accountId)}"]`);
    const threshold = Number(thresholdInput?.value);
    if (!Number.isInteger(threshold) || threshold < 0 || threshold > 100) {
      notify("Quota protection threshold must be an integer from 0 to 100", true);
      return;
    }
    try {
      await api(`/accounts/${encodeURIComponent(accountId)}/quota-protection/set`, {
        method: "POST",
        body: JSON.stringify({ enabled: Boolean(enabledInput?.checked), threshold }),
      });
      notify("Quota protection updated");
      refresh(true);
    } catch (error) {
      notify(error.message, true);
    }
  }

  async function handlePublicPoolAction(button) {
    const action = button.dataset.publicPoolAction;
    const ref = button.dataset.poolRef;
    if (action === "repair") {
      if (!window.confirm("Repair this sign-in? Continue with the same upstream account so its cache and affinity history can be preserved.")) return;
      button.disabled = true;
      try {
        await startPublicDeviceAuth(ref);
        await refreshPublic(true);
      } catch (error) {
        button.disabled = false;
        notify(error.message, true);
      }
      return;
    }
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

  async function startPublicDeviceAuth(ref) {
    const response = await publicApi(`/accounts/${encodeURIComponent(ref)}/repair`, { method: "POST" });
    state.currentPublicRepairRef = ref;
    if (response.job.status === "waiting_for_user" && (response.job.verificationUrl || response.job.userCode)) {
      showDeviceAuth(response.job);
    }
    watchPublicLoginJob(ref);
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
          const completionMessage = job.historyReset
            ? "Different or unverifiable account detected; cache and affinity history reset"
            : job.reauthentication
              ? "Sign-in repaired; cache and affinity history preserved"
              : "Sign-in completed";
          await refresh(true);
          const serviceStatus = $("#service-status");
          if (serviceStatus) serviceStatus.textContent = completionMessage;
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

  async function watchPublicLoginJob(ref) {
    let attempts = 0;
    const tick = async () => {
      if (state.currentPublicRepairRef !== ref) return;
      attempts += 1;
      try {
        const response = await publicApi(`/accounts/${encodeURIComponent(ref)}/repair`);
        const job = response.job;
        if (state.currentPublicRepairRef !== ref) return;
        if (job.status === "waiting_for_user" && (job.verificationUrl || job.userCode)) {
          showDeviceAuth(job);
        }
        if (job.status === "completed") {
          closeDeviceAuth(false);
          await refreshPublic(true);
          notify("Sign-in repaired");
          return;
        }
        if (job.status === "failed" || job.status === "cancelled") {
          closeDeviceAuth(false);
          await refreshPublic(true);
          if (job.status === "failed") notify("Sign-in repair failed; verify that you used the same account", true);
          return;
        }
        if (attempts < 180) state.deviceAuthPollTimer = window.setTimeout(tick, 5000);
      } catch (error) {
        notify(error.message, true);
      }
    };
    tick();
  }

  async function updateOwnerNote(input) {
    const ownerNote = input.value;
    const publicRef = input.dataset.ownerNoteRef;
    const accountId = input.dataset.ownerNoteAccountId;
    input.disabled = true;
    try {
      if (publicRef) {
        await publicApi(`/accounts/${encodeURIComponent(publicRef)}/note`, { method: "POST", body: JSON.stringify({ ownerNote }) });
        await refreshPublic(true);
      } else if (accountId) {
        await api(`/accounts/${encodeURIComponent(accountId)}/note`, { method: "POST", body: JSON.stringify({ ownerNote }) });
        await refresh(true);
      }
      notify("Account note saved");
    } catch (error) {
      input.disabled = false;
      notify(error.message, true);
    }
  }

  $("#login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    try {
      const response = await fetch("/admin/api/login", { method: "POST", cache: "no-store", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password: form.get("password") }) });
      const body = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(body.error?.message || "Unable to sign in");
      state.csrfToken = body.csrfToken;
      sessionStorage.setItem("codexPoolCsrf", state.csrfToken);
      $("#password").value = "";
      showDashboard();
    } catch (error) { $("#login-error").textContent = error.message; $("#login-error").hidden = false; }
  });

  $("#refresh-button").addEventListener("click", () => refresh());
  $("#theme-select").addEventListener("change", (event) => applyTheme(event.currentTarget.value, true));
  $("#cache-window-reset").addEventListener("click", async () => {
    try { await api("/cache/reset", { method: "POST" }); notify("Cache window reset"); refresh(true); }
    catch (error) { notify(error.message, true); }
  });
  $("#sign-in-button").addEventListener("click", () => showLogin());
  $("#logout-button").addEventListener("click", async () => { try { await api("/logout", { method: "POST" }); } catch (_) {} sessionStorage.removeItem("codexPoolCsrf"); state.csrfToken = ""; showPublicDashboard(); });
  $("#add-account-button").addEventListener("click", createAccountAndStartLogin);
  $("#preserve-pro-quota-switch").addEventListener("change", updatePreserveProQuota);
  $("#close-device-auth-button").addEventListener("click", () => closeDeviceAuth(true));
  $("#device-auth-dialog").addEventListener("click", (event) => {
    const button = event.target.closest("[data-copy-target]");
    if (button) handleCopyButton(button);
  });
  $("#accounts-body").addEventListener("click", (event) => {
    const publicButton = event.target.closest("[data-public-pool-action]");
    if (publicButton) {
      handlePublicPoolAction(publicButton);
      return;
    }
    const resetButton = event.target.closest("[data-cache-reset]");
    if (resetButton) {
      const id = resetButton.dataset.cacheReset;
      (async () => {
        try { await api(`/accounts/${encodeURIComponent(id)}/cache/reset`, { method: "POST" }); notify("Account cache window reset"); refresh(true); }
        catch (error) { notify(error.message, true); }
      })();
      return;
    }
    const protectionButton = event.target.closest("[data-quota-protection-save]");
    if (protectionButton) {
      updateQuotaProtection(protectionButton.dataset.quotaProtectionSave);
      return;
    }
    const button = event.target.closest("[data-account-action]");
    if (button) handleAccountAction(button);
  });
  $("#accounts-body").addEventListener("change", (event) => {
    const input = event.target.closest("[data-owner-note-ref], [data-owner-note-account-id]");
    if (input) updateOwnerNote(input);
  });
  $("#sticky-list").addEventListener("click", async (event) => { const button = event.target.closest("[data-sticky-key]"); if (!button) return; try { await api(`/sticky-sessions/${encodeURIComponent(button.dataset.stickyKey)}`, { method: "DELETE" }); notify("Route cleared"); refresh(true); } catch (error) { notify(error.message, true); } });

  showPublicDashboard();
})();
