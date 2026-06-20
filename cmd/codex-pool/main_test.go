package main

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func testApp(t *testing.T, accounts []account) *app {
	t.Helper()
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "state"), 0o700); err != nil {
		t.Fatal(err)
	}
	hash, err := newPasswordHash("admin-password")
	if err != nil {
		t.Fatal(err)
	}
	return &app{
		config:  config{DefaultModel: "gpt-test", ModelAliases: map[string]string{"alias": "gpt-test"}, Accounts: accounts},
		state:   state{StickySessions: map[string]stickySession{}, Cooldowns: map[string][]cooldown{}},
		dataDir: dir, apiKeys: [][]byte{[]byte("client-key")}, adminUser: "admin", adminHash: []byte(hash),
		sessionKey: []byte("01234567890123456789012345678901"), codexBaseURL: "https://chatgpt.example.test/backend-api", jobs: map[string]*loginJob{}, client: &http.Client{Timeout: time.Second},
	}
}

func TestParseModel(t *testing.T) {
	cases := []struct{ input, model, tier string }{
		{"gpt-5.4(high)", "gpt-5.4", "high"},
		{"gpt-5.4(none)", "gpt-5.4", "none"},
		{"gpt-5.4(unknown)", "gpt-5.4(unknown)", ""},
		{"gpt-5.4", "gpt-5.4", ""},
	}
	for _, tc := range cases {
		model, tier := parseModel(tc.input)
		if model != tc.model || tier != tc.tier {
			t.Fatalf("parseModel(%q) = %q, %q", tc.input, model, tier)
		}
	}
}

func TestIsLoopbackAddress(t *testing.T) {
	for address, expected := range map[string]bool{
		"127.0.0.1:8318": true,
		"localhost:8318": true,
		"0.0.0.0:8318":   false,
		":8318":          false,
	} {
		if actual := isLoopbackAddress(address); actual != expected {
			t.Fatalf("isLoopbackAddress(%q) = %v, want %v", address, actual, expected)
		}
	}
}

func TestDashboardStatusSeparatesQuotaAndErrors(t *testing.T) {
	quota := 20
	a := testApp(t, []account{
		{ID: "ready", Enabled: true, InPool: true},
		{ID: "low", Enabled: true, InPool: true, RemainingQuota: &quota},
		{ID: "error", Enabled: true, InPool: true},
		{ID: "cooldown", Enabled: true, InPool: true},
		{ID: "disabled", Enabled: false, InPool: true},
		{ID: "missing", AuthType: "codex_device_auth", Enabled: true, InPool: true},
	})
	now := time.Now().UTC()
	a.state.Health = map[string]accountHealth{"error": {ConsecutiveFailure: 2, LastFailureReason: "upstream_transport_error"}}
	a.state.Cooldowns = map[string][]cooldown{"cooldown": {{ModelID: "gpt-test", NextRetryAt: now.Add(time.Minute), Reason: "rate_limited"}}}

	expected := map[string]string{"ready": "ready", "low": "low", "error": "error", "cooldown": "cooldown", "disabled": "disabled", "missing": "missing_auth"}
	for _, item := range a.config.Accounts {
		status, _ := a.accountStatusLocked(item, now)
		if status != expected[item.ID] {
			t.Fatalf("account %s status = %q, want %q", item.ID, status, expected[item.ID])
		}
	}

	status, reason := a.accountStatusLocked(a.config.Accounts[2], now)
	if status != "error" || reason != "upstream_transport_error" {
		t.Fatalf("error status did not retain the upstream reason: %q, %q", status, reason)
	}
}

func TestDeviceAuthPromptParsing(t *testing.T) {
	url, code := parseDeviceAuthPrompt("Open https://auth.openai.com/activate\nABCD-EFGH\n")
	if url != "https://auth.openai.com/activate" || code != "ABCD-EFGH" {
		t.Fatalf("parsed url/code = %q/%q", url, code)
	}
	fakeJWT := "ey" + "JhbGciOiJIUzI1NiJ9." + "eyJzdWIiOiIxMjM0NTY3ODkwIn0." + "signaturevalue"
	redacted := redactLoginOutput("Authorization: Bearer <secret-token>\naccess_token=<secret>\napi key: <secret>\nCookie: <secret>\n" + fakeJWT)
	for _, forbidden := range []string{"<secret-token>", "access_token=<secret>", "api key: <secret>", "Cookie: <secret>", "ey" + "JhbGci"} {
		if strings.Contains(redacted, forbidden) {
			t.Fatalf("login output was not redacted: %s", redacted)
		}
	}
	if strings.Count(redacted, "[REDACTED]") < 5 {
		t.Fatalf("login output was not redacted: %s", redacted)
	}
}

func TestCodexLoginEnvDoesNotInheritServiceSecrets(t *testing.T) {
	t.Setenv("CODEX_POOL_API_KEY", "client-secret")
	t.Setenv("CODEX_POOL_ADMIN_PASSWORD_HASH", "admin-secret")
	t.Setenv("CODEX_POOL_UPSTREAM_API_KEY", "upstream-secret")
	t.Setenv("HTTPS_PROXY", "http://proxy.example.test")
	env := codexLoginEnv("/data/accounts/acct/.codex")
	joined := strings.Join(env, "\n")
	for _, forbidden := range []string{"CODEX_POOL_API_KEY=", "CODEX_POOL_ADMIN_PASSWORD_HASH=", "CODEX_POOL_UPSTREAM_API_KEY=", "client-secret", "admin-secret", "upstream-secret"} {
		if strings.Contains(joined, forbidden) {
			t.Fatalf("login env inherited secret %q: %s", forbidden, joined)
		}
	}
	for _, required := range []string{"CODEX_HOME=/data/accounts/acct/.codex", "HOME=/data/accounts/acct", "HTTPS_PROXY=http://proxy.example.test"} {
		if !strings.Contains(joined, required) {
			t.Fatalf("login env missing %q: %s", required, joined)
		}
	}
}

func TestDeviceAuthLoginJobLifecycle(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-login", Label: "Login", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("acct-login")
	bin := t.TempDir()
	script := `#!/bin/sh
set -eu
mkdir -p "$CODEX_HOME"
env > "$CODEX_HOME/env.txt"
printf '%s\n' 'Open https://auth.openai.com/activate' 'ABCD-EFGH'
cat > "$CODEX_HOME/auth.json" <<EOF
{"auth_mode":"chatgpt","tokens":{"id_token":"` + fakeJWTClaims(map[string]any{"email": "user@example.test", "https://api.openai.com/auth": map[string]any{"chatgpt_account_id": "acct-chatgpt"}}) + `","access_token":"<access-token>","refresh_token":"<refresh-token>"}}
EOF
`
	if err := os.WriteFile(filepath.Join(bin, "codex"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv("CODEX_POOL_API_KEY", "client-secret")
	t.Setenv("CODEX_POOL_ADMIN_PASSWORD_HASH", "admin-secret")
	t.Setenv("CODEX_POOL_UPSTREAM_API_KEY", "upstream-secret")
	a.mu.Lock()
	job := a.startLoginJobLocked(a.config.Accounts[0])
	a.mu.Unlock()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		a.mu.RLock()
		current := *a.jobs[job.ID]
		a.mu.RUnlock()
		if current.Status == "completed" {
			if current.VerificationURL != "https://auth.openai.com/activate" || current.UserCode != "ABCD-EFGH" {
				t.Fatalf("job did not capture device prompt: %#v", current)
			}
			if a.config.Accounts[0].Email != "user@example.test" || a.config.Accounts[0].AccountID != "acct-chatgpt" {
				t.Fatalf("account metadata not updated: %#v", a.config.Accounts[0])
			}
			envData, err := os.ReadFile(filepath.Join(a.accountCodexHome("acct-login"), "env.txt"))
			if err != nil {
				t.Fatal(err)
			}
			envText := string(envData)
			for _, forbidden := range []string{"CODEX_POOL_API_KEY", "CODEX_POOL_ADMIN_PASSWORD_HASH", "CODEX_POOL_UPSTREAM_API_KEY", "client-secret", "admin-secret", "upstream-secret"} {
				if strings.Contains(envText, forbidden) {
					t.Fatalf("device login inherited service secret %q in env:\n%s", forbidden, envText)
				}
			}
			return
		}
		time.Sleep(25 * time.Millisecond)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	t.Fatalf("login job did not complete: %#v", a.jobs[job.ID])
}

func TestAdminDashboardAssets(t *testing.T) {
	a := testApp(t, nil)
	checks := map[string]string{
		"/admin":                "Pool status",
		"/admin/assets/app.css": ".badge.error",
		"/admin/assets/app.js":  "Low quota",
	}
	for path, expected := range checks {
		req := httptest.NewRequest(http.MethodGet, path, nil)
		recorder := httptest.NewRecorder()
		a.adminMux().ServeHTTP(recorder, req)
		if recorder.Code != http.StatusOK {
			t.Fatalf("GET %s returned %d", path, recorder.Code)
		}
		if !strings.Contains(recorder.Body.String(), expected) {
			t.Fatalf("GET %s did not include %q", path, expected)
		}
	}
}

func TestRootEndpointsAreHelpful(t *testing.T) {
	a := testApp(t, nil)

	publicRequest := httptest.NewRequest(http.MethodGet, "/", nil)
	publicRecorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusOK {
		t.Fatalf("public root returned %d", publicRecorder.Code)
	}
	if body := publicRecorder.Body.String(); !strings.Contains(body, "codex-pool") || !strings.Contains(body, "/v1") {
		t.Fatalf("public root did not describe service endpoints: %s", body)
	}

	adminRequest := httptest.NewRequest(http.MethodGet, "/", nil)
	adminRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(adminRecorder, adminRequest)
	if adminRecorder.Code != http.StatusFound {
		t.Fatalf("admin root returned %d", adminRecorder.Code)
	}
	if location := adminRecorder.Header().Get("Location"); location != "/admin" {
		t.Fatalf("admin root redirected to %q", location)
	}
}

func TestAdminCookieSecureBehindForwardedHTTPS(t *testing.T) {
	a := testApp(t, nil)
	request := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"admin-password"}`))
	request.Header.Set("X-Forwarded-Proto", "https")
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("login returned %d", recorder.Code)
	}
	for _, cookie := range recorder.Result().Cookies() {
		if strings.HasPrefix(cookie.Name, "codex_pool_") && !cookie.Secure {
			t.Fatalf("cookie %s was not Secure behind forwarded HTTPS", cookie.Name)
		}
	}
}

func TestAdminLoginRateLimit(t *testing.T) {
	a := testApp(t, nil)
	for i := 0; i < adminLoginMaxFailures; i++ {
		request := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"wrong"}`))
		request.RemoteAddr = "198.51.100.10:1234"
		recorder := httptest.NewRecorder()
		a.adminMux().ServeHTTP(recorder, request)
		if recorder.Code != http.StatusUnauthorized {
			t.Fatalf("failed login %d returned %d", i+1, recorder.Code)
		}
	}
	request := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"admin-password"}`))
	request.RemoteAddr = "198.51.100.10:1234"
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusTooManyRequests {
		t.Fatalf("rate-limited login returned %d", recorder.Code)
	}
}

func TestPublicDashboardRedactsAccountSecrets(t *testing.T) {
	quota := 12
	a := testApp(t, []account{{
		ID: "private-account-id", Label: "Public account", Email: "private@example.test", Enabled: true, InPool: true, RemainingQuota: &quota,
		UpstreamBaseURL: "https://upstream.example.test/v1", UpstreamAPIKey: "upstream-secret-value", AllowedModels: []string{"gpt-test"},
	}})

	publicRequest := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	publicRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", publicRecorder.Code)
	}
	publicBody := publicRecorder.Body.String()
	for _, forbidden := range []string{"private-account-id", "private@example.test", "upstream.example.test", "upstream-secret-value"} {
		if strings.Contains(publicBody, forbidden) {
			t.Fatalf("public dashboard exposed %q", forbidden)
		}
	}
	if !strings.Contains(publicBody, "Public account") || !strings.Contains(publicBody, `"status":"low"`) {
		t.Fatalf("public dashboard omitted expected status data: %s", publicBody)
	}

	managementRequest := httptest.NewRequest(http.MethodGet, "/admin/api/accounts", nil)
	managementRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(managementRecorder, managementRequest)
	if managementRecorder.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated management API returned %d", managementRecorder.Code)
	}
}

func TestManagementAPIsRequireAdminAndCSRF(t *testing.T) {
	a := testApp(t, []account{{ID: "acct", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	cases := []struct {
		method string
		path   string
		body   string
	}{
		{http.MethodGet, "/admin/api/accounts", ""},
		{http.MethodPost, "/admin/api/accounts", `{"id":"new"}`},
		{http.MethodGet, "/admin/api/accounts/health", ""},
		{http.MethodPost, "/admin/api/accounts/acct/enable", ""},
		{http.MethodPost, "/admin/api/accounts/acct/login", ""},
		{http.MethodPost, "/admin/api/accounts/quota/refresh-all", ""},
		{http.MethodDelete, "/admin/api/accounts/acct", ""},
		{http.MethodDelete, "/admin/api/sticky-sessions/key", ""},
	}
	for _, tc := range cases {
		req := httptest.NewRequest(tc.method, tc.path, strings.NewReader(tc.body))
		recorder := httptest.NewRecorder()
		a.adminMux().ServeHTTP(recorder, req)
		if recorder.Code != http.StatusUnauthorized {
			t.Fatalf("%s %s without admin returned %d", tc.method, tc.path, recorder.Code)
		}
	}

	login := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"admin-password"}`))
	loginRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(loginRecorder, login)
	req := httptest.NewRequest(http.MethodPost, "/admin/api/accounts/acct/enable", nil)
	for _, cookie := range loginRecorder.Result().Cookies() {
		req.AddCookie(cookie)
	}
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusForbidden {
		t.Fatalf("mutating management API without CSRF returned %d", recorder.Code)
	}
}

func TestAccountDeletePurgesCodexCredentials(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-delete", Label: "Delete Me", AuthType: "codex_device_auth", CodexHome: filepath.Join(t.TempDir(), "ignored"), Enabled: true, InPool: true}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("acct-delete")
	home := a.accountCodexHome("acct-delete")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(`{"auth_mode":"chatgpt"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	login := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"admin-password"}`))
	loginRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(loginRecorder, login)
	var response struct {
		CSRFToken string `json:"csrfToken"`
	}
	if err := json.Unmarshal(loginRecorder.Body.Bytes(), &response); err != nil {
		t.Fatal(err)
	}
	deleteRequest := httptest.NewRequest(http.MethodDelete, "/admin/api/accounts/acct-delete", nil)
	for _, cookie := range loginRecorder.Result().Cookies() {
		deleteRequest.AddCookie(cookie)
	}
	deleteRequest.Header.Set("X-CSRF-Token", response.CSRFToken)
	deleteRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(deleteRecorder, deleteRequest)
	if deleteRecorder.Code != http.StatusOK {
		t.Fatalf("delete returned %d: %s", deleteRecorder.Code, deleteRecorder.Body.String())
	}
	if _, err := os.Stat(a.accountRoot("acct-delete")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("credential directory still exists or stat failed unexpectedly: %v", err)
	}
}

func TestResponsesProxyTranslatesModelAndUsesStickySession(t *testing.T) {
	requests := 0
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests++
		if r.URL.Path != "/v1/responses" {
			t.Fatalf("unexpected upstream path %s", r.URL.Path)
		}
		if r.Header.Get("Authorization") != "Bearer upstream-key" {
			t.Fatal("upstream API key was not forwarded")
		}
		var body map[string]any
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Fatal(err)
		}
		if body["model"] != "gpt-test" {
			t.Fatalf("unexpected model: %#v", body["model"])
		}
		reasoning, _ := body["reasoning"].(map[string]any)
		if reasoning["effort"] != "high" {
			t.Fatalf("reasoning not translated: %#v", body["reasoning"])
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_test","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "one", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/v1", UpstreamAPIKey: "upstream-key", Priority: 100}})
	for range 2 {
		req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"alias(high)","input":"hello"}`))
		req.Header.Set("Authorization", "Bearer client-key")
		req.Header.Set("X-Codex-Pool-Session", "session-a")
		recorder := httptest.NewRecorder()
		a.publicMux().ServeHTTP(recorder, req)
		if recorder.Code != http.StatusOK {
			t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
		}
	}
	if requests != 2 {
		t.Fatalf("expected 2 upstream requests, got %d", requests)
	}
	if session := a.state.StickySessions["gpt-test:session-a"]; session.AccountID != "one" {
		t.Fatalf("sticky session was not saved: %#v", session)
	}
}

func TestResponsesProxyUsesCodexDeviceAuth(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/backend-api/responses" {
			t.Fatalf("unexpected upstream path %s", r.URL.Path)
		}
		if r.Header.Get("Authorization") != "Bearer <access-token>" {
			t.Fatalf("unexpected auth header %q", r.Header.Get("Authorization"))
		}
		if r.Header.Get("ChatGPT-Account-ID") != "acct-chatgpt" {
			t.Fatalf("missing ChatGPT account header")
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_test","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "codex-one", AuthType: "codex_device_auth", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/backend-api", Priority: 100}})
	home := filepath.Join(a.dataDir, "accounts", "codex-one", ".codex")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := `{"auth_mode":"chatgpt","tokens":{"access_token":"<access-token>","refresh_token":"<refresh-token>","account_id":"acct-chatgpt"}}`
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
	}
}

func TestResponsesProxyAddsCodexMetadataHeaders(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer <access-token>" {
			t.Fatalf("unexpected auth header %q", r.Header.Get("Authorization"))
		}
		if r.Header.Get("ChatGPT-Account-ID") != "acct-from-jwt" {
			t.Fatalf("missing account id from id_token: %q", r.Header.Get("ChatGPT-Account-ID"))
		}
		if r.Header.Get("X-OpenAI-Fedramp") != "true" {
			t.Fatalf("missing fedramp header")
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_test","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "codex-meta", AuthType: "codex_device_auth", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/backend-api", Priority: 100}})
	home := a.accountCodexHome("codex-meta")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	idToken := fakeJWTClaims(map[string]any{"https://api.openai.com/auth": map[string]any{"chatgpt_account_id": "acct-from-jwt", "chatgpt_account_is_fedramp": true}})
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"id_token":%q,"access_token":"<access-token>","refresh_token":"<refresh-token>"}}`, idToken)
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
	}
}

func TestResponsesProxyRefreshesExpiringCodexToken(t *testing.T) {
	newAccess := fakeJWT(time.Now().Add(time.Hour))
	refresh := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]string
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Fatal(err)
		}
		if body["client_id"] == "" || body["grant_type"] != "refresh_token" || body["refresh_token"] != "<old-refresh-token>" {
			t.Fatalf("unexpected refresh request: %#v", body)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = fmt.Fprintf(w, `{"access_token":%q,"refresh_token":"<new-refresh-token>"}`, newAccess)
	}))
	defer refresh.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", refresh.URL)

	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer "+newAccess {
			t.Fatalf("unexpected auth header %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_test","object":"response","output":[]}`))
	}))
	defer upstream.Close()

	a := testApp(t, []account{{ID: "codex-refresh", AuthType: "codex_device_auth", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/backend-api", Priority: 100}})
	home := filepath.Join(a.dataDir, "accounts", "codex-refresh", ".codex")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<old-refresh-token>","account_id":"acct-chatgpt"}}`, fakeJWT(time.Now().Add(-time.Minute)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
	}
	updated, err := os.ReadFile(filepath.Join(home, "auth.json"))
	if err != nil {
		t.Fatal(err)
	}
	var persisted codexAuthFile
	if err := json.Unmarshal(updated, &persisted); err != nil {
		t.Fatal(err)
	}
	if persisted.Tokens == nil || persisted.Tokens.RefreshToken != "<new-refresh-token>" {
		t.Fatalf("refreshed auth was not persisted: %s", updated)
	}
}

func TestCodexTokenRefreshBoundaries(t *testing.T) {
	a := testApp(t, []account{{ID: "codex-valid", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	home := a.accountCodexHome("codex-valid")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<refresh-token>"}}`, fakeJWT(time.Now().Add(time.Hour)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	called := false
	refresh := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		called = true
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer refresh.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", refresh.URL)
	if err := a.refreshCodexAuthIfNeeded(a.config.Accounts[0]); err != nil {
		t.Fatal(err)
	}
	if called {
		t.Fatal("valid access token triggered refresh")
	}

	expiredRefresh := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	}))
	defer expiredRefresh.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", expiredRefresh.URL)
	expired := testApp(t, []account{{ID: "codex-expired", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	expiredHome := expired.accountCodexHome("codex-expired")
	if err := os.MkdirAll(expiredHome, 0o700); err != nil {
		t.Fatal(err)
	}
	expiredJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<refresh-token>"}}`, fakeJWT(time.Now().Add(-time.Minute)))
	if err := os.WriteFile(filepath.Join(expiredHome, "auth.json"), []byte(expiredJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := expired.refreshCodexAuthIfNeeded(expired.config.Accounts[0]); err == nil {
		t.Fatal("expired access token with failed refresh returned nil error")
	}
}

func TestFailoverAfterRateLimit(t *testing.T) {
	first := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Retry-After", "1")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer first.Close()
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_ok","object":"response","output":[]}`))
	}))
	defer second.Close()
	a := testApp(t, []account{
		{ID: "first", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: first.URL, UpstreamAPIKey: "one"},
		{ID: "second", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: second.URL, UpstreamAPIKey: "two"},
	})
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "failover")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if session := a.state.StickySessions["gpt-test:failover"]; session.AccountID != "second" {
		t.Fatalf("expected sticky failover to second, got %#v", session)
	}
	if len(a.state.Cooldowns["first"]) != 1 {
		t.Fatalf("missing cooldown: %#v", a.state.Cooldowns)
	}
	if reason := a.state.Health["first"].LastFailureReason; reason != "rate_limited" {
		t.Fatalf("rate limit reason = %q, want rate_limited", reason)
	}
}

func fakeJWT(exp time.Time) string {
	return fakeJWTClaims(map[string]any{"exp": exp.Unix()})
}

func fakeJWTClaims(claims map[string]any) string {
	header := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"none"}`))
	payloadData, err := json.Marshal(claims)
	if err != nil {
		panic(err)
	}
	payload := base64.RawURLEncoding.EncodeToString(payloadData)
	return header + "." + payload + ".sig"
}

func TestAdminLoginAndCSRFMiddleware(t *testing.T) {
	a := testApp(t, nil)
	login := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"admin-password"}`))
	loginRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(loginRecorder, login)
	if loginRecorder.Code != http.StatusOK {
		t.Fatalf("login returned %d", loginRecorder.Code)
	}
	var response struct {
		CSRFToken string `json:"csrfToken"`
	}
	if err := json.Unmarshal(loginRecorder.Body.Bytes(), &response); err != nil {
		t.Fatal(err)
	}
	var sessionCookie, csrfCookie *http.Cookie
	for _, cookie := range loginRecorder.Result().Cookies() {
		if cookie.Name == "codex_pool_session" {
			sessionCookie = cookie
		}
		if cookie.Name == "codex_pool_csrf" {
			csrfCookie = cookie
		}
	}
	if sessionCookie == nil || csrfCookie == nil || response.CSRFToken == "" {
		t.Fatal("login did not establish expected cookies")
	}
	request := httptest.NewRequest(http.MethodPost, "/admin/api/accounts", strings.NewReader(`{"id":"codex-account","label":"Codex Account","codexHome":"/tmp/evil/.codex"}`))
	request.AddCookie(sessionCookie)
	request.AddCookie(csrfCookie)
	request.Header.Set("X-CSRF-Token", response.CSRFToken)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusCreated {
		t.Fatalf("admin account create returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if !strings.Contains(recorder.Body.String(), `"authType":"codex_device_auth"`) {
		t.Fatalf("admin account create did not default to Codex device auth: %s", recorder.Body.String())
	}
	if strings.Contains(recorder.Body.String(), "/tmp/evil") {
		t.Fatalf("admin account create accepted caller-supplied codexHome: %s", recorder.Body.String())
	}
}
