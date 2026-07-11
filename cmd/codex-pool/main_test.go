package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
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
		state:   state{StickySessions: map[string]stickySession{}, ResponseBindings: map[string]responseBinding{}, Cooldowns: map[string][]cooldown{}, Health: map[string]accountHealth{}, Quotas: map[string]quotaSnapshot{}, PromptCache: map[string]promptCacheStat{}},
		dataDir: dir, apiKeys: [][]byte{[]byte("client-key")}, adminUser: "admin", adminHash: []byte(hash),
		sessionKey: []byte("01234567890123456789012345678901"), sessionAffinityTTL: sessionAffinityTTLDefault, promptCacheKeyMode: "auto", publicDashboard: true, codexBaseURL: "https://chatgpt.example.test/backend-api", codexGatewayMode: "direct", jobs: map[string]*loginJob{}, loginCancels: map[string]context.CancelFunc{}, authLocks: map[string]*sync.Mutex{}, client: &http.Client{Timeout: time.Second}, streamClient: &http.Client{Timeout: time.Second},
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

func TestDefaultModelCatalogIncludesThinkingTiers(t *testing.T) {
	a := testApp(t, nil)
	a.config.DefaultModel = "gpt-5.5(xhigh)"
	models := strings.Join(a.modelsLocked(), "\n")
	for _, expected := range []string{"gpt-5.5", "gpt-5.5(low)", "gpt-5.5(medium)", "gpt-5.5(high)", "gpt-5.5(xhigh)"} {
		if !strings.Contains(models, expected) {
			t.Fatalf("modelsLocked missing %q in:\n%s", expected, models)
		}
	}
}

func TestCodexModelCatalogAdvertisesReasoningCapabilities(t *testing.T) {
	a := testApp(t, nil)
	a.config.DefaultModel = "gpt-5.5(xhigh)"

	req := httptest.NewRequest(http.MethodGet, "/v1/models?client_version=0.142.4", nil)
	req.Header.Set("Authorization", "Bearer client-key")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("Codex model catalog returned %d: %s", recorder.Code, recorder.Body.String())
	}
	// Codex 0.141.0 deserializes these fields without serde defaults. Checking
	// raw keys keeps this test from passing when a required nullable or false
	// field is accidentally omitted and would restart the model refresh loop.
	var rawPayload struct {
		Models []map[string]json.RawMessage `json:"models"`
	}
	if err := json.Unmarshal(recorder.Body.Bytes(), &rawPayload); err != nil {
		t.Fatal(err)
	}
	if len(rawPayload.Models) == 0 {
		t.Fatal("Codex model catalog is empty")
	}
	var rawModel map[string]json.RawMessage
	for _, candidate := range rawPayload.Models {
		var slug string
		if err := json.Unmarshal(candidate["slug"], &slug); err == nil && slug == "gpt-5.5" {
			rawModel = candidate
			break
		}
	}
	if rawModel == nil {
		t.Fatal("Codex model catalog omitted gpt-5.5")
	}
	for _, field := range []string{
		"slug", "display_name", "description", "supported_reasoning_levels",
		"shell_type", "visibility", "supported_in_api", "priority",
		"availability_nux", "upgrade", "base_instructions",
		"supports_reasoning_summaries", "supports_reasoning_summary_parameter",
		"support_verbosity", "default_verbosity", "apply_patch_tool_type",
		"truncation_policy", "supports_parallel_tool_calls",
		"experimental_supported_tools",
	} {
		if _, ok := rawModel[field]; !ok {
			t.Fatalf("Codex 0.141 model metadata missing required field %q: %s", field, recorder.Body.String())
		}
	}

	var payload struct {
		Models []codexModelInfo `json:"models"`
	}
	if err := json.Unmarshal(recorder.Body.Bytes(), &payload); err != nil {
		t.Fatal(err)
	}
	var model *codexModelInfo
	for index := range payload.Models {
		if payload.Models[index].ID == "gpt-5.5" {
			model = &payload.Models[index]
			break
		}
	}
	if model == nil || model.Slug != "gpt-5.5" || model.ContextWindow != 272000 || model.DefaultReasoningLevel != "xhigh" {
		t.Fatalf("unexpected Codex model metadata: %#v", model)
	}
	if model.Priority != 0 {
		t.Fatalf("default model priority = %d, want 0", model.Priority)
	}
	if model.ShellType != "shell_command" || model.Visibility != "list" || !model.SupportedInAPI {
		t.Fatalf("invalid Codex model routing metadata: %#v", model)
	}
	if model.BaseInstructions == "" || model.ApplyPatchToolType != "freeform" || !model.SupportsParallelToolCalls {
		t.Fatalf("invalid Codex agent capability metadata: %#v", model)
	}
	if !model.SupportsReasoningSummaries || !model.SupportsReasoningSummaryParameter {
		t.Fatalf("reasoning summary compatibility fields missing: %#v", model)
	}
	if model.TruncationPolicy.Mode != "tokens" || model.TruncationPolicy.Limit != 10000 {
		t.Fatalf("unexpected truncation policy: %#v", model.TruncationPolicy)
	}
	if len(model.InputModalities) != 2 || model.InputModalities[0] != "text" || model.InputModalities[1] != "image" {
		t.Fatalf("unexpected input modalities: %#v", model.InputModalities)
	}
	if len(model.SupportedReasoningLevels) != 4 {
		t.Fatalf("supported reasoning levels = %#v", model.SupportedReasoningLevels)
	}
	for index, effort := range []string{"low", "medium", "high", "xhigh"} {
		if model.SupportedReasoningLevels[index].Effort != effort || model.SupportedReasoningLevels[index].Description == "" {
			t.Fatalf("reasoning level %d = %#v", index, model.SupportedReasoningLevels[index])
		}
	}
	if strings.Contains(recorder.Body.String(), "gpt-5.5(xhigh)") {
		t.Fatal("Codex model catalog exposed legacy reasoning suffix")
	}

	genericReq := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	genericReq.Header.Set("Authorization", "Bearer client-key")
	genericRecorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(genericRecorder, genericReq)
	if !strings.Contains(genericRecorder.Body.String(), "gpt-5.5(xhigh)") {
		t.Fatal("generic model list lost legacy reasoning alias compatibility")
	}
}

func TestCodexModelCatalogNormalizesUnsupportedDefaultReasoningTier(t *testing.T) {
	a := testApp(t, nil)
	a.config.DefaultModel = "gpt-5.5(max)"
	models := a.codexModelCatalogLocked(a.modelsLocked())
	for _, model := range models {
		if model.Slug == "gpt-5.5" {
			if model.DefaultReasoningLevel != "medium" {
				t.Fatalf("unsupported catalog default = %q, want medium", model.DefaultReasoningLevel)
			}
			return
		}
	}
	t.Fatal("Codex model catalog omitted gpt-5.5")
}

func TestDefaultModelEnvOverridesPersistedConfig(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "state"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := writeJSONAtomic(filepath.Join(dir, "config.json"), config{DefaultModel: "gpt-5.4", ModelAliases: map[string]string{}}); err != nil {
		t.Fatal(err)
	}
	t.Setenv("CODEX_POOL_DEFAULT_MODEL", "gpt-5.5(xhigh)")
	a := &app{dataDir: dir}
	if err := a.load(); err != nil {
		t.Fatal(err)
	}
	if a.config.DefaultModel != "gpt-5.5(xhigh)" {
		t.Fatalf("default model = %q", a.config.DefaultModel)
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
		{ID: "quota-error", Enabled: true, InPool: true},
		{ID: "stale-failure", Enabled: true, InPool: true},
		{ID: "cooldown", Enabled: true, InPool: true},
		{ID: "disabled", Enabled: false, InPool: true},
		{ID: "staged", AuthType: "codex_device_auth", Enabled: false, InPool: false, PendingPoolActivation: true},
		{ID: "standby", AuthType: "codex_device_auth", Enabled: true, InPool: false},
		{ID: "missing", AuthType: "codex_device_auth", Enabled: true, InPool: true},
	})
	now := time.Now().UTC()
	a.state.Health = map[string]accountHealth{"stale-failure": {ConsecutiveFailure: 2, LastFailureReason: "upstream_transport_error", LastFailureAt: now.Add(-time.Hour)}}
	a.state.Quotas = map[string]quotaSnapshot{"quota-error": {AccountID: "quota-error", QuotaError: &quotaErrorInfo{Code: "invalid_token", Message: "credential unavailable", Timestamp: now.Add(-time.Minute)}}}
	a.state.Cooldowns = map[string][]cooldown{"cooldown": {{ModelID: "gpt-test", NextRetryAt: now.Add(time.Minute), Reason: "rate_limited"}}}

	expected := map[string]string{"ready": "ready", "low": "low", "quota-error": "error", "stale-failure": "ready", "cooldown": "cooldown", "disabled": "disabled", "staged": "disabled", "standby": "standby", "missing": "missing_auth"}
	for _, item := range a.config.Accounts {
		status, _ := a.accountStatusLocked(item, now)
		if status != expected[item.ID] {
			t.Fatalf("account %s status = %q, want %q", item.ID, status, expected[item.ID])
		}
	}

	status, reason := a.accountStatusLocked(a.config.Accounts[2], now)
	if status != "error" || !strings.Contains(reason, "invalid_token") {
		t.Fatalf("quota error status did not retain the sanitized code: %q, %q", status, reason)
	}
	status, reason = a.accountStatusLocked(a.config.Accounts[3], now)
	if status != "ready" || reason != "Ready" {
		t.Fatalf("stale failure status = %q/%q, want ready", status, reason)
	}
}

func TestMissingCodexAuthClassifiesWithoutRetry(t *testing.T) {
	a := testApp(t, []account{{ID: "missing-fast", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	start := time.Now()
	_, err := a.codexAuth(a.config.Accounts[0])
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("missing auth unexpectedly succeeded")
	}
	if !errors.Is(err, errAccountAuthFailed) || !errors.Is(err, errCodexAuthMissing) {
		t.Fatalf("missing auth error = %v, want account auth + missing sentinels", err)
	}
	if elapsed > 200*time.Millisecond {
		t.Fatalf("missing auth classification took %s, want no retry delay", elapsed)
	}
}

func TestProxyClientCancelDoesNotMarkTransportFailure(t *testing.T) {
	reached := make(chan struct{}, 1)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case reached <- struct{}{}:
		default:
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer upstream.Close()
	a := testApp(t, []account{{
		ID:              "provider",
		AuthType:        "provider_api_key",
		Enabled:         true,
		InPool:          true,
		UpstreamBaseURL: upstream.URL,
		UpstreamAPIKey:  "provider-key",
	}})
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	request := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"ping"}`)).WithContext(ctx)
	recorder := httptest.NewRecorder()
	a.handleResponses(recorder, request)
	select {
	case <-reached:
		t.Fatal("cancelled request reached upstream")
	default:
	}
	if a.state.FailureCount != 0 {
		t.Fatalf("cancelled request incremented failure count: %d", a.state.FailureCount)
	}
	if health := a.state.Health["provider"]; health.LastFailureReason != "" || health.ConsecutiveFailure != 0 {
		t.Fatalf("cancelled request marked account unhealthy: %#v", health)
	}
	if cooldowns := a.state.Cooldowns["provider"]; len(cooldowns) != 0 {
		t.Fatalf("cancelled request added cooldowns: %#v", cooldowns)
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

func TestCodexAuthUsesAccountNameInsteadOfProfileName(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-auth", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	home := a.accountCodexHome("acct-auth")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	idToken := fakeJWTClaims(map[string]any{
		"email": "user@example.test",
		"name":  "Yi Fan Liou",
		"https://api.openai.com/profile": map[string]any{
			"email":        "user@example.test",
			"display_name": "Yi Fan Liou",
		},
		"https://api.openai.com/auth": map[string]any{
			"chatgpt_account_id":   "acct-chatgpt",
			"chatgpt_account_name": "markliou",
			"chatgpt_plan_type":    "team",
		},
	})
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"id_token":%q,"access_token":"<access-token>","refresh_token":"<refresh-token>"}}`, idToken)
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	auth, err := a.codexAuth(a.config.Accounts[0])
	if err != nil {
		t.Fatal(err)
	}
	if auth.OrganizationName != "markliou" {
		t.Fatalf("organization name = %q, want markliou", auth.OrganizationName)
	}
}

func TestCodexAuthRetriesDuringAuthFileRewrite(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-auth", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	home := a.accountCodexHome("acct-auth")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(home, "auth.json")
	if err := os.WriteFile(path, []byte(`{"auth_mode":"chatgpt","tokens":`), 0o600); err != nil {
		t.Fatal(err)
	}
	done := make(chan struct{})
	go func() {
		defer close(done)
		time.Sleep(25 * time.Millisecond)
		authJSON := `{"auth_mode":"chatgpt","tokens":{"access_token":"<access-token>","refresh_token":"<refresh-token>","account_id":"acct-chatgpt"}}`
		_ = os.WriteFile(path, []byte(authJSON), 0o600)
	}()

	// Codex CLI rewrites auth.json outside Pool's locks. A request that lands on
	// the partial-write window must wait briefly and use the completed file
	// instead of marking every device-auth slot missing and returning 503.
	auth, err := a.codexAuth(a.config.Accounts[0])
	<-done
	if err != nil {
		t.Fatal(err)
	}
	if auth.AccessToken != "<access-token>" || auth.AccountID != "acct-chatgpt" {
		t.Fatalf("auth after rewrite = %#v", auth)
	}
}

func TestCliproxyCodexAuthUsesJWTAccountNameOverStoredProfileName(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-auth", AuthType: "codex_device_auth", Enabled: true, InPool: true, OrganizationName: "Yi Fan Liou", PlanType: "team"}})
	a.codexGatewayMode = "cliproxy"
	idToken := fakeJWTClaims(map[string]any{
		"email": "user@example.test",
		"name":  "Yi Fan Liou",
		"https://api.openai.com/profile": map[string]any{
			"email":        "user@example.test",
			"display_name": "Yi Fan Liou",
		},
		"https://api.openai.com/auth": map[string]any{
			"chatgpt_account_id":   "acct-chatgpt",
			"chatgpt_account_name": "markliou",
			"chatgpt_plan_type":    "team",
		},
	})
	path := a.cliproxyAuthPath("acct-auth")
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	record := cliproxyCodexAuthFile{
		Type:             "codex",
		Email:            "user@example.test",
		IDToken:          idToken,
		AccessToken:      "<access-token>",
		AccountID:        "acct-chatgpt",
		OrganizationName: "Yi Fan Liou",
		Prefix:           cliproxyAccountPrefix("acct-auth"),
		PlanType:         "team",
	}
	if err := writeJSONAtomic(path, record); err != nil {
		t.Fatal(err)
	}
	auth, err := a.cliproxyCodexAuth(a.config.Accounts[0])
	if err != nil {
		t.Fatal(err)
	}
	if auth.OrganizationName != "markliou" {
		t.Fatalf("organization name = %q, want markliou", auth.OrganizationName)
	}
}

func TestCliproxyCodexAuthIgnoresStoredOrganizationNameWithoutJWTOrganization(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-auth", AuthType: "codex_device_auth", Enabled: true, InPool: true, OrganizationName: "Yi-Fan Liou", PlanType: "team"}})
	a.codexGatewayMode = "cliproxy"
	idToken := fakeJWTClaims(map[string]any{
		"email": "user@example.test",
		"name":  "Yi-Fan Liou",
		"https://api.openai.com/auth": map[string]any{
			"chatgpt_account_id": "acct-chatgpt",
			"chatgpt_plan_type":  "team",
		},
	})
	path := a.cliproxyAuthPath("acct-auth")
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	record := cliproxyCodexAuthFile{
		Type:             "codex",
		Email:            "user@example.test",
		IDToken:          idToken,
		AccessToken:      "<access-token>",
		AccountID:        "acct-chatgpt",
		OrganizationName: "Yi-Fan Liou",
		Prefix:           cliproxyAccountPrefix("acct-auth"),
		PlanType:         "team",
	}
	if err := writeJSONAtomic(path, record); err != nil {
		t.Fatal(err)
	}
	auth, err := a.cliproxyCodexAuth(a.config.Accounts[0])
	if err != nil {
		t.Fatal(err)
	}
	if auth.OrganizationName != "" {
		t.Fatalf("stored sidecar organization name was trusted: %q", auth.OrganizationName)
	}
}

func TestDeviceAuthLoginJobLifecycle(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-login", Label: "Login", AuthType: "codex_device_auth", Enabled: false, InPool: false, PendingPoolActivation: true}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("acct-login")
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/backend-api/wham/usage" {
			t.Fatalf("unexpected quota path %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":60},"secondary_window":null}}`))
	}))
	defer usage.Close()
	a.codexBaseURL = usage.URL + "/backend-api"
	bin := t.TempDir()
	script := `#!/bin/sh
set -eu
mkdir -p "$CODEX_HOME"
env > "$CODEX_HOME/env.txt"
printf '%s\n' 'Open https://auth.openai.com/activate' 'ABCD-EFGH'
cat > "$CODEX_HOME/auth.json" <<EOF
{"auth_mode":"chatgpt","tokens":{"id_token":"` + fakeJWTClaims(map[string]any{"email": "user@example.test", "https://api.openai.com/auth": map[string]any{"chatgpt_account_id": "acct-chatgpt", "chatgpt_account_name": "Acme Workspace"}}) + `","access_token":"<access-token>","refresh_token":"<refresh-token>"}}
EOF
`
	if err := os.WriteFile(filepath.Join(bin, "codex"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv("CODEX_POOL_API_KEY", "client-secret")
	t.Setenv("CODEX_POOL_ADMIN_PASSWORD_HASH", "admin-secret")
	t.Setenv("CODEX_POOL_UPSTREAM_API_KEY", "upstream-secret")
	a.state.Health["acct-login"] = accountHealth{LastFailureAt: time.Now().Add(-time.Minute), LastFailureReason: "old auth error", ConsecutiveFailure: 2}
	a.state.Cooldowns["acct-login"] = []cooldown{{ModelID: "gpt-test", NextRetryAt: time.Now().Add(time.Hour), Reason: "old cooldown"}}
	a.state.Quotas["acct-login"] = quotaSnapshot{AccountID: "acct-login", QuotaError: &quotaErrorInfo{Code: "old_error", Message: "old quota error", Timestamp: time.Now().Add(-time.Minute)}}
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
			if current.CodeExpiresAt.IsZero() || time.Until(current.CodeExpiresAt) < 14*time.Minute {
				t.Fatalf("job did not set a 15 minute code expiry: %#v", current)
			}
			if a.config.Accounts[0].Email != "user@example.test" || a.config.Accounts[0].AccountID != "acct-chatgpt" || a.config.Accounts[0].OrganizationName != "" {
				t.Fatalf("account metadata not updated: %#v", a.config.Accounts[0])
			}
			if !a.config.Accounts[0].Enabled || !a.config.Accounts[0].InPool || a.config.Accounts[0].PendingPoolActivation {
				t.Fatalf("login did not activate staged account: %#v", a.config.Accounts[0])
			}
			quota := a.state.Quotas["acct-login"].Quota
			if quota == nil || quota.Hourly.Percentage != 90 {
				t.Fatalf("quota was not refreshed before job completion: %#v", quota)
			}
			if health := a.state.Health["acct-login"]; health.ConsecutiveFailure != 0 || health.LastFailureReason != "" {
				t.Fatalf("login did not clear stale health error: %#v", health)
			}
			if cooldowns := a.state.Cooldowns["acct-login"]; len(cooldowns) != 0 {
				t.Fatalf("login did not clear stale cooldowns: %#v", cooldowns)
			}
			if errInfo := a.state.Quotas["acct-login"].QuotaError; errInfo != nil {
				t.Fatalf("login did not clear stale quota error: %#v", errInfo)
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

func TestDeviceAuthLoginDoesNotStartDuplicateJob(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-login", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	existing := &loginJob{ID: "job-existing", AccountID: "acct-login", Status: "waiting_for_user"}
	a.mu.Lock()
	a.jobs[existing.ID] = existing
	job := a.startLoginJobLocked(a.config.Accounts[0])
	a.mu.Unlock()
	if job.ID != existing.ID {
		t.Fatalf("duplicate login created job %q, want existing %q", job.ID, existing.ID)
	}
}

func TestDeviceAuthLoginJobCancel(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-cancel", Label: "Cancel", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("acct-cancel")
	bin := t.TempDir()
	script := `#!/bin/sh
set -eu
mkdir -p "$CODEX_HOME"
printf '%s\n' 'Open https://auth.openai.com/activate' 'ABCD-EFGH'
while true; do sleep 1; done
`
	if err := os.WriteFile(filepath.Join(bin, "codex"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	a.mu.Lock()
	job := a.startLoginJobLocked(a.config.Accounts[0])
	a.mu.Unlock()

	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		a.mu.RLock()
		current := *a.jobs[job.ID]
		a.mu.RUnlock()
		if current.Status == "waiting_for_user" {
			break
		}
		time.Sleep(25 * time.Millisecond)
	}

	cookies, csrf := adminSession(t, a)
	cancelRequest := httptest.NewRequest(http.MethodPost, "/admin/api/jobs/"+job.ID+"/cancel", nil)
	for _, cookie := range cookies {
		cancelRequest.AddCookie(cookie)
	}
	cancelRequest.Header.Set("X-CSRF-Token", csrf)
	cancelRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(cancelRecorder, cancelRequest)
	if cancelRecorder.Code != http.StatusOK {
		t.Fatalf("cancel returned %d: %s", cancelRecorder.Code, cancelRecorder.Body.String())
	}

	deadline = time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		a.mu.RLock()
		current := *a.jobs[job.ID]
		a.mu.RUnlock()
		if current.Status == "cancelled" {
			if current.Error != "" {
				t.Fatalf("cancelled job retained error: %#v", current)
			}
			return
		}
		time.Sleep(25 * time.Millisecond)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	t.Fatalf("login job was not cancelled: %#v", a.jobs[job.ID])
}

func TestAdminDashboardAssets(t *testing.T) {
	oldVersion, oldCommit, oldBuiltAt := buildVersion, buildCommit, buildBuiltAt
	buildVersion, buildCommit, buildBuiltAt = "vtest", "abcdef123456", "2026-06-25T02:30:00Z"
	defer func() {
		buildVersion, buildCommit, buildBuiltAt = oldVersion, oldCommit, oldBuiltAt
	}()

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
	request := httptest.NewRequest(http.MethodGet, "/admin", nil)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Header().Get("X-Codex-Pool-Version") != "vtest+abcdef12" {
		t.Fatalf("admin version header = %q", recorder.Header().Get("X-Codex-Pool-Version"))
	}
	if strings.Contains(recorder.Body.String(), adminVersionPlaceholder) || !strings.Contains(recorder.Body.String(), "vtest+abcdef12") {
		t.Fatal("admin page did not inject build metadata version")
	}
	for _, forbidden := range []string{"Admin sign in", "Username", "Account ID", "Label", "Models", "Subscription tier", "Email hint", "Quota hint", "account-form", "toast"} {
		if strings.Contains(recorder.Body.String(), forbidden) {
			t.Fatalf("admin page still includes %q", forbidden)
		}
	}
	for _, expected := range []string{"ACCESS", "Continue", "Password", "Add account", "Use Pro last", "SERVICE STATUS", "Active routes", "device-auth-url", "device-auth-code", "device-auth-countdown", "Copy verification link", "Copy verification code"} {
		if !strings.Contains(recorder.Body.String(), expected) {
			t.Fatalf("admin page does not include low-key label %q", expected)
		}
	}
	for _, forbidden := range []string{"ADMIN", "Sign in", "Console", "Preserve Pro quota", "PUBLIC STATUS", "DEVICE AUTH", "Passphrase", "Sticky sessions"} {
		if strings.Contains(recorder.Body.String(), forbidden) {
			t.Fatalf("admin page still exposes internal label %q", forbidden)
		}
	}
	jsRequest := httptest.NewRequest(http.MethodGet, "/admin/assets/app.js", nil)
	jsRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(jsRecorder, jsRequest)
	if strings.Contains(jsRecorder.Body.String(), "Open the verification URL and enter the code") {
		t.Fatal("admin JS still renders device-auth instruction text instead of URL/code only")
	}
	if strings.Contains(jsRecorder.Body.String(), "account-form") {
		t.Fatal("admin JS still depends on add-account form inputs")
	}
	if !strings.Contains(jsRecorder.Body.String(), "maskRouteKey") || !strings.Contains(jsRecorder.Body.String(), "Session ") {
		t.Fatal("admin JS must keep masked sticky route keys visible in Active routes")
	}
	for _, forbidden := range []string{"Device login completed", "Device login failed", "Sticky session cleared", "Refresh failed:", "No quota window", "Detected after login", "Need login"} {
		if strings.Contains(jsRecorder.Body.String(), forbidden) {
			t.Fatalf("admin JS still exposes internal label %q", forbidden)
		}
	}
	if strings.Contains(jsRecorder.Body.String(), `actionButton("login"`) || strings.Contains(jsRecorder.Body.String(), `data-account-action="login"`) {
		t.Fatal("admin JS still renders a per-account login action")
	}
	if !strings.Contains(jsRecorder.Body.String(), "codeExpiresAt") {
		t.Fatal("admin JS does not render the device-auth expiry countdown")
	}
	if !strings.Contains(jsRecorder.Body.String(), "5 * 60 * 1000") {
		t.Fatal("admin JS does not use the 5 minute dashboard refresh interval")
	}
	if !strings.Contains(jsRecorder.Body.String(), "preserveProQuota") {
		t.Fatal("admin JS does not render the Pro quota preservation switch")
	}
	for _, expected := range []string{"displayResetCountdown", "quotaTrackMarkup", "Resets in", "% left", "<progress", "value=\"${remaining}\""} {
		if !strings.Contains(jsRecorder.Body.String(), expected) {
			t.Fatalf("admin JS does not render clear quota state %q", expected)
		}
	}
	if strings.Contains(jsRecorder.Body.String(), "toast") {
		t.Fatal("admin JS still renders bottom-right toast notifications")
	}
	cssRequest := httptest.NewRequest(http.MethodGet, "/admin/assets/app.css", nil)
	cssRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(cssRecorder, cssRequest)
	if !strings.Contains(cssRecorder.Body.String(), "::-webkit-progress-value") || !strings.Contains(cssRecorder.Body.String(), "background: #0f172a") || !strings.Contains(cssRecorder.Body.String(), "border: 1px solid #334155") {
		t.Fatal("admin CSS does not provide a visible unfilled quota track")
	}
}

func TestRootEndpointsAreHelpful(t *testing.T) {
	a := testApp(t, nil)

	publicRequest := httptest.NewRequest(http.MethodGet, "/", nil)
	publicRecorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusNotFound {
		t.Fatalf("public root without key returned %d", publicRecorder.Code)
	}

	publicRequest = httptest.NewRequest(http.MethodGet, "/", nil)
	publicRequest.Header.Set("Authorization", "Bearer client-key")
	publicRecorder = httptest.NewRecorder()
	a.publicMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusNotFound {
		t.Fatalf("public root with key returned %d", publicRecorder.Code)
	}
	if body := publicRecorder.Body.String(); strings.Contains(body, "codex-pool") || strings.Contains(body, "admin") || strings.Contains(body, "/v1") {
		t.Fatalf("public root exposed service details: %s", body)
	}

	healthRequest := httptest.NewRequest(http.MethodGet, "/healthz", nil)
	healthRecorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(healthRecorder, healthRequest)
	if healthRecorder.Code != http.StatusUnauthorized {
		t.Fatalf("public health without key returned %d", healthRecorder.Code)
	}

	healthRequest = httptest.NewRequest(http.MethodGet, "/healthz", nil)
	healthRequest.Header.Set("Authorization", "Bearer client-key")
	healthRecorder = httptest.NewRecorder()
	a.publicMux().ServeHTTP(healthRecorder, healthRequest)
	if healthRecorder.Code != http.StatusOK {
		t.Fatalf("public health with key returned %d", healthRecorder.Code)
	}

	adminRequest := httptest.NewRequest(http.MethodGet, "/", nil)
	adminRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(adminRecorder, adminRequest)
	if adminRecorder.Code != http.StatusOK {
		t.Fatalf("admin root returned %d", adminRecorder.Code)
	}
	if body := adminRecorder.Body.String(); !strings.Contains(body, `id="dashboard-view"`) || !strings.Contains(body, `id="login-view"`) {
		t.Fatalf("admin root did not serve dashboard shell: %s", body)
	}
}

func TestPublicDashboardEnabledByDefaultFromEnv(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "state"), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("CODEX_POOL_DATA_DIR", dir)
	t.Setenv("CODEX_POOL_API_KEY", "client-key")
	hash, err := newPasswordHash("admin-password")
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("CODEX_POOL_ADMIN_PASSWORD_HASH", hash)
	a, err := newAppFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if !a.publicDashboard {
		t.Fatal("public dashboard should be enabled by default")
	}
	request := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("default public dashboard returned %d", recorder.Code)
	}
}

func TestPublicDashboardCanBeDisabledFromEnv(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "state"), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("CODEX_POOL_DATA_DIR", dir)
	t.Setenv("CODEX_POOL_API_KEY", "client-key")
	t.Setenv("CODEX_POOL_PUBLIC_DASHBOARD", "false")
	hash, err := newPasswordHash("admin-password")
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("CODEX_POOL_ADMIN_PASSWORD_HASH", hash)
	a, err := newAppFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if a.publicDashboard {
		t.Fatal("public dashboard should be disabled by explicit env")
	}
	request := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusNotFound {
		t.Fatalf("disabled public dashboard returned %d", recorder.Code)
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

func TestAdminLoginAcceptsPasswordOnly(t *testing.T) {
	a := testApp(t, nil)
	request := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"password":"admin-password"}`))
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("password-only login returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if !strings.Contains(recorder.Body.String(), "csrfToken") {
		t.Fatalf("password-only login did not return csrf token: %s", recorder.Body.String())
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
		ID: "private-account-id", Label: "private@example.test · Plus", Email: "private@example.test", AccountID: "chatgpt-private-id", OrganizationName: "Private private@example.test", PlanType: "plus", CodexHome: "/data/accounts/private-account-id/.codex", Enabled: true, InPool: true, RemainingQuota: &quota,
		UpstreamBaseURL: "https://upstream.example.test/v1", UpstreamAPIKey: "upstream-secret-value", AllowedModels: []string{"gpt-test"},
	}})

	publicRequest := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	publicRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", publicRecorder.Code)
	}
	publicBody := publicRecorder.Body.String()
	for _, forbidden := range []string{"private-account-id", "private@example.test", "chatgpt-private-id", "Private private@example.test", "upstream.example.test", "upstream-secret-value", "gpt-test", "credentialMetadata", "statusReason", "allowedModels", "planType", "planLimit", "email"} {
		if strings.Contains(publicBody, forbidden) {
			t.Fatalf("public dashboard exposed %q", forbidden)
		}
	}
	if !strings.Contains(publicBody, "pr***te@example.test") || !strings.Contains(publicBody, "Plus") || !strings.Contains(publicBody, `"statusTone":"low"`) || !strings.Contains(publicBody, `"statusLabel":"Limited"`) {
		t.Fatalf("public dashboard omitted expected status data: %s", publicBody)
	}

	managementRequest := httptest.NewRequest(http.MethodGet, "/admin/api/accounts", nil)
	managementRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(managementRecorder, managementRequest)
	if managementRecorder.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated management API returned %d", managementRecorder.Code)
	}

	cookies, _ := adminSession(t, a)
	authenticatedManagementRequest := httptest.NewRequest(http.MethodGet, "/admin/api/accounts", nil)
	for _, cookie := range cookies {
		authenticatedManagementRequest.AddCookie(cookie)
	}
	authenticatedManagementRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(authenticatedManagementRecorder, authenticatedManagementRequest)
	if authenticatedManagementRecorder.Code != http.StatusOK {
		t.Fatalf("authenticated management API returned %d", authenticatedManagementRecorder.Code)
	}
	managementBody := authenticatedManagementRecorder.Body.String()
	for _, forbidden := range []string{"private@example.test", "chatgpt-private-id", "/data/accounts/private-account-id/.codex", "upstream.example.test", "upstream-secret-value"} {
		if strings.Contains(managementBody, forbidden) {
			t.Fatalf("management account list exposed %q", forbidden)
		}
	}
	if !strings.Contains(managementBody, "pr***te@example.test") {
		t.Fatalf("management account list omitted masked email: %s", managementBody)
	}
	if strings.Contains(managementBody, "Private private@example.test") || !strings.Contains(managementBody, "Private pr***te@example.test") {
		t.Fatalf("management account list did not mask email in organization name: %s", managementBody)
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
		{http.MethodPost, "/admin/api/jobs/job-id/cancel", ""},
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

func TestAdminSettingsTogglePreserveProQuota(t *testing.T) {
	a := testApp(t, nil)
	cookies, csrf := adminSession(t, a)
	request := httptest.NewRequest(http.MethodPost, "/admin/api/settings", strings.NewReader(`{"preserveProQuota":true}`))
	for _, cookie := range cookies {
		request.AddCookie(cookie)
	}
	request.Header.Set("X-CSRF-Token", csrf)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("settings update returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if !a.preserveProQuota {
		t.Fatal("settings update did not enable preserveProQuota")
	}
	if !strings.Contains(recorder.Body.String(), `"preserveProQuota":true`) {
		t.Fatalf("settings response did not include enabled preserveProQuota: %s", recorder.Body.String())
	}
	var saved config
	if err := readJSON(filepath.Join(a.dataDir, "config.json"), &saved); err != nil {
		t.Fatal(err)
	}
	if saved.PreserveProQuota == nil || !*saved.PreserveProQuota {
		t.Fatalf("settings update did not persist preserveProQuota: %#v", saved.PreserveProQuota)
	}

	request = httptest.NewRequest(http.MethodPost, "/admin/api/settings", strings.NewReader(`{"preserveProQuota":false}`))
	for _, cookie := range cookies {
		request.AddCookie(cookie)
	}
	request.Header.Set("X-CSRF-Token", csrf)
	recorder = httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("settings disable returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if a.preserveProQuota {
		t.Fatal("settings update did not disable preserveProQuota")
	}
	if err := readJSON(filepath.Join(a.dataDir, "config.json"), &saved); err != nil {
		t.Fatal(err)
	}
	if saved.PreserveProQuota == nil || *saved.PreserveProQuota {
		t.Fatalf("settings update did not persist disabled preserveProQuota: %#v", saved.PreserveProQuota)
	}
}

func TestCreateCodexDeviceAuthAccountStagesUntilLogin(t *testing.T) {
	a := testApp(t, nil)
	cookies, csrf := adminSession(t, a)
	request := httptest.NewRequest(http.MethodPost, "/admin/api/accounts", strings.NewReader(`{"authType":"codex_device_auth","enabled":true,"inPool":true,"priority":100}`))
	for _, cookie := range cookies {
		request.AddCookie(cookie)
	}
	request.Header.Set("X-CSRF-Token", csrf)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusCreated {
		t.Fatalf("account create returned %d: %s", recorder.Code, recorder.Body.String())
	}
	var response struct {
		Account account `json:"account"`
	}
	if err := json.Unmarshal(recorder.Body.Bytes(), &response); err != nil {
		t.Fatal(err)
	}
	if response.Account.Enabled || response.Account.InPool {
		t.Fatalf("new device-auth account response was routable before login: %#v", response.Account)
	}
	if len(a.config.Accounts) != 1 {
		t.Fatalf("configured account count = %d", len(a.config.Accounts))
	}
	staged := a.config.Accounts[0]
	if staged.Enabled || staged.InPool || !staged.PendingPoolActivation {
		t.Fatalf("new device-auth account was not staged: %#v", staged)
	}
}

func adminSession(t *testing.T, a *app) ([]*http.Cookie, string) {
	t.Helper()
	login := httptest.NewRequest(http.MethodPost, "/admin/api/login", strings.NewReader(`{"username":"admin","password":"admin-password"}`))
	loginRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(loginRecorder, login)
	if loginRecorder.Code != http.StatusOK {
		t.Fatalf("login returned %d: %s", loginRecorder.Code, loginRecorder.Body.String())
	}
	var response struct {
		CSRFToken string `json:"csrfToken"`
	}
	if err := json.Unmarshal(loginRecorder.Body.Bytes(), &response); err != nil {
		t.Fatal(err)
	}
	return loginRecorder.Result().Cookies(), response.CSRFToken
}

func TestPublicPoolToggleDoesNotExposeAccountID(t *testing.T) {
	a := testApp(t, []account{{
		ID: "private-account-id", Label: "private@example.test · Plus", Email: "private@example.test", PlanType: "plus", Enabled: true, InPool: true, RemainingQuota: nil,
		UpstreamBaseURL: "https://upstream.example.test/v1", UpstreamAPIKey: "upstream-secret-value",
	}})
	a.state.StickySessions["gpt-test:session"] = stickySession{Key: "gpt-test:session", ModelID: "gpt-test", AccountID: "private-account-id", CreatedAt: time.Now().UTC(), LastSuccessAt: time.Now().UTC()}

	publicRequest := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	publicRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", publicRecorder.Code)
	}
	publicBody := publicRecorder.Body.String()
	for _, forbidden := range []string{"private-account-id", "private@example.test", "upstream.example.test", "upstream-secret-value", "credentialMetadata", "statusReason", "inPool"} {
		if strings.Contains(publicBody, forbidden) {
			t.Fatalf("public dashboard exposed %q", forbidden)
		}
	}
	var parsed struct {
		Dashboard struct {
			Accounts []struct {
				DisplayName string `json:"displayName"`
				Detail      string `json:"detail"`
				PoolLabel   string `json:"poolLabel"`
				PoolRef     string `json:"poolRef"`
				PoolAction  string `json:"poolAction"`
			} `json:"accounts"`
		} `json:"dashboard"`
	}
	if err := json.Unmarshal(publicRecorder.Body.Bytes(), &parsed); err != nil {
		t.Fatal(err)
	}
	if len(parsed.Dashboard.Accounts) != 1 || parsed.Dashboard.Accounts[0].DisplayName != "pr***te@example.test" || parsed.Dashboard.Accounts[0].Detail != "Plus" || parsed.Dashboard.Accounts[0].PoolLabel != "In pool" || parsed.Dashboard.Accounts[0].PoolRef == "" || parsed.Dashboard.Accounts[0].PoolAction != "pool-remove" {
		t.Fatalf("public dashboard did not return expected display state: %s", publicBody)
	}

	removeRequest := httptest.NewRequest(http.MethodPost, "/admin/api/public-dashboard/accounts/"+parsed.Dashboard.Accounts[0].PoolRef+"/pool-remove", nil)
	removeRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(removeRecorder, removeRequest)
	if removeRecorder.Code != http.StatusOK {
		t.Fatalf("public pool-remove returned %d: %s", removeRecorder.Code, removeRecorder.Body.String())
	}
	if a.config.Accounts[0].InPool {
		t.Fatal("public pool-remove did not remove account from pool")
	}
	if _, ok := a.state.StickySessions["gpt-test:session"]; ok {
		t.Fatal("public pool-remove did not clear account sticky sessions")
	}

	addRequest := httptest.NewRequest(http.MethodPost, "/admin/api/public-dashboard/accounts/"+parsed.Dashboard.Accounts[0].PoolRef+"/pool-add", nil)
	addRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(addRecorder, addRequest)
	if addRecorder.Code != http.StatusOK {
		t.Fatalf("public pool-add returned %d: %s", addRecorder.Code, addRecorder.Body.String())
	}
	if !a.config.Accounts[0].Enabled || !a.config.Accounts[0].InPool {
		t.Fatalf("public pool-add did not enable pool participation: %#v", a.config.Accounts[0])
	}
}

func TestAccountDeletePurgesCodexCredentials(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-delete", Label: "Delete Me", AuthType: "codex_device_auth", CodexHome: filepath.Join(t.TempDir(), "ignored"), Enabled: true, InPool: true}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("acct-delete")
	a.state.Health["acct-delete"] = accountHealth{LastFailureReason: "old failure"}
	a.state.Quotas["acct-delete"] = quotaSnapshot{AccountID: "acct-delete", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 50, Present: true}}}
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
	if _, ok := a.state.Health["acct-delete"]; ok {
		t.Fatal("deleted account health was retained")
	}
	if _, ok := a.state.Quotas["acct-delete"]; ok {
		t.Fatal("deleted account quota was retained")
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
	if session := a.state.StickySessions["gpt-test:session-a"]; session.ExpiresAt.IsZero() || time.Until(session.ExpiresAt) < 23*time.Hour {
		t.Fatalf("sticky session expiry was not refreshed: %#v", session)
	}
}

func TestResponsesProxyInjectsPromptCacheControlsAndTracksUsage(t *testing.T) {
	var sawPromptCacheKey string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Fatal(err)
		}
		sawPromptCacheKey, _ = body["prompt_cache_key"].(string)
		if sawPromptCacheKey == "" || !strings.HasPrefix(sawPromptCacheKey, "cp_") {
			t.Fatalf("prompt_cache_key was not auto-generated: %#v", body["prompt_cache_key"])
		}
		if body["prompt_cache_retention"] != "24h" {
			t.Fatalf("prompt_cache_retention = %#v, want 24h", body["prompt_cache_retention"])
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_cache","object":"response","output":[],"usage":{"input_tokens":2006,"input_tokens_details":{"cached_tokens":1920}}}`))
	}))
	defer upstream.Close()

	a := testApp(t, []account{{ID: "one", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/v1", UpstreamAPIKey: "upstream-key", Priority: 100}})
	a.promptCacheRetention = "24h"
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Project", "repo-main")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if binding := a.state.ResponseBindings["resp_cache"]; binding.AccountID != "one" || binding.StickyKey != "gpt-test:repo-main" {
		t.Fatalf("response binding was not recorded: %#v", binding)
	}
	stat := a.state.PromptCache["one:gpt-test"]
	if stat.RequestCount != 1 || stat.InputTokens != 2006 || stat.CachedTokens != 1920 {
		t.Fatalf("prompt cache stats not recorded: %#v", stat)
	}
	if sawPromptCacheKey == "repo-main" {
		t.Fatal("raw project id was sent as prompt_cache_key")
	}
}

func TestChatCompletionConversionPreservesPromptCacheControls(t *testing.T) {
	a := testApp(t, nil)
	candidate := account{ID: "one", UpstreamBaseURL: "https://upstream.example.test/v1", WireAPI: "responses"}
	body := []byte(`{"model":"gpt-test","messages":[{"role":"system","content":"static"},{"role":"user","content":"dynamic"}],"prompt_cache_key":"cache-a","prompt_cache_retention":"24h","stream":true}`)
	endpoint, outbound, convertResponse, err := a.prepareUpstreamRequest(candidate, body, true)
	if err != nil {
		t.Fatal(err)
	}
	if endpoint != "https://upstream.example.test/v1/responses" || !convertResponse {
		t.Fatalf("unexpected conversion endpoint=%q convert=%v", endpoint, convertResponse)
	}
	var converted map[string]any
	if err := json.Unmarshal(outbound, &converted); err != nil {
		t.Fatal(err)
	}
	if converted["prompt_cache_key"] != "cache-a" || converted["prompt_cache_retention"] != "24h" {
		t.Fatalf("cache controls were not preserved: %#v", converted)
	}
	if _, ok := converted["input"].([]any); !ok {
		t.Fatalf("messages were not converted to input: %#v", converted["input"])
	}
}

func TestStickyFallbackUsesChatMessagesAndAPIKey(t *testing.T) {
	a := testApp(t, nil)
	reqA := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
	reqB := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
	payloadA := map[string]any{"messages": []any{map[string]any{"role": "system", "content": "static"}, map[string]any{"role": "user", "content": "one"}}}
	payloadB := map[string]any{"messages": []any{map[string]any{"role": "system", "content": "static"}, map[string]any{"role": "user", "content": "two"}}}
	keyA := a.routingDecision(reqA, payloadA, "gpt-test", "client-key").StickyKey
	keyB := a.routingDecision(reqA, payloadB, "gpt-test", "client-key").StickyKey
	keyOtherClient := a.routingDecision(reqB, payloadA, "gpt-test", "other-client-key").StickyKey
	if keyA == keyB {
		t.Fatalf("different chat messages produced same sticky fallback key %q", keyA)
	}
	if keyA == keyOtherClient {
		t.Fatalf("different API keys produced same sticky fallback key %q", keyA)
	}
	if strings.Contains(keyA, "client-key") || strings.Contains(keyA, "static") || strings.Contains(keyA, "one") {
		t.Fatalf("sticky fallback key leaked raw request data: %q", keyA)
	}
}

func TestPreviousResponseIDRoutesToOriginalAccount(t *testing.T) {
	a := testApp(t, []account{
		{ID: "low", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: "https://low.example.test/v1", UpstreamAPIKey: "low"},
		{ID: "high", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "https://high.example.test/v1", UpstreamAPIKey: "high"},
	})
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:conversation-a"] = stickySession{Key: "gpt-test:conversation-a", ModelID: "gpt-test", AccountID: "low", CreatedAt: now, LastSuccessAt: now, ExpiresAt: now.Add(time.Hour)}
	a.state.ResponseBindings["resp_previous"] = responseBinding{ResponseID: "resp_previous", StickyKey: "gpt-test:conversation-a", ModelID: "gpt-test", AccountID: "low", CreatedAt: now, ExpiresAt: now.Add(time.Hour)}
	route := a.routingDecision(httptest.NewRequest(http.MethodPost, "/v1/responses", nil), map[string]any{"previous_response_id": "resp_previous", "input": "next"}, "gpt-test", "client-key")
	selected, err := a.selectAccount(route.StickyKey, "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "low" {
		t.Fatalf("previous_response_id selected %q, want low", selected.ID)
	}
}

func TestResponsesProxyAddsCurrentAccountHeaders(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_test","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{
		ID: "private-account-id", Label: "private@example.test · Team", Email: "private@example.test", OrganizationName: "Acme Workspace", PlanType: "team", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/v1", UpstreamAPIKey: "upstream-key", Priority: 100,
	}})
	now := time.Now().UTC()
	a.state.Quotas["private-account-id"] = quotaSnapshot{
		AccountID:        "private-account-id",
		OrganizationName: "Acme Workspace",
		PlanType:         "team",
		Quota:            &accountQuota{Hourly: quotaWindow{Percentage: 72, Present: true}, Weekly: quotaWindow{Percentage: 44, Present: true}},
		UsageUpdatedAt:   now,
	}
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "session-a")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if got := recorder.Header().Get("X-Codex-Pool-Account"); got != "pr***te@example.test" {
		t.Fatalf("account header = %q", got)
	}
	if got := recorder.Header().Get("X-Codex-Pool-Quota-Hourly-Remaining"); got != "72" {
		t.Fatalf("hourly quota header = %q", got)
	}
	if got := recorder.Header().Get("X-Codex-Pool-Quota-Weekly-Remaining"); got != "44" {
		t.Fatalf("weekly quota header = %q", got)
	}
	for _, forbidden := range []string{"private-account-id", "private@example.test"} {
		for key, values := range recorder.Header() {
			if strings.Contains(strings.Join(values, " "), forbidden) {
				t.Fatalf("response header %s exposed %q: %#v", key, forbidden, values)
			}
		}
	}
}

func TestCurrentStatusReturnsSessionAccountQuota(t *testing.T) {
	a := testApp(t, []account{{
		ID: "private-account-id", Label: "private@example.test · Team", Email: "private@example.test", AccountID: "chatgpt-private-id", OrganizationName: "Private private@example.test", PlanType: "team", Enabled: true, InPool: true, UpstreamBaseURL: "https://upstream.example.test/v1", UpstreamAPIKey: "upstream-secret-value", Priority: 100,
	}})
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:session-a"] = stickySession{Key: "gpt-test:session-a", ModelID: "gpt-test", AccountID: "private-account-id", CreatedAt: now.Add(-time.Minute), LastSuccessAt: now, ExpiresAt: now.Add(time.Hour)}
	a.state.Quotas["private-account-id"] = quotaSnapshot{
		AccountID:        "private-account-id",
		OrganizationName: "Private private@example.test",
		PlanType:         "team",
		Quota:            &accountQuota{Hourly: quotaWindow{Percentage: 72, Present: true}, Weekly: quotaWindow{Percentage: 44, Present: true}},
		UsageUpdatedAt:   now,
	}
	req := httptest.NewRequest(http.MethodGet, "/v1/codex-pool/status?model=alias", nil)
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "session-a")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("status returned %d: %s", recorder.Code, recorder.Body.String())
	}
	body := recorder.Body.String()
	for _, expected := range []string{`"model":"gpt-test"`, `"displayName":"pr***te@example.test"`, `"planDisplayName":"Team · Private pr***te@example.test"`, `"percentage":72`, `"percentage":44`} {
		if !strings.Contains(body, expected) {
			t.Fatalf("status body missing %s: %s", expected, body)
		}
	}
	for _, forbidden := range []string{"private-account-id", "private@example.test", "chatgpt-private-id", "upstream.example.test", "upstream-secret-value"} {
		if strings.Contains(body, forbidden) {
			t.Fatalf("status body exposed %q: %s", forbidden, body)
		}
	}
}

func TestStickySessionTTLExpiresAndReselects(t *testing.T) {
	a := testApp(t, []account{
		{ID: "old", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: "https://old.example.test", UpstreamAPIKey: "old"},
		{ID: "new", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "https://new.example.test", UpstreamAPIKey: "new"},
	})
	a.sessionAffinityTTL = time.Hour
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:session"] = stickySession{Key: "gpt-test:session", ModelID: "gpt-test", AccountID: "old", CreatedAt: now.Add(-2 * time.Hour), LastSuccessAt: now.Add(-2 * time.Hour), ExpiresAt: now.Add(-time.Minute)}

	selected, err := a.selectAccount("gpt-test:session", "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "new" {
		t.Fatalf("expired sticky session selected %q, want new", selected.ID)
	}
	if _, ok := a.state.StickySessions["gpt-test:session"]; ok {
		t.Fatalf("expired sticky session was not pruned: %#v", a.state.StickySessions["gpt-test:session"])
	}
}

func TestStickySessionTTLRefreshesOnSuccess(t *testing.T) {
	a := testApp(t, []account{{ID: "one", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "https://one.example.test", UpstreamAPIKey: "one"}})
	a.sessionAffinityTTL = time.Hour
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:session"] = stickySession{Key: "gpt-test:session", ModelID: "gpt-test", AccountID: "one", CreatedAt: now.Add(-30 * time.Minute), LastSuccessAt: now.Add(-30 * time.Minute), ExpiresAt: now.Add(30 * time.Minute)}
	previousExpiry := a.state.StickySessions["gpt-test:session"].ExpiresAt

	selected, err := a.selectAccount("gpt-test:session", "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "one" {
		t.Fatalf("active sticky session selected %q, want one", selected.ID)
	}
	a.markSuccess("gpt-test:session", "gpt-test", "one", proxyResponseInfo{})
	refreshed := a.state.StickySessions["gpt-test:session"]
	if !refreshed.ExpiresAt.After(previousExpiry) {
		t.Fatalf("sticky session expiry was not refreshed: before=%s after=%s", previousExpiry, refreshed.ExpiresAt)
	}
}

func TestPreserveProQuotaModeMovesStickySessionBackToNonPro(t *testing.T) {
	a := testApp(t, []account{
		{ID: "plus", PlanType: "plus", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: "https://plus.example.test", UpstreamAPIKey: "plus"},
		{ID: "pro", PlanType: "pro", PlanLimit: "20x", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "https://pro.example.test", UpstreamAPIKey: "pro"},
	})
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:session"] = stickySession{Key: "gpt-test:session", ModelID: "gpt-test", AccountID: "pro", CreatedAt: now, LastSuccessAt: now, ExpiresAt: now.Add(time.Hour)}

	selected, err := a.selectAccount("gpt-test:session", "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "pro" {
		t.Fatalf("preserve mode off selected %q, want existing pro sticky", selected.ID)
	}

	a.preserveProQuota = true
	selected, err = a.selectAccount("gpt-test:session", "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "plus" {
		t.Fatalf("preserve mode selected %q, want plus", selected.ID)
	}
	a.markSuccess("gpt-test:session", "gpt-test", selected.ID, proxyResponseInfo{})
	if session := a.state.StickySessions["gpt-test:session"]; session.AccountID != "plus" || session.FailoverFrom != "pro" {
		t.Fatalf("preserve mode did not rewrite sticky session from pro to plus: %#v", session)
	}
}

func TestPreserveProQuotaModeUsesProWhenNonProCoolingDown(t *testing.T) {
	a := testApp(t, []account{
		{ID: "plus", PlanType: "plus", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: "https://plus.example.test", UpstreamAPIKey: "plus"},
		{ID: "pro", PlanType: "pro", PlanLimit: "20x", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "https://pro.example.test", UpstreamAPIKey: "pro"},
	})
	a.preserveProQuota = true
	a.state.Cooldowns["plus"] = []cooldown{{ModelID: "gpt-test", NextRetryAt: time.Now().UTC().Add(time.Minute), Reason: "rate_limited"}}
	selected, err := a.selectAccount("gpt-test:new", "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "pro" {
		t.Fatalf("selected %q, want pro while plus is cooling down", selected.ID)
	}
}

func TestTransientQuotaRefreshErrorDoesNotBlockProFailover(t *testing.T) {
	plusHits := 0
	plus := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		plusHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer plus.Close()
	proHits := 0
	pro := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		proHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_pro_fallback","object":"response","output":[]}`))
	}))
	defer pro.Close()

	a := testApp(t, []account{
		{ID: "plus", PlanType: "plus", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: plus.URL, UpstreamAPIKey: "plus"},
		{ID: "pro", PlanType: "pro", PlanLimit: "20x", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: pro.URL, UpstreamAPIKey: "pro"},
	})
	a.preserveProQuota = true
	a.state.Quotas["plus"] = quotaSnapshot{AccountID: "plus", PlanType: "plus", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 0, Present: true}, Weekly: quotaWindow{Percentage: 80, Present: true}}}
	a.state.Quotas["pro"] = quotaSnapshot{
		AccountID: "pro",
		PlanType:  "pro",
		Quota:     &accountQuota{Hourly: quotaWindow{Percentage: 98, Present: true}, Weekly: quotaWindow{Percentage: 100, Present: true}},
		QuotaError: &quotaErrorInfo{
			Code:      "upstream_status",
			Message:   "quota API returned status 500",
			Timestamp: time.Now().UTC(),
		},
	}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "plus-to-pro-after-quota-poll-500")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("transient quota error caused failover response %d: %s", recorder.Code, recorder.Body.String())
	}
	if plusHits != 0 || proHits != 1 {
		t.Fatalf("plus-to-pro routing hits = plus:%d pro:%d, want 0/1", plusHits, proHits)
	}
	if session := a.state.StickySessions["gpt-test:plus-to-pro-after-quota-poll-500"]; session.AccountID != "pro" {
		t.Fatalf("plus-to-pro sticky session = %#v", session)
	}
	if status, reason := a.accountStatusLocked(a.config.Accounts[1], time.Now().UTC()); status != "ready" {
		t.Fatalf("transient quota error made Pro dashboard unavailable: %q, %q", status, reason)
	}
}

func TestExplicitQuotaAuthErrorStillBlocksRouting(t *testing.T) {
	a := testApp(t, []account{{
		ID: "pro", PlanType: "pro", Enabled: true, InPool: true, Priority: 100,
		UpstreamBaseURL: "https://pro.example.test", UpstreamAPIKey: "pro",
	}})
	a.state.Quotas["pro"] = quotaSnapshot{
		AccountID: "pro",
		PlanType:  "pro",
		Quota:     &accountQuota{Hourly: quotaWindow{Percentage: 98, Present: true}},
		QuotaError: &quotaErrorInfo{
			Code:      "invalid_token",
			Message:   "credential unavailable",
			Timestamp: time.Now().UTC(),
		},
	}
	if _, err := a.selectAccount("gpt-test:auth-error", "gpt-test", map[string]bool{}); err == nil {
		t.Fatal("explicit quota auth error did not block routing")
	}
}

func TestResponsesProxyPreservesLargeMCPToolPayloadAndStreamingEvents(t *testing.T) {
	const toolCount = 320
	longToolName := "mcp__apps_runtime__workspace_agents__upsert_agent_application_configuration_with_extended_name"
	round := 0
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		round++
		var payload map[string]any
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Fatal(err)
		}
		tools, _ := payload["tools"].([]any)
		if len(tools) != toolCount {
			t.Fatalf("forwarded tools = %d, want %d", len(tools), toolCount)
		}
		first, _ := tools[0].(map[string]any)
		function, _ := first["function"].(map[string]any)
		if function["name"] != longToolName {
			t.Fatalf("long MCP tool name changed: %#v", function["name"])
		}
		parameters, _ := function["parameters"].(map[string]any)
		properties, _ := parameters["properties"].(map[string]any)
		config, _ := properties["configuration"].(map[string]any)
		if config["description"] != strings.Repeat("schema-detail-", 200) {
			t.Fatal("large MCP JSON Schema was truncated or changed")
		}
		if round == 2 {
			if payload["previous_response_id"] != "resp_mcp_large" {
				t.Fatalf("previous response id changed: %#v", payload["previous_response_id"])
			}
			input, _ := payload["input"].([]any)
			if len(input) != 1 {
				t.Fatalf("second-round input = %#v", payload["input"])
			}
			output, _ := input[0].(map[string]any)
			if output["type"] != "function_call_output" || output["call_id"] != "call_mcp_long_1" || output["output"] != `{"saved":true}` {
				t.Fatalf("function call output changed: %#v", output)
			}
		}

		w.Header().Set("Content-Type", "text/event-stream")
		if round == 1 {
			_, _ = io.WriteString(w, "event: response.output_item.added\n")
			_, _ = io.WriteString(w, `data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_mcp_long_1","name":"`+longToolName+`","arguments":"{\\"enabled\\":true}"}}`+"\n\n")
		} else {
			_, _ = io.WriteString(w, "event: response.output_text.delta\n")
			_, _ = io.WriteString(w, `data: {"type":"response.output_text.delta","delta":"saved"}`+"\n\n")
		}
		if flusher, ok := w.(http.Flusher); ok {
			flusher.Flush()
		}
		_, _ = io.WriteString(w, "event: response.completed\n")
		responseID := "resp_mcp_large"
		inputTokens := 4096
		cachedTokens := 3072
		if round == 2 {
			responseID = "resp_mcp_large_2"
			inputTokens = 5120
			cachedTokens = 4096
		}
		_, _ = fmt.Fprintf(w, "data: {\"type\":\"response.completed\",\"response\":{\"id\":%q,\"usage\":{\"input_tokens\":%d,\"input_tokens_details\":{\"cached_tokens\":%d}}}}\n\n", responseID, inputTokens, cachedTokens)
	}))
	defer upstream.Close()

	a := testApp(t, []account{{
		ID: "provider", Enabled: true, InPool: true, Priority: 100,
		UpstreamBaseURL: upstream.URL, UpstreamAPIKey: "provider-key",
	}})
	tools := make([]map[string]any, 0, toolCount)
	for index := 0; index < toolCount; index++ {
		name := fmt.Sprintf("mcp__apps_runtime__tool_%03d", index)
		if index == 0 {
			name = longToolName
		}
		tools = append(tools, map[string]any{
			"type": "function",
			"function": map[string]any{
				"name": name,
				"parameters": map[string]any{
					"type": "object",
					"properties": map[string]any{
						"configuration": map[string]any{"type": "string", "description": strings.Repeat("schema-detail-", 200)},
					},
				},
			},
		})
	}
	proxy := func(payload map[string]any) string {
		t.Helper()
		body, err := json.Marshal(payload)
		if err != nil {
			t.Fatal(err)
		}
		req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(string(body)))
		req.Header.Set("Authorization", "Bearer client-key")
		recorder := httptest.NewRecorder()
		a.publicMux().ServeHTTP(recorder, req)
		if recorder.Code != http.StatusOK {
			t.Fatalf("large MCP proxy returned %d: %s", recorder.Code, recorder.Body.String())
		}
		return recorder.Body.String()
	}

	responseBody := proxy(map[string]any{"model": "gpt-test", "input": "hello", "stream": true, "tools": tools})
	for _, expected := range []string{longToolName, "call_mcp_long_1", "response.output_item.added", "response.completed"} {
		if !strings.Contains(responseBody, expected) {
			t.Fatalf("streaming response lost %q: %s", expected, responseBody)
		}
	}
	secondResponse := proxy(map[string]any{
		"model":                "gpt-test",
		"previous_response_id": "resp_mcp_large",
		"input": []map[string]any{{
			"type":    "function_call_output",
			"call_id": "call_mcp_long_1",
			"output":  `{"saved":true}`,
		}},
		"stream": true,
		"tools":  tools,
	})
	for _, expected := range []string{"response.output_text.delta", "saved", "resp_mcp_large_2", "response.completed"} {
		if !strings.Contains(secondResponse, expected) {
			t.Fatalf("second streaming round lost %q: %s", expected, secondResponse)
		}
	}
	if round != 2 {
		t.Fatalf("upstream rounds = %d, want 2", round)
	}
	if stat := a.state.PromptCache["provider:gpt-test"]; stat.InputTokens != 9216 || stat.CachedTokens != 7168 || stat.RequestCount != 2 {
		t.Fatalf("streaming usage was not recorded: %#v", stat)
	}
}

func TestCopyStreamingProxyResponseFlushesSSE(t *testing.T) {
	recorder := httptest.NewRecorder()
	info := copyStreamingProxyResponse(recorder, strings.NewReader("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_flushed\"}}\n\n"))
	if !recorder.Flushed {
		t.Fatal("SSE proxy buffered the response without flushing")
	}
	if info.ResponseID != "resp_flushed" {
		t.Fatalf("streaming response id = %q", info.ResponseID)
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
		if r.Header.Get("ChatGPT-Account-ID") != "acct-from-metadata" {
			t.Fatalf("missing account id from account metadata: %q", r.Header.Get("ChatGPT-Account-ID"))
		}
		if r.Header.Get("X-OpenAI-Fedramp") != "true" {
			t.Fatalf("missing fedramp header")
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_test","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "codex-meta", AccountID: "acct-from-metadata", AuthType: "codex_device_auth", Enabled: true, InPool: true, UpstreamBaseURL: upstream.URL + "/backend-api", Priority: 100}})
	home := a.accountCodexHome("codex-meta")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	idToken := fakeJWTClaims(map[string]any{"https://api.openai.com/auth": map[string]any{"chatgpt_account_is_fedramp": true}})
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

func TestDeviceAuthFailoverAfterRateLimit(t *testing.T) {
	firstHits := 0
	first := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		firstHits++
		if r.Header.Get("Authorization") != "Bearer <first-access-token>" {
			t.Fatalf("first account used unexpected authorization header %q", r.Header.Get("Authorization"))
		}
		if r.Header.Get("ChatGPT-Account-ID") != "acct-first" {
			t.Fatalf("first account used unexpected ChatGPT account header %q", r.Header.Get("ChatGPT-Account-ID"))
		}
		w.Header().Set("Retry-After", "1")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer first.Close()
	secondHits := 0
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		secondHits++
		if r.Header.Get("Authorization") != "Bearer <second-access-token>" {
			t.Fatalf("second account used unexpected authorization header %q", r.Header.Get("Authorization"))
		}
		if r.Header.Get("ChatGPT-Account-ID") != "acct-second" {
			t.Fatalf("second account used unexpected ChatGPT account header %q", r.Header.Get("ChatGPT-Account-ID"))
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_device_failover","object":"response","output":[]}`))
	}))
	defer second.Close()

	a := testApp(t, []account{
		{ID: "device-first", AccountID: "acct-first", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: first.URL},
		{ID: "device-second", AccountID: "acct-second", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: second.URL},
	})
	for _, item := range a.config.Accounts {
		home := a.accountCodexHome(item.ID)
		if err := os.MkdirAll(home, 0o700); err != nil {
			t.Fatal(err)
		}
		accessToken := "<first-access-token>"
		if item.ID == "device-second" {
			accessToken = "<second-access-token>"
		}
		authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q}}`, accessToken)
		if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
			t.Fatal(err)
		}
	}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "device-failover")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("device-auth failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if firstHits != 1 || secondHits != 1 {
		t.Fatalf("device-auth failover hits = first:%d second:%d, want 1 each", firstHits, secondHits)
	}
	if session := a.state.StickySessions["gpt-test:device-failover"]; session.AccountID != "device-second" {
		t.Fatalf("device-auth sticky failover = %#v, want device-second", session)
	}
	if reason := a.state.Health["device-first"].LastFailureReason; reason != "rate_limited" {
		t.Fatalf("first device-auth account failure reason = %q, want rate_limited", reason)
	}
}

func TestCliproxyAuthAdapterUsesAnIsolatedRefreshOwner(t *testing.T) {
	a := testApp(t, []account{{ID: "device-one", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexGatewayMode = "sidecar"
	item := a.config.Accounts[0]
	home := a.accountCodexHome(item.ID)
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	idToken := fakeJWTClaims(map[string]any{
		"email":                       "pool-user@example.test",
		"https://api.openai.com/auth": map[string]any{"chatgpt_account_id": "acct-one", "chatgpt_plan_type": "team", "chatgpt_account_name": "Acme Workspace"},
	})
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"id_token":%q,"access_token":"<pool-access-token>","refresh_token":"<pool-refresh-token>"}}`, idToken)
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := a.syncCliproxyAuth(item, true); err != nil {
		t.Fatal(err)
	}
	var sidecar cliproxyCodexAuthFile
	if err := readJSON(a.cliproxyAuthPath(item.ID), &sidecar); err != nil {
		t.Fatal(err)
	}
	if sidecar.Type != "codex" || sidecar.Prefix != "codex-pool-device-one" || sidecar.AccountID != "acct-one" || sidecar.PlanType != "team" || sidecar.OrganizationName != "Acme Workspace" {
		t.Fatalf("unexpected cliproxy auth record: %#v", sidecar)
	}
	if sidecar.AccessToken != "<pool-access-token>" || sidecar.RefreshToken != "<pool-refresh-token>" {
		t.Fatalf("cliproxy auth did not preserve token fields")
	}

	// Sidecar owns refreshes. Pool must use its current copy for quota reads and
	// never refresh the original Codex CLI auth in parallel.
	sidecar.AccessToken = "<sidecar-refreshed-access-token>"
	if err := writeJSONAtomic(a.cliproxyAuthPath(item.ID), sidecar); err != nil {
		t.Fatal(err)
	}
	active, err := a.activeCodexAuthContext(context.Background(), item)
	if err != nil {
		t.Fatal(err)
	}
	if active.AccessToken != "<sidecar-refreshed-access-token>" {
		t.Fatalf("active sidecar auth token = %q", active.AccessToken)
	}
}

func TestCliproxyMetadataUpdatePreservesSidecarRefreshTokens(t *testing.T) {
	a := testApp(t, []account{{ID: "device-one", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexGatewayMode = "sidecar"
	item := a.config.Accounts[0]
	path := a.cliproxyAuthPath(item.ID)
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	original := cliproxyCodexAuthFile{
		Type:         "codex",
		AccessToken:  "<sidecar-access-token>",
		RefreshToken: "<sidecar-refresh-token>",
		AccountID:    "acct-old",
		Prefix:       cliproxyAccountPrefix(item.ID),
		PlanType:     "plus",
	}
	if err := writeJSONAtomic(path, original); err != nil {
		t.Fatal(err)
	}

	item.Email = "pool-user@example.test"
	item.AccountID = "acct-new"
	item.OrganizationName = "Acme Workspace"
	item.PlanType = "team"
	item.PlanRank = planRank(item.PlanType)
	if err := a.updateCliproxyAuthMetadata(item); err != nil {
		t.Fatal(err)
	}
	var updated cliproxyCodexAuthFile
	if err := readJSON(path, &updated); err != nil {
		t.Fatal(err)
	}
	if updated.AccessToken != original.AccessToken || updated.RefreshToken != original.RefreshToken {
		t.Fatalf("metadata update changed sidecar-owned tokens: %#v", updated)
	}
	if updated.Email != "pool-user@example.test" || updated.AccountID != "acct-new" || updated.OrganizationName != "Acme Workspace" || updated.PlanType != "team" {
		t.Fatalf("metadata update did not refresh account fields: %#v", updated)
	}
}

func TestDeviceAuthFailoverThroughCliproxyAdapter(t *testing.T) {
	seenModels := make([]string, 0, 2)
	sidecar := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/responses" {
			t.Fatalf("unexpected sidecar path %s", r.URL.Path)
		}
		if r.Header.Get("Authorization") != "Bearer sidecar-test-key" {
			t.Fatalf("unexpected sidecar authorization %q", r.Header.Get("Authorization"))
		}
		var payload map[string]any
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Fatal(err)
		}
		model, _ := payload["model"].(string)
		seenModels = append(seenModels, model)
		switch model {
		case "codex-pool-device-first/gpt-test":
			w.Header().Set("Retry-After", "1")
			w.WriteHeader(http.StatusTooManyRequests)
		case "codex-pool-device-second/gpt-test":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"id":"resp_sidecar_failover","object":"response","output":[]}`))
		default:
			t.Fatalf("unexpected sidecar model %q", model)
		}
	}))
	defer sidecar.Close()

	a := testApp(t, []account{
		{ID: "device-first", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100},
		{ID: "device-second", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 10},
	})
	a.codexGatewayMode = "sidecar"
	a.cliproxyBaseURL = sidecar.URL + "/v1"
	a.cliproxyAPIKey = "sidecar-test-key"
	for _, item := range a.config.Accounts {
		home := a.accountCodexHome(item.ID)
		if err := os.MkdirAll(home, 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(`{"auth_mode":"chatgpt","tokens":{"access_token":"<test-access-token>","refresh_token":"<test-refresh-token>"}}`), 0o600); err != nil {
			t.Fatal(err)
		}
	}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "cliproxy-failover")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("cliproxy failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if got := strings.Join(seenModels, ","); got != "codex-pool-device-first/gpt-test,codex-pool-device-second/gpt-test" {
		t.Fatalf("sidecar account sequence = %q", got)
	}
	if session := a.state.StickySessions["gpt-test:cliproxy-failover"]; session.AccountID != "device-second" {
		t.Fatalf("cliproxy sticky failover = %#v", session)
	}
	if reason := a.state.Health["device-first"].LastFailureReason; reason != "rate_limited" {
		t.Fatalf("cliproxy first account reason = %q", reason)
	}
}

func TestDeviceAuthFailoverThroughCliproxyAdapterAfterAuthFailure(t *testing.T) {
	seenModels := make([]string, 0, 2)
	sidecar := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/responses" {
			t.Fatalf("unexpected sidecar path %s", r.URL.Path)
		}
		var payload map[string]any
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Fatal(err)
		}
		model, _ := payload["model"].(string)
		seenModels = append(seenModels, model)
		switch model {
		case "codex-pool-device-first/gpt-test":
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":{"code":"invalid_token","message":"secret-token-body"}}`))
		case "codex-pool-device-second/gpt-test":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"id":"resp_sidecar_auth_failover","object":"response","output":[]}`))
		default:
			t.Fatalf("unexpected sidecar model %q", model)
		}
	}))
	defer sidecar.Close()

	a := testApp(t, []account{
		{ID: "device-first", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100},
		{ID: "device-second", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 10},
	})
	a.codexGatewayMode = "sidecar"
	a.cliproxyBaseURL = sidecar.URL + "/v1"
	a.cliproxyAPIKey = "sidecar-test-key"
	for _, item := range a.config.Accounts {
		home := a.accountCodexHome(item.ID)
		if err := os.MkdirAll(home, 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(`{"auth_mode":"chatgpt","tokens":{"access_token":"<test-access-token>","refresh_token":"<test-refresh-token>"}}`), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:cliproxy-auth-failover"] = stickySession{Key: "gpt-test:cliproxy-auth-failover", ModelID: "gpt-test", AccountID: "device-first", CreatedAt: now.Add(-time.Minute), LastSuccessAt: now.Add(-time.Minute), ExpiresAt: now.Add(time.Hour)}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "cliproxy-auth-failover")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("cliproxy auth failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if got := strings.Join(seenModels, ","); got != "codex-pool-device-first/gpt-test,codex-pool-device-second/gpt-test" {
		t.Fatalf("sidecar account sequence = %q", got)
	}
	if session := a.state.StickySessions["gpt-test:cliproxy-auth-failover"]; session.AccountID != "device-second" || session.FailoverFrom != "device-first" {
		t.Fatalf("cliproxy auth failover sticky session = %#v", session)
	}
	snapshot := a.state.Quotas["device-first"]
	if snapshot.QuotaError == nil || snapshot.QuotaError.Code != "invalid_token" {
		t.Fatalf("auth failure did not mark first account unavailable: %#v", snapshot)
	}
	if strings.Contains(snapshot.QuotaError.Message, "secret-token-body") {
		t.Fatalf("auth failure persisted upstream body: %#v", snapshot.QuotaError)
	}
	selected, err := a.selectAccount("gpt-test:new-session", "gpt-test", map[string]bool{})
	if err != nil {
		t.Fatal(err)
	}
	if selected.ID != "device-second" {
		t.Fatalf("unavailable auth-failed account was selected: %q", selected.ID)
	}
}

func TestDeviceAuthFailoverAfterRefreshTokenRevoked(t *testing.T) {
	refresh := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
	}))
	defer refresh.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", refresh.URL)

	firstHits := 0
	first := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		firstHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer first.Close()
	secondHits := 0
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		secondHits++
		if r.Header.Get("Authorization") != "Bearer <second-access-token>" {
			t.Fatalf("second account used unexpected authorization header %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_refresh_failover","object":"response","output":[]}`))
	}))
	defer second.Close()

	a := testApp(t, []account{
		{ID: "device-first", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: first.URL},
		{ID: "device-second", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: second.URL},
	})
	auths := map[string]string{
		"device-first":  fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<revoked-refresh-token>"}}`, fakeJWT(time.Now().Add(-time.Minute))),
		"device-second": `{"auth_mode":"chatgpt","tokens":{"access_token":"<second-access-token>"}}`,
	}
	for _, item := range a.config.Accounts {
		home := a.accountCodexHome(item.ID)
		if err := os.MkdirAll(home, 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(auths[item.ID]), 0o600); err != nil {
			t.Fatal(err)
		}
	}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "refresh-auth-failover")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("refresh auth failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if firstHits != 0 || secondHits != 1 {
		t.Fatalf("refresh auth failover hits = first:%d second:%d", firstHits, secondHits)
	}
	if session := a.state.StickySessions["gpt-test:refresh-auth-failover"]; session.AccountID != "device-second" {
		t.Fatalf("refresh auth failover sticky session = %#v", session)
	}
	snapshot := a.state.Quotas["device-first"]
	if snapshot.QuotaError == nil || snapshot.QuotaError.Code != "account_auth_failed" {
		t.Fatalf("refresh auth failure did not mark first account unavailable: %#v", snapshot)
	}
}

func TestDuplicateUpstreamAccountsAreNotFailoverCapacity(t *testing.T) {
	primaryHits := 0
	primary := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		primaryHits++
		w.Header().Set("Retry-After", "1")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer primary.Close()
	duplicateHits := 0
	duplicate := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		duplicateHits++
		w.WriteHeader(http.StatusOK)
	}))
	defer duplicate.Close()
	backupHits := 0
	backup := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		backupHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_duplicate_guard","object":"response","output":[]}`))
	}))
	defer backup.Close()

	a := testApp(t, []account{
		{ID: "slot-primary", AuthType: "codex_device_auth", AccountID: "upstream-shared", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: primary.URL},
		{ID: "slot-duplicate", AuthType: "codex_device_auth", AccountID: "upstream-shared", Enabled: true, InPool: true, Priority: 90, UpstreamBaseURL: duplicate.URL},
		{ID: "slot-backup", AuthType: "codex_device_auth", AccountID: "upstream-backup", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: backup.URL},
	})
	writeCodexDeviceAuth(t, a, "slot-primary", "upstream-shared", "shared@example.test")
	writeCodexDeviceAuth(t, a, "slot-duplicate", "upstream-shared", "shared@example.test")
	writeCodexDeviceAuth(t, a, "slot-backup", "upstream-backup", "backup@example.test")

	// Regression guard: two local device-auth slots for one upstream ChatGPT
	// account must not become immediate retry targets inside the same request.
	// The next request may elect a healthy sibling as the single representative,
	// but same-request failover still skips the duplicate identity and either
	// uses a different upstream account or fails closed.
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "duplicate-upstream")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("duplicate upstream guard returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if primaryHits != 1 || duplicateHits != 0 || backupHits != 1 {
		t.Fatalf("routing hits = primary:%d duplicate:%d backup:%d", primaryHits, duplicateHits, backupHits)
	}
	session := a.state.StickySessions["gpt-test:duplicate-upstream"]
	if session.AccountID != "slot-backup" {
		t.Fatalf("duplicate upstream sticky session = %#v", session)
	}
	status, reason := a.accountStatusLocked(a.config.Accounts[1], time.Now().UTC())
	if status != "ready" {
		t.Fatalf("duplicate sibling did not become next-request representative: %q, %q", status, reason)
	}
}

func TestDuplicateUpstreamPositiveQuotaCredentialRepresentsIdentityBeforePro(t *testing.T) {
	primaryHits := 0
	primary := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		primaryHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_team_primary","object":"response","output":[]}`))
	}))
	defer primary.Close()
	duplicateHits := 0
	duplicate := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		duplicateHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_team_duplicate","object":"response","output":[]}`))
	}))
	defer duplicate.Close()
	proHits := 0
	pro := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		proHits++
		w.WriteHeader(http.StatusOK)
	}))
	defer pro.Close()

	a := testApp(t, []account{
		{ID: "team-primary", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: primary.URL},
		{ID: "team-duplicate", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 90, UpstreamBaseURL: duplicate.URL},
		{ID: "pro-backup", AuthType: "codex_device_auth", AccountID: "upstream-pro", PlanType: "pro", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: pro.URL},
	})
	a.preserveProQuota = true
	preserve := true
	a.config.PreserveProQuota = &preserve
	a.state.Quotas["team-primary"] = quotaSnapshot{AccountID: "team-primary", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 99, Present: true}, Weekly: quotaWindow{Percentage: 0, Present: true}}}
	a.state.Quotas["team-duplicate"] = quotaSnapshot{AccountID: "team-duplicate", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 99, Present: true}, Weekly: quotaWindow{Percentage: 61, Present: true}}}
	a.state.Quotas["pro-backup"] = quotaSnapshot{AccountID: "pro-backup", PlanType: "pro", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 97, Present: true}, Weekly: quotaWindow{Percentage: 11, Present: true}}}
	writeCodexDeviceAuth(t, a, "team-primary", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "team-duplicate", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "pro-backup", "upstream-pro", "pro@example.test")

	// The representative must come from the credential copy with positive quota,
	// not from the stale zero-quota slot that happens to sort first. Otherwise a
	// Team identity with healthy local auth copies falls through to Pro even
	// though one duplicate slot can still serve the next request.
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "team-before-pro")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("same-identity quota routing returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if primaryHits != 0 || duplicateHits != 1 || proHits != 0 {
		t.Fatalf("routing hits = primary:%d duplicate:%d pro:%d", primaryHits, duplicateHits, proHits)
	}
	session := a.state.StickySessions["gpt-test:team-before-pro"]
	if session.AccountID != "team-duplicate" {
		t.Fatalf("same-identity quota sticky session = %#v", session)
	}
}

func TestDuplicateUpstreamCoolingRepresentativeUsesHealthyCredentialBeforePro(t *testing.T) {
	primaryHits := 0
	primary := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		primaryHits++
		w.WriteHeader(http.StatusOK)
	}))
	defer primary.Close()
	duplicateHits := 0
	duplicate := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		duplicateHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_team_cooldown_duplicate","object":"response","output":[]}`))
	}))
	defer duplicate.Close()
	proHits := 0
	pro := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		proHits++
		w.WriteHeader(http.StatusOK)
	}))
	defer pro.Close()

	a := testApp(t, []account{
		{ID: "team-primary", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: primary.URL},
		{ID: "team-duplicate", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 90, UpstreamBaseURL: duplicate.URL},
		{ID: "pro-backup", AuthType: "codex_device_auth", AccountID: "upstream-pro", PlanType: "pro", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: pro.URL},
	})
	a.preserveProQuota = true
	a.state.Cooldowns["team-primary"] = []cooldown{{ModelID: "gpt-test", NextRetryAt: time.Now().UTC().Add(time.Minute), Reason: "rate_limited"}}
	a.state.Quotas["team-primary"] = quotaSnapshot{AccountID: "team-primary", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 80, Present: true}, Weekly: quotaWindow{Percentage: 80, Present: true}}}
	a.state.Quotas["team-duplicate"] = quotaSnapshot{AccountID: "team-duplicate", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 99, Present: true}, Weekly: quotaWindow{Percentage: 61, Present: true}}}
	a.state.Quotas["pro-backup"] = quotaSnapshot{AccountID: "pro-backup", PlanType: "pro", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 97, Present: true}, Weekly: quotaWindow{Percentage: 11, Present: true}}}
	writeCodexDeviceAuth(t, a, "team-primary", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "team-duplicate", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "pro-backup", "upstream-pro", "pro@example.test")

	// Cooldown is scoped to the local representative that just hit a rate limit.
	// For a later request, a healthy credential copy for the same non-Pro
	// identity should represent that identity before the router burns Pro quota.
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "cooling-team-before-pro")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("cooldown duplicate routing returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if primaryHits != 0 || duplicateHits != 1 || proHits != 0 {
		t.Fatalf("routing hits = primary:%d duplicate:%d pro:%d", primaryHits, duplicateHits, proHits)
	}
	session := a.state.StickySessions["gpt-test:cooling-team-before-pro"]
	if session.AccountID != "team-duplicate" {
		t.Fatalf("cooldown duplicate sticky session = %#v", session)
	}
}

func TestDuplicateUpstreamHealthyCredentialCanRepresentIdentity(t *testing.T) {
	primaryHits := 0
	primary := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		primaryHits++
		w.WriteHeader(http.StatusOK)
	}))
	defer primary.Close()
	duplicateHits := 0
	duplicate := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		duplicateHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_team_credential_copy","object":"response","output":[]}`))
	}))
	defer duplicate.Close()
	proHits := 0
	pro := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		proHits++
		w.WriteHeader(http.StatusOK)
	}))
	defer pro.Close()

	a := testApp(t, []account{
		{ID: "team-primary", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: primary.URL},
		{ID: "team-duplicate", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 90, UpstreamBaseURL: duplicate.URL},
		{ID: "pro-backup", AuthType: "codex_device_auth", AccountID: "upstream-pro", PlanType: "pro", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: pro.URL},
	})
	a.preserveProQuota = true
	a.state.Quotas["team-primary"] = quotaSnapshot{AccountID: "team-primary", PlanType: "team", QuotaError: &quotaErrorInfo{Code: "token_invalidated", Message: "credential unavailable", Timestamp: time.Now().UTC()}}
	a.state.Quotas["team-duplicate"] = quotaSnapshot{AccountID: "team-duplicate", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 99, Present: true}, Weekly: quotaWindow{Percentage: 61, Present: true}}}
	a.state.Quotas["pro-backup"] = quotaSnapshot{AccountID: "pro-backup", PlanType: "pro", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 97, Present: true}, Weekly: quotaWindow{Percentage: 11, Present: true}}}
	writeCodexDeviceAuth(t, a, "team-primary", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "team-duplicate", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "pro-backup", "upstream-pro", "pro@example.test")

	// A metadata/auth error means the local credential copy is unavailable, not
	// that the shared upstream identity should be abandoned for Pro. Select one
	// healthy sibling as the representative, while still treating that identity
	// as a single piece of capacity.
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "healthy-copy-before-pro")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("healthy duplicate credential routing returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if primaryHits != 0 || duplicateHits != 1 || proHits != 0 {
		t.Fatalf("routing hits = primary:%d duplicate:%d pro:%d", primaryHits, duplicateHits, proHits)
	}
	session := a.state.StickySessions["gpt-test:healthy-copy-before-pro"]
	if session.AccountID != "team-duplicate" {
		t.Fatalf("healthy duplicate credential sticky session = %#v", session)
	}
}

func TestErroredDuplicateManualQuotaDoesNotKeepZeroQuotaRepresentativeEligible(t *testing.T) {
	teamHits := 0
	team := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		teamHits++
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer team.Close()
	duplicateHits := 0
	duplicate := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		duplicateHits++
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer duplicate.Close()
	proHits := 0
	pro := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		proHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_pro_backup","object":"response","output":[]}`))
	}))
	defer pro.Close()

	one := 1
	a := testApp(t, []account{
		{ID: "team-primary", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: team.URL},
		{ID: "team-duplicate", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 90, RemainingQuota: &one, UpstreamBaseURL: duplicate.URL},
		{ID: "pro-backup", AuthType: "codex_device_auth", AccountID: "upstream-pro", PlanType: "pro", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: pro.URL},
	})
	a.preserveProQuota = true
	a.state.Quotas["team-primary"] = quotaSnapshot{AccountID: "team-primary", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 0, Present: true}, Weekly: quotaWindow{Percentage: 37, Present: true}}}
	a.state.Quotas["team-duplicate"] = quotaSnapshot{AccountID: "team-duplicate", PlanType: "team", QuotaError: &quotaErrorInfo{Code: "token_invalidated", Message: "credential unavailable", Timestamp: time.Now().UTC()}}
	a.state.Quotas["pro-backup"] = quotaSnapshot{AccountID: "pro-backup", PlanType: "pro", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 93, Present: true}, Weekly: quotaWindow{Percentage: 99, Present: true}}}
	writeCodexDeviceAuth(t, a, "team-primary", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "team-duplicate", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "pro-backup", "upstream-pro", "pro@example.test")

	// An errored duplicate may still carry stale manual quota from the dashboard.
	// That stale hint must not keep a zero-quota representative selectable; the
	// next distinct upstream identity is the only real backup capacity.
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "errored-duplicate-quota-hint")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("errored duplicate quota hint routing returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if teamHits != 0 || duplicateHits != 0 || proHits != 1 {
		t.Fatalf("routing hits = team:%d duplicate:%d pro:%d", teamHits, duplicateHits, proHits)
	}
	session := a.state.StickySessions["gpt-test:errored-duplicate-quota-hint"]
	if session.AccountID != "pro-backup" {
		t.Fatalf("errored duplicate quota hint sticky session = %#v", session)
	}
}

func TestDeviceAuthZeroQuotaAccountIsNotSelected(t *testing.T) {
	zero := 0
	emptyHits := 0
	empty := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		emptyHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer empty.Close()
	ready := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer <ready-access-token>" {
			t.Fatalf("ready device-auth account used unexpected authorization header %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_device_ready","object":"response","output":[]}`))
	}))
	defer ready.Close()

	a := testApp(t, []account{
		{ID: "device-empty", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100, RemainingQuota: &zero, UpstreamBaseURL: empty.URL},
		{ID: "device-ready", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: ready.URL},
	})
	home := a.accountCodexHome("device-ready")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(`{"auth_mode":"chatgpt","tokens":{"access_token":"<ready-access-token>"}}`), 0o600); err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("device-auth zero quota routing returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if emptyHits != 0 {
		t.Fatalf("zero-quota device-auth account was called %d times", emptyHits)
	}
}

func TestCodexQuotaRefreshUpdatesDashboardState(t *testing.T) {
	resetAt := time.Now().UTC().Add(7 * time.Hour).Unix()
	var sawAccountHeader bool
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/backend-api/wham/usage":
			if r.Header.Get("Authorization") == "" {
				t.Fatal("missing authorization header")
			}
			if r.Header.Get("ChatGPT-Account-Id") == "acct-chatgpt" {
				sawAccountHeader = true
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = fmt.Fprintf(w, `{
			"plan_type":"team",
			"workspace_name":"Acme Workspace",
			"rate_limit":{
				"allowed":true,
				"limit_reached":false,
				"primary_window":{"used_percent":30,"limit_window_seconds":18000,"reset_after_seconds":60},
				"secondary_window":{"used_percent":80,"limit_window_seconds":604800,"reset_at":%d}
			}
		}`, resetAt)
		case "/backend-api/accounts/check/v4-2023-04-27":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"accounts":{"acct-chatgpt":{"account":{"account_id":"acct-chatgpt","name":"Acme Workspace","plan_type":"team"},"entitlement":{"subscription_plan":"chatgptteamplan"}}},"account_ordering":["acct-chatgpt"]}`))
		case "/backend-api/subscriptions":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"subscription_plan":"chatgptteamplan"}`))
		default:
			t.Fatalf("unexpected usage path %s", r.URL.Path)
		}
	}))
	defer usage.Close()

	a := testApp(t, []account{{ID: "codex-quota", AccountID: "acct-chatgpt", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexBaseURL = usage.URL + "/backend-api"
	home := a.accountCodexHome("codex-quota")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<refresh-token>"}}`, fakeJWT(time.Now().Add(time.Hour)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}

	snapshot, err := a.refreshAccountQuota(context.Background(), "codex-quota")
	if err != nil {
		t.Fatal(err)
	}
	if !sawAccountHeader {
		t.Fatal("quota refresh did not send ChatGPT-Account-Id")
	}
	if snapshot.PlanType != "team" || snapshot.OrganizationName != "Acme Workspace" || snapshot.Quota == nil {
		t.Fatalf("unexpected quota snapshot: %#v", snapshot)
	}
	if a.config.Accounts[0].OrganizationName != "Acme Workspace" || a.config.Accounts[0].Label != "codex-quota" {
		t.Fatalf("quota refresh did not update account organization display: %#v", a.config.Accounts[0])
	}
	publicRequest := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	publicRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(publicRecorder, publicRequest)
	if publicRecorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", publicRecorder.Code)
	}
	if body := publicRecorder.Body.String(); !strings.Contains(body, "Credential 1") || !strings.Contains(body, "Team · Acme Workspace") {
		t.Fatalf("public dashboard omitted team organization label: %s", body)
	}
	if snapshot.Quota.Hourly.Percentage != 70 || snapshot.Quota.Hourly.WindowMinutes == nil || *snapshot.Quota.Hourly.WindowMinutes != 300 {
		t.Fatalf("unexpected hourly quota: %#v", snapshot.Quota.Hourly)
	}
	if snapshot.Quota.Weekly.Percentage != 20 || snapshot.Quota.Weekly.ResetAt == nil || *snapshot.Quota.Weekly.ResetAt != resetAt {
		t.Fatalf("unexpected weekly quota: %#v", snapshot.Quota.Weekly)
	}
	if a.config.Accounts[0].RemainingQuota == nil || *a.config.Accounts[0].RemainingQuota != 20 {
		t.Fatalf("remaining quota hint not updated: %#v", a.config.Accounts[0].RemainingQuota)
	}
	status, reason := a.accountStatusLocked(a.config.Accounts[0], time.Now().UTC())
	if status != "low" || !strings.Contains(reason, "Quota window") {
		t.Fatalf("quota status = %q/%q, want low quota window", status, reason)
	}

	request := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", recorder.Code)
	}
	body := recorder.Body.String()
	if !strings.Contains(body, `"hourly"`) || !strings.Contains(body, `"weekly"`) {
		t.Fatalf("public dashboard did not include quota windows: %s", body)
	}
	if strings.Contains(body, "acct-chatgpt") || strings.Contains(body, "<refresh-token>") {
		t.Fatalf("public dashboard exposed credential/account internals: %s", body)
	}
}

func TestCodexQuotaRefreshClearsStoredPersonalOrganizationName(t *testing.T) {
	resetAt := time.Now().UTC().Add(time.Hour).Unix()
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/backend-api/wham/usage":
			w.Header().Set("Content-Type", "application/json")
			_, _ = fmt.Fprintf(w, `{
				"plan_type":"team",
				"rate_limit":{
					"allowed":true,
					"limit_reached":false,
					"primary_window":{"used_percent":30,"limit_window_seconds":18000,"reset_after_seconds":60},
					"secondary_window":{"used_percent":40,"limit_window_seconds":604800,"reset_at":%d}
				}
			}`, resetAt)
		case "/backend-api/accounts/check/v4-2023-04-27":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"accounts":{"acct-chatgpt":{"account":{"account_id":"acct-chatgpt","name":"Yi-Fan Liou","plan_type":"team"},"entitlement":{"subscription_plan":"chatgptteamplan"}}},"account_ordering":["acct-chatgpt"]}`))
		case "/backend-api/subscriptions":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"subscription_plan":"chatgptteamplan"}`))
		default:
			t.Fatalf("unexpected usage path %s", r.URL.Path)
		}
	}))
	defer usage.Close()

	a := testApp(t, []account{{ID: "codex-team", AccountID: "acct-chatgpt", AuthType: "codex_device_auth", Enabled: true, InPool: true, OrganizationName: "Yi-Fan Liou", PlanType: "team"}})
	a.codexBaseURL = usage.URL + "/backend-api"
	home := a.accountCodexHome("codex-team")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<refresh-token>"}}`, fakeJWT(time.Now().Add(time.Hour)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}

	snapshot, err := a.refreshAccountQuota(context.Background(), "codex-team")
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.PlanType != "team" || snapshot.OrganizationName != "" {
		t.Fatalf("personal account name was retained in quota snapshot: %#v", snapshot)
	}
	if a.config.Accounts[0].OrganizationName != "" {
		t.Fatalf("personal account name was retained in account config: %#v", a.config.Accounts[0])
	}
}

func TestOrganizationSetActionIsNotAvailable(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-team", Email: "user@example.test", AuthType: "codex_device_auth", Enabled: true, InPool: true, PlanType: "team"}})
	request := httptest.NewRequest(http.MethodPost, "/admin/api/accounts/acct-team/organization/set", strings.NewReader(`{"organizationName":"markliou"}`))
	recorder := httptest.NewRecorder()
	a.handleAccountAction(recorder, request)
	if recorder.Code != http.StatusNotFound {
		t.Fatalf("organization override action returned %d, want 404: %s", recorder.Code, recorder.Body.String())
	}
}

func TestQuotaOrganizationControlsTeamDisplayName(t *testing.T) {
	a := testApp(t, []account{{ID: "acct-team", Email: "user@example.test", AuthType: "codex_device_auth", Enabled: true, InPool: true, PlanType: "team"}})
	a.state.Quotas["acct-team"] = quotaSnapshot{AccountID: "acct-team", OrganizationName: "markliou", PlanType: "team"}
	dashboard := a.publicDashboardAccountLocked(a.config.Accounts[0], 0, time.Now().UTC())
	if dashboard["detail"] != "Team · markliou" {
		t.Fatalf("quota organization was not used in team display: %#v", dashboard)
	}
}

func TestAccountActiveLocked(t *testing.T) {
	now := time.Now().UTC()
	cases := []struct {
		name string
		last time.Time
		want bool
	}{
		{"never used", time.Time{}, false},
		{"just now", now, true},
		{"within window", now.Add(-30 * time.Second), true},
		{"on boundary", now.Add(-accountActiveWindow), false},
		{"past window", now.Add(-2 * time.Minute), false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := accountActiveLocked(accountHealth{LastSuccessAt: tc.last}, now)
			if got != tc.want {
				t.Fatalf("accountActiveLocked(last=%v) = %v, want %v", tc.last, got, tc.want)
			}
		})
	}
}

func TestPromptCacheStatsForAccountLocked(t *testing.T) {
	a := testApp(t, []account{{ID: "acct", Enabled: true, InPool: true}})
	a.state.PromptCache = map[string]promptCacheStat{
		"acct:gpt-a":  {AccountID: "acct", ModelID: "gpt-a", RequestCount: 3, InputTokens: 1000, CachedTokens: 700},
		"acct:gpt-b":  {AccountID: "acct", ModelID: "gpt-b", RequestCount: 2, InputTokens: 500, CachedTokens: 100},
		"other:gpt-a": {AccountID: "other", ModelID: "gpt-a", RequestCount: 9, InputTokens: 9000, CachedTokens: 9000},
	}
	input, cached, requests := a.promptCacheStatsForAccountLocked("acct")
	if input != 1500 || cached != 800 || requests != 5 {
		t.Fatalf("aggregate = input %d cached %d requests %d, want 1500/800/5", input, cached, requests)
	}
	if input, cached, requests := a.promptCacheStatsForAccountLocked("missing"); input != 0 || cached != 0 || requests != 0 {
		t.Fatalf("missing account aggregate = %d/%d/%d, want 0/0/0", input, cached, requests)
	}
}

func TestPromptCacheColdStartAndResetWindow(t *testing.T) {
	a := testApp(t, []account{{ID: "acct", Enabled: true, InPool: true}})
	now := time.Now().UTC()
	rec := func(input, cached uint64) {
		a.recordPromptCacheUsageLocked("acct", "gpt-test", promptCacheUsage{InputTokens: input, CachedTokens: cached, Present: true}, now)
	}
	rec(2000, 1900) // warm
	rec(1500, 0)    // cold start (eligible, no cache)
	rec(500, 0)     // sub-1024: not cache-eligible, must not count as cold

	stat := a.state.PromptCache["acct:gpt-test"]
	if stat.ColdRequestCount != 1 {
		t.Fatalf("cold request count = %d, want 1", stat.ColdRequestCount)
	}

	// No reset yet: window equals lifetime totals.
	win := a.promptCacheWindowLocked()
	if win["inputTokens"].(uint64) != 4000 || win["cachedTokens"].(uint64) != 1900 || win["coldRequestCount"].(uint64) != 1 || win["requestCount"].(uint64) != 3 {
		t.Fatalf("pre-reset window = %#v", win)
	}

	a.resetPromptCacheWindowLocked(now)
	if win := a.promptCacheWindowLocked(); win["inputTokens"].(uint64) != 0 || win["cachedTokens"].(uint64) != 0 || win["coldRequestCount"].(uint64) != 0 {
		t.Fatalf("window right after reset should be zero: %#v", win)
	}

	// Fresh traffic after reset shows only the delta.
	rec(3000, 2700)
	rec(1200, 0)
	win = a.promptCacheWindowLocked()
	if win["inputTokens"].(uint64) != 4200 || win["cachedTokens"].(uint64) != 2700 || win["coldRequestCount"].(uint64) != 1 || win["requestCount"].(uint64) != 2 {
		t.Fatalf("post-reset window = %#v", win)
	}
	// Lifetime totals are preserved across the reset.
	if lifetime := a.state.PromptCache["acct:gpt-test"]; lifetime.RequestCount != 5 || lifetime.ColdRequestCount != 2 {
		t.Fatalf("lifetime totals not preserved: %#v", lifetime)
	}
}

func TestPromptCacheWindowPerAccountReset(t *testing.T) {
	a := testApp(t, []account{{ID: "a", Enabled: true, InPool: true}, {ID: "b", Enabled: true, InPool: true}})
	now := time.Now().UTC()
	rec := func(acct string, input, cached uint64) {
		a.recordPromptCacheUsageLocked(acct, "gpt-test", promptCacheUsage{InputTokens: input, CachedTokens: cached, Present: true}, now)
	}
	rec("a", 1000, 800)
	rec("b", 1000, 600)

	// Reset only account a; b's window must be untouched.
	a.resetPromptCacheWindowForAccountLocked("a", now)
	if win := a.promptCacheWindowForAccountLocked("a"); win["inputTokens"].(uint64) != 0 {
		t.Fatalf("account a window not reset: %#v", win)
	}
	if win := a.promptCacheWindowForAccountLocked("b"); win["inputTokens"].(uint64) != 1000 || win["cachedTokens"].(uint64) != 600 {
		t.Fatalf("account b window should be untouched by a's reset: %#v", win)
	}

	// Fresh traffic on a shows only its post-reset delta.
	rec("a", 2000, 1900)
	if win := a.promptCacheWindowForAccountLocked("a"); win["inputTokens"].(uint64) != 2000 || win["cachedTokens"].(uint64) != 1900 {
		t.Fatalf("account a post-reset window wrong: %#v", win)
	}

	// A pool-wide reset clears per-account overrides.
	a.resetPromptCacheWindowLocked(now)
	if a.state.PromptCacheResetAtByAccount != nil {
		t.Fatalf("pool-wide reset should clear per-account overrides: %#v", a.state.PromptCacheResetAtByAccount)
	}
	if win := a.promptCacheWindowForAccountLocked("b"); win["inputTokens"].(uint64) != 0 {
		t.Fatalf("account b window should be zero after pool-wide reset: %#v", win)
	}
}

func TestSubSatClampsToZero(t *testing.T) {
	if subSat(5, 8) != 0 {
		t.Fatal("subSat should clamp underflow to 0")
	}
	if subSat(10, 3) != 7 {
		t.Fatal("subSat normal subtraction failed")
	}
}

func TestPublicDashboardAccountIncludesCacheStats(t *testing.T) {
	a := testApp(t, []account{{ID: "acct", Enabled: true, InPool: true}})
	a.state.PromptCache = map[string]promptCacheStat{
		"acct:gpt-test": {AccountID: "acct", ModelID: "gpt-test", RequestCount: 4, InputTokens: 1000, CachedTokens: 750},
	}
	item := a.publicDashboardAccountLocked(a.config.Accounts[0], 0, time.Now().UTC())
	if item["cacheInputTokens"].(uint64) != 1000 || item["cacheCachedTokens"].(uint64) != 750 || item["cacheRequestCount"].(uint64) != 4 {
		t.Fatalf("public dashboard missing cache stats: %#v", item)
	}
}

func TestScopedPromptCacheKeyGroupsByProject(t *testing.T) {
	a := testApp(t, nil)
	a.promptCacheKeyScope = "auto"
	a.promptCacheBuckets = 1
	mk := func(session, project string) routingDecision {
		req := httptest.NewRequest(http.MethodPost, "/v1/responses", nil)
		req.Header.Set("X-Codex-Pool-Session", session)
		req.Header.Set("X-Codex-Pool-Project", project)
		return a.routingDecision(req, map[string]any{"input": "x"}, "gpt-test", "client-key")
	}
	a1 := mk("sess-1", "repo-x")
	a2 := mk("sess-2", "repo-x")
	b := mk("sess-3", "repo-y")
	// Same project, different conversations share one cache key (buckets=1) so
	// they reuse the static prefix, while routing stays per-conversation.
	if a1.PromptCacheKey == "" || a1.PromptCacheKey != a2.PromptCacheKey {
		t.Fatalf("same-project conversations did not share cache key: %q vs %q", a1.PromptCacheKey, a2.PromptCacheKey)
	}
	if a1.StickyKey == a2.StickyKey {
		t.Fatalf("conversations collapsed to a single sticky route: %q", a1.StickyKey)
	}
	if a1.PromptCacheKey == b.PromptCacheKey {
		t.Fatalf("different projects shared a cache key: %q", a1.PromptCacheKey)
	}
	if !strings.HasPrefix(a1.PromptCacheKey, "cp_") || strings.Contains(a1.PromptCacheKey, "repo-x") {
		t.Fatalf("cache key leaked raw data or was malformed: %q", a1.PromptCacheKey)
	}
}

func TestScopedPromptCacheKeyConversationScopeUnchanged(t *testing.T) {
	a := testApp(t, nil) // testApp leaves scope empty == conversation behavior
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", nil)
	req.Header.Set("X-Codex-Pool-Project", "repo-x")
	got := a.routingDecision(req, map[string]any{"input": "x"}, "gpt-test", "client-key").PromptCacheKey
	want := promptCacheKeyHash("gpt-test", "project", "repo-x")
	if got != want {
		t.Fatalf("conversation-scope cache key = %q, want historical %q", got, want)
	}
}

func TestPromptCacheBucketIndex(t *testing.T) {
	if promptCacheBucketIndex("anything", 1) != 0 || promptCacheBucketIndex("anything", 0) != 0 {
		t.Fatal("single/zero bucket must map to index 0")
	}
	for _, size := range []int{4, 16, 256} {
		for i := 0; i < 200; i++ {
			idx := promptCacheBucketIndex(fmt.Sprintf("sticky-%d", i), size)
			if idx < 0 || idx >= size {
				t.Fatalf("bucket index %d out of range for size %d", idx, size)
			}
		}
		if promptCacheBucketIndex("stable", size) != promptCacheBucketIndex("stable", size) {
			t.Fatalf("bucket index not deterministic for size %d", size)
		}
	}
}

func TestPromptCacheKeyScopeFromEnv(t *testing.T) {
	for _, env := range []string{"", "auto", "conversation", "project", "user", "  PROJECT "} {
		t.Setenv("CODEX_POOL_PROMPT_CACHE_KEY_SCOPE", env)
		if _, err := promptCacheKeyScopeFromEnv(); err != nil {
			t.Fatalf("promptCacheKeyScopeFromEnv(%q) error: %v", env, err)
		}
	}
	t.Setenv("CODEX_POOL_PROMPT_CACHE_KEY_SCOPE", "")
	if got, _ := promptCacheKeyScopeFromEnv(); got != "auto" {
		t.Fatalf("default scope = %q, want auto", got)
	}
	t.Setenv("CODEX_POOL_PROMPT_CACHE_KEY_SCOPE", "bogus")
	if _, err := promptCacheKeyScopeFromEnv(); err == nil {
		t.Fatal("expected error for invalid scope")
	}
}

func TestPromptCacheBucketsFromEnv(t *testing.T) {
	t.Setenv("CODEX_POOL_PROMPT_CACHE_BUCKETS", "")
	if got, _ := promptCacheBucketsFromEnv(); got != promptCacheBucketsDefault {
		t.Fatalf("default buckets = %d, want %d", got, promptCacheBucketsDefault)
	}
	t.Setenv("CODEX_POOL_PROMPT_CACHE_BUCKETS", "8")
	if got, _ := promptCacheBucketsFromEnv(); got != 8 {
		t.Fatalf("buckets = %d, want 8", got)
	}
	for _, bad := range []string{"0", "-1", "300", "abc"} {
		t.Setenv("CODEX_POOL_PROMPT_CACHE_BUCKETS", bad)
		if _, err := promptCacheBucketsFromEnv(); err == nil {
			t.Fatalf("expected error for buckets=%q", bad)
		}
	}
}

func TestPromptCacheRetentionFromEnv(t *testing.T) {
	cases := []struct {
		env  string
		want string
	}{
		{"", "24h"},
		{"passthrough", ""},
		{"24h", "24h"},
		{"in_memory", "in_memory"},
		{"  24H ", "24h"},
	}
	for _, tc := range cases {
		t.Setenv("CODEX_POOL_PROMPT_CACHE_RETENTION", tc.env)
		got, err := promptCacheRetentionFromEnv()
		if err != nil {
			t.Fatalf("promptCacheRetentionFromEnv(%q) error: %v", tc.env, err)
		}
		if got != tc.want {
			t.Fatalf("promptCacheRetentionFromEnv(%q) = %q, want %q", tc.env, got, tc.want)
		}
	}
	t.Setenv("CODEX_POOL_PROMPT_CACHE_RETENTION", "forever")
	if _, err := promptCacheRetentionFromEnv(); err == nil {
		t.Fatal("expected error for invalid retention value")
	}
}

func TestCodexQuotaRefreshUpdatesProPlanLimit(t *testing.T) {
	var sawAccountCheckRequest bool
	var sawSubscriptionRequest bool
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/backend-api/wham/usage":
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{
				"plan_type":"pro",
				"rate_limit":{
					"allowed":true,
					"limit_reached":false,
					"primary_window":{"used_percent":10,"limit_window_seconds":18000},
					"secondary_window":{"used_percent":20,"limit_window_seconds":604800}
				}
			}`))
		case "/backend-api/accounts/check/v4-2023-04-27":
			if r.URL.Query().Get("timezone_offset_min") != "" && r.Header.Get("X-OpenAI-Target-Path") == "/backend-api/accounts/check/v4-2023-04-27" {
				sawAccountCheckRequest = true
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"accounts":{"primary":{"account":{"account_id":"acct-pro","account_name":"Personal Pro","plan_type":"pro"},"entitlement":{"expires_at":"2020-01-01T00:00:00Z"}}}}`))
		case "/backend-api/subscriptions":
			if r.URL.Query().Get("account_id") == "acct-pro" && r.Header.Get("X-OpenAI-Target-Path") == "/backend-api/subscriptions" {
				sawSubscriptionRequest = true
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"subscription_plan":"chatgptpro"}`))
		default:
			t.Fatalf("unexpected path %s", r.URL.Path)
		}
	}))
	defer usage.Close()

	a := testApp(t, []account{{ID: "codex-pro", AccountID: "acct-pro", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexBaseURL = usage.URL + "/backend-api"
	home := a.accountCodexHome("codex-pro")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<refresh-token>"}}`, fakeJWT(time.Now().Add(time.Hour)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}

	snapshot, err := a.refreshAccountQuota(context.Background(), "codex-pro")
	if err != nil {
		t.Fatal(err)
	}
	if !sawAccountCheckRequest {
		t.Fatal("quota refresh did not fetch account metadata for Pro plan limit")
	}
	if !sawSubscriptionRequest {
		t.Fatal("quota refresh did not fetch subscription metadata for Pro plan limit")
	}
	if snapshot.PlanType != "pro" || snapshot.PlanLimit != "20x" || snapshot.OrganizationName != "" {
		t.Fatalf("unexpected Pro quota metadata: %#v", snapshot)
	}
	if a.config.Accounts[0].PlanLimit != "20x" || a.config.Accounts[0].Label != "codex-pro" {
		t.Fatalf("account did not store Pro plan limit display: %#v", a.config.Accounts[0])
	}
	request := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", recorder.Code)
	}
	if body := recorder.Body.String(); !strings.Contains(body, "Credential 1") || !strings.Contains(body, "Pro 20x") || strings.Contains(body, `"planLimit":"20x"`) {
		t.Fatalf("public dashboard omitted Pro plan limit: %s", body)
	}
}

func TestSubscriptionMetadataFromAccountCheckUsesExplicitWorkspaceName(t *testing.T) {
	metadata, ok := subscriptionMetadataFromValue(map[string]any{
		"accounts": map[string]any{
			"acct-team": map[string]any{
				"account": map[string]any{
					"account_id":     "acct-team",
					"name":           "Yi-Fan Liou",
					"workspace_name": "markliou",
					"plan_type":      "team",
				},
				"entitlement": map[string]any{"subscription_plan": "chatgptteamplan"},
			},
			"acct-pro": map[string]any{
				"account":     map[string]any{"account_id": "acct-pro", "plan_type": "pro"},
				"entitlement": map[string]any{"subscription_plan": "chatgptpro"},
			},
		},
		"account_ordering": []any{"acct-team"},
	}, "acct-team")
	if !ok {
		t.Fatal("metadata parser did not find account records")
	}
	if metadata.AccountID != "acct-team" || metadata.OrganizationName != "markliou" || metadata.PlanType != "team" {
		t.Fatalf("unexpected team metadata: %#v", metadata)
	}

	metadata, ok = subscriptionMetadataFromValue(map[string]any{
		"accounts": map[string]any{
			"acct-team": map[string]any{
				"account": map[string]any{
					"account_id": "acct-team",
					"name":       "Yi-Fan Liou",
					"plan_type":  "team",
				},
				"entitlement": map[string]any{"subscription_plan": "chatgptteamplan"},
			},
		},
		"account_ordering": []any{"acct-team"},
	}, "acct-team")
	if !ok {
		t.Fatal("metadata parser did not find account records with personal account name")
	}
	if metadata.AccountID != "acct-team" || metadata.OrganizationName != "" || metadata.PlanType != "team" {
		t.Fatalf("personal account name was used as team organization metadata: %#v", metadata)
	}

	metadata, ok = subscriptionMetadataFromValue(map[string]any{
		"accounts": map[string]any{
			"acct-team": map[string]any{
				"account": map[string]any{
					"account_id": "acct-team",
					"name":       "markliou",
					"structure":  "workspace",
					"plan_type":  "team",
				},
				"entitlement": map[string]any{"subscription_plan": "chatgptteamplan"},
			},
		},
		"account_ordering": []any{"acct-team"},
	}, "acct-team")
	if !ok {
		t.Fatal("metadata parser did not find workspace account records")
	}
	if metadata.AccountID != "acct-team" || metadata.OrganizationName != "markliou" || metadata.PlanType != "team" {
		t.Fatalf("workspace account name was not used as team organization metadata: %#v", metadata)
	}

	metadata, ok = subscriptionMetadataFromValue(map[string]any{
		"accounts": map[string]any{
			"acct-team": map[string]any{
				"account":     map[string]any{"account_id": "acct-team", "name": "markliou", "plan_type": "team"},
				"entitlement": map[string]any{"subscription_plan": "chatgptteamplan"},
			},
			"acct-pro": map[string]any{
				"account":     map[string]any{"account_id": "acct-pro", "plan_type": "pro"},
				"entitlement": map[string]any{"subscription_plan": "chatgptpro"},
			},
		},
	}, "acct-pro")
	if !ok {
		t.Fatal("metadata parser did not find preferred Pro record")
	}
	if metadata.AccountID != "acct-pro" || metadata.PlanType != "pro" || metadata.PlanLimit != "20x" {
		t.Fatalf("unexpected Pro metadata: %#v", metadata)
	}
}

func TestCodexQuotaErrorDoesNotPersistUpstreamBody(t *testing.T) {
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = w.Write([]byte(`{"error":{"code":"invalid_token","message":"secret-body-token"}}`))
	}))
	defer usage.Close()

	a := testApp(t, []account{{ID: "codex-quota-error", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexBaseURL = usage.URL + "/backend-api"
	home := a.accountCodexHome("codex-quota-error")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<refresh-token>","account_id":"acct-chatgpt"}}`, fakeJWT(time.Now().Add(time.Hour)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}

	_, err := a.refreshAccountQuota(context.Background(), "codex-quota-error")
	if err == nil {
		t.Fatal("quota refresh with upstream 401 returned nil error")
	}
	if strings.Contains(err.Error(), "secret-body-token") {
		t.Fatalf("quota error exposed upstream body: %s", err)
	}
	snapshot := a.state.Quotas["codex-quota-error"]
	if snapshot.QuotaError == nil || snapshot.QuotaError.Code != "invalid_token" {
		t.Fatalf("quota error was not persisted with code: %#v", snapshot)
	}
	if strings.Contains(snapshot.QuotaError.Message, "secret-body-token") {
		t.Fatalf("persisted quota error exposed upstream body: %#v", snapshot.QuotaError)
	}
	status, reason := a.accountStatusLocked(a.config.Accounts[0], time.Now().UTC())
	if status != "error" || !strings.Contains(reason, "invalid_token") {
		t.Fatalf("quota refresh failure status = %q/%q, want error with sanitized code", status, reason)
	}
	request := httptest.NewRequest(http.MethodGet, "/admin/api/public-dashboard", nil)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusOK {
		t.Fatalf("public dashboard returned %d", recorder.Code)
	}
	publicBody := recorder.Body.String()
	if strings.Contains(publicBody, "invalid_token") || strings.Contains(publicBody, "secret-body-token") || strings.Contains(publicBody, "acct-chatgpt") {
		t.Fatalf("public dashboard exposed quota failure internals: %s", publicBody)
	}
}

func TestQuotaFromUsageTreatsReachedLimitWithoutWindowsAsExhausted(t *testing.T) {
	reached := true
	quota := quotaFromUsage(codexUsageResponse{
		RateLimit: &codexRateLimitInfo{LimitReached: &reached},
	}, time.Now().UTC())
	if !quota.Hourly.Present || quota.Hourly.Percentage != 0 {
		t.Fatalf("limit-reached usage was not normalized to exhausted quota: %#v", quota)
	}
	if remainingQuotaHint(quota) != 0 {
		t.Fatalf("limit-reached quota remaining hint = %d, want 0", remainingQuotaHint(quota))
	}
}

func TestExtractUpstreamErrorCodeRejectsUnsafeValues(t *testing.T) {
	if code := extractUpstreamErrorCode([]byte(`{"error":{"code":"invalid_token"}}`)); code != "invalid_token" {
		t.Fatalf("safe error code = %q, want invalid_token", code)
	}
	if code := extractUpstreamErrorCode([]byte(`{"error":{"code":"secret token value"}}`)); code != "" {
		t.Fatalf("unsafe error code was retained: %q", code)
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

func TestConcurrentCodexAuthRefreshUsesOneTokenRequest(t *testing.T) {
	newAccess := fakeJWT(time.Now().Add(time.Hour))
	var refreshMu sync.Mutex
	refreshCalls := 0
	refresh := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		refreshMu.Lock()
		refreshCalls++
		refreshMu.Unlock()
		time.Sleep(50 * time.Millisecond)
		w.Header().Set("Content-Type", "application/json")
		_, _ = fmt.Fprintf(w, `{"access_token":%q,"refresh_token":"<new-refresh-token>"}`, newAccess)
	}))
	defer refresh.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", refresh.URL)

	a := testApp(t, []account{{ID: "codex-lock", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	home := a.accountCodexHome("codex-lock")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"access_token":%q,"refresh_token":"<old-refresh-token>"}}`, fakeJWT(time.Now().Add(-time.Minute)))
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}

	start := make(chan struct{})
	errs := make(chan error, 2)
	var wg sync.WaitGroup
	for range 2 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			auth, err := a.refreshedCodexAuth(a.config.Accounts[0])
			if err == nil && auth.AccessToken != newAccess {
				err = fmt.Errorf("unexpected access token %q", auth.AccessToken)
			}
			errs <- err
		}()
	}
	close(start)
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatal(err)
		}
	}
	refreshMu.Lock()
	defer refreshMu.Unlock()
	if refreshCalls != 1 {
		t.Fatalf("refresh endpoint was called %d times, want 1", refreshCalls)
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

func TestUpstreamFailureAfterSelectionDoesNotReturnNoEligible503(t *testing.T) {
	proHits := 0
	pro := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		proHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer pro.Close()

	a := testApp(t, []account{
		{ID: "team-empty", AuthType: "codex_device_auth", AccountID: "upstream-team", PlanType: "team", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "http://127.0.0.1:1"},
		{ID: "pro-only", AuthType: "codex_device_auth", AccountID: "upstream-pro", PlanType: "pro", Enabled: true, InPool: true, Priority: 90, UpstreamBaseURL: pro.URL},
	})
	a.preserveProQuota = true
	a.state.Quotas["team-empty"] = quotaSnapshot{AccountID: "team-empty", PlanType: "team", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 0, Present: true}, Weekly: quotaWindow{Percentage: 0, Present: true}}}
	a.state.Quotas["pro-only"] = quotaSnapshot{AccountID: "pro-only", PlanType: "pro", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 96, Present: true}, Weekly: quotaWindow{Percentage: 98, Present: true}}}
	writeCodexDeviceAuth(t, a, "team-empty", "upstream-team", "team@example.test")
	writeCodexDeviceAuth(t, a, "pro-only", "upstream-pro", "pro@example.test")

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "single-pro-5xx")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusBadGateway {
		t.Fatalf("single eligible upstream failure returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if strings.Contains(recorder.Body.String(), "no eligible account") || strings.Contains(recorder.Body.String(), "all_accounts_cooling_down") {
		t.Fatalf("upstream failure was reported as pool exhaustion: %s", recorder.Body.String())
	}
	if proHits != 1 {
		t.Fatalf("pro hit count = %d, want 1", proHits)
	}
	if cooldowns := activeCooldowns(a.state.Cooldowns["pro-only"], time.Now().UTC()); len(cooldowns) != 0 {
		t.Fatalf("single eligible upstream 5xx without Retry-After should not create active cooldown: %#v", cooldowns)
	}
	if reason := a.state.Health["pro-only"].LastFailureReason; reason != "upstream_5xx" {
		t.Fatalf("pro failure reason = %q, want upstream_5xx", reason)
	}
}

func TestTransientUpstream5xxPreservesStickyAccount(t *testing.T) {
	stickyHits := 0
	sticky := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		stickyHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer sticky.Close()
	backupHits := 0
	backup := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		backupHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_backup","object":"response","output":[]}`))
	}))
	defer backup.Close()

	a := testApp(t, []account{
		{ID: "sticky", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: sticky.URL, UpstreamAPIKey: "sticky"},
		{ID: "backup", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: backup.URL, UpstreamAPIKey: "backup"},
	})
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:transient-5xx"] = stickySession{Key: "gpt-test:transient-5xx", ModelID: "gpt-test", AccountID: "sticky", CreatedAt: now.Add(-time.Minute), LastSuccessAt: now.Add(-time.Minute), ExpiresAt: now.Add(time.Hour)}
	a.state.Health["sticky"] = accountHealth{ConsecutiveFailure: upstream5xxFailoverAfter - 1, LastFailureReason: "upstream_5xx", LastFailureAt: now.Add(-upstream5xxFailureWindow - time.Second)}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "transient-5xx")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusBadGateway {
		t.Fatalf("transient 5xx returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if stickyHits != 1 || backupHits != 0 {
		t.Fatalf("transient 5xx hits = sticky:%d backup:%d, want 1/0", stickyHits, backupHits)
	}
	if session := a.state.StickySessions["gpt-test:transient-5xx"]; session.AccountID != "sticky" {
		t.Fatalf("transient 5xx changed sticky account: %#v", session)
	}
	if cooldowns := activeCooldowns(a.state.Cooldowns["sticky"], time.Now().UTC()); len(cooldowns) != 0 {
		t.Fatalf("transient 5xx should not cool down sticky account: %#v", cooldowns)
	}
	if failures := a.state.Health["sticky"].ConsecutiveFailure; failures != 1 {
		t.Fatalf("stale 5xx failures were not reset: got %d", failures)
	}
}

func TestRepeatedUpstream5xxCanFailoverAfterThreshold(t *testing.T) {
	stickyHits := 0
	sticky := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		stickyHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer sticky.Close()
	backupHits := 0
	backup := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		backupHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_backup","object":"response","output":[]}`))
	}))
	defer backup.Close()

	a := testApp(t, []account{
		{ID: "sticky", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: sticky.URL, UpstreamAPIKey: "sticky"},
		{ID: "backup", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: backup.URL, UpstreamAPIKey: "backup"},
	})
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:repeated-5xx"] = stickySession{Key: "gpt-test:repeated-5xx", ModelID: "gpt-test", AccountID: "sticky", CreatedAt: now.Add(-time.Minute), LastSuccessAt: now.Add(-time.Minute), ExpiresAt: now.Add(time.Hour)}

	for attempt := 1; attempt <= upstream5xxFailoverAfter; attempt++ {
		req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
		req.Header.Set("Authorization", "Bearer client-key")
		req.Header.Set("X-Codex-Pool-Session", "repeated-5xx")
		recorder := httptest.NewRecorder()
		a.publicMux().ServeHTTP(recorder, req)
		if attempt < upstream5xxFailoverAfter {
			if recorder.Code != http.StatusBadGateway {
				t.Fatalf("attempt %d returned %d: %s", attempt, recorder.Code, recorder.Body.String())
			}
			continue
		}
		if recorder.Code != http.StatusOK {
			t.Fatalf("threshold attempt returned %d: %s", recorder.Code, recorder.Body.String())
		}
	}
	if stickyHits != upstream5xxFailoverAfter || backupHits != 1 {
		t.Fatalf("repeated 5xx hits = sticky:%d backup:%d", stickyHits, backupHits)
	}
	if session := a.state.StickySessions["gpt-test:repeated-5xx"]; session.AccountID != "backup" || session.FailoverFrom != "sticky" {
		t.Fatalf("repeated 5xx did not fail over sticky session: %#v", session)
	}
	if cooldowns := activeCooldowns(a.state.Cooldowns["sticky"], time.Now().UTC()); len(cooldowns) != 1 {
		t.Fatalf("repeated 5xx should cool down failed sticky account: %#v", cooldowns)
	}
}

func TestStickySessionWithExhaustedQuotaSnapshotReselects(t *testing.T) {
	firstHits := 0
	first := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		firstHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer first.Close()
	secondHits := 0
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		secondHits++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_ok","object":"response","output":[]}`))
	}))
	defer second.Close()
	a := testApp(t, []account{
		{ID: "first", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: first.URL, UpstreamAPIKey: "one"},
		{ID: "second", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: second.URL, UpstreamAPIKey: "two"},
	})
	now := time.Now().UTC()
	a.state.StickySessions["gpt-test:quota-session"] = stickySession{Key: "gpt-test:quota-session", ModelID: "gpt-test", AccountID: "first", CreatedAt: now.Add(-time.Minute), LastSuccessAt: now.Add(-time.Minute), ExpiresAt: now.Add(time.Hour)}
	a.state.Quotas["first"] = quotaSnapshot{AccountID: "first", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 0, Present: true}}}
	a.state.Quotas["second"] = quotaSnapshot{AccountID: "second", Quota: &accountQuota{Hourly: quotaWindow{Percentage: 80, Present: true}}}

	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "quota-session")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("quota snapshot failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if firstHits != 0 {
		t.Fatalf("exhausted sticky account was called %d times", firstHits)
	}
	if secondHits != 1 {
		t.Fatalf("available account was called %d times", secondHits)
	}
	if session := a.state.StickySessions["gpt-test:quota-session"]; session.AccountID != "second" || session.FailoverFrom != "first" {
		t.Fatalf("sticky session was not rebound to available account: %#v", session)
	}
}

func TestFailoverTriesAllConfiguredAccounts(t *testing.T) {
	hits := map[string]int{}
	rateLimited := func(id string) *httptest.Server {
		return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
			hits[id]++
			w.Header().Set("Retry-After", "1")
			w.WriteHeader(http.StatusTooManyRequests)
		}))
	}
	first := rateLimited("first")
	defer first.Close()
	second := rateLimited("second")
	defer second.Close()
	third := rateLimited("third")
	defer third.Close()
	fourth := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits["fourth"]++
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_ok","object":"response","output":[]}`))
	}))
	defer fourth.Close()
	a := testApp(t, []account{
		{ID: "first", Enabled: true, InPool: true, Priority: 400, UpstreamBaseURL: first.URL, UpstreamAPIKey: "one"},
		{ID: "second", Enabled: true, InPool: true, Priority: 300, UpstreamBaseURL: second.URL, UpstreamAPIKey: "two"},
		{ID: "third", Enabled: true, InPool: true, Priority: 200, UpstreamBaseURL: third.URL, UpstreamAPIKey: "three"},
		{ID: "fourth", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: fourth.URL, UpstreamAPIKey: "four"},
	})
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("X-Codex-Pool-Session", "all-accounts")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("failover returned %d: %s", recorder.Code, recorder.Body.String())
	}
	for _, id := range []string{"first", "second", "third", "fourth"} {
		if hits[id] != 1 {
			t.Fatalf("account %s hit count = %d, want 1 (all hits: %#v)", id, hits[id], hits)
		}
	}
	if session := a.state.StickySessions["gpt-test:all-accounts"]; session.AccountID != "fourth" {
		t.Fatalf("expected sticky failover to fourth, got %#v", session)
	}
}

func TestZeroQuotaAccountIsNotSelected(t *testing.T) {
	emptyQuota := 0
	firstHits := 0
	first := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		firstHits++
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer first.Close()
	second := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_ok","object":"response","output":[]}`))
	}))
	defer second.Close()
	a := testApp(t, []account{
		{ID: "empty", Enabled: true, InPool: true, Priority: 100, RemainingQuota: &emptyQuota, UpstreamBaseURL: first.URL, UpstreamAPIKey: "empty"},
		{ID: "ready", Enabled: true, InPool: true, Priority: 10, UpstreamBaseURL: second.URL, UpstreamAPIKey: "ready"},
	})
	req := httptest.NewRequest(http.MethodPost, "/v1/responses", strings.NewReader(`{"model":"gpt-test","input":"hello"}`))
	req.Header.Set("Authorization", "Bearer client-key")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("proxy returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if firstHits != 0 {
		t.Fatalf("zero-quota account was selected %d times", firstHits)
	}
}

func fakeJWT(exp time.Time) string {
	return fakeJWTClaims(map[string]any{"exp": exp.Unix()})
}

func writeCodexDeviceAuth(t *testing.T, a *app, accountID, upstreamAccountID, email string) {
	t.Helper()
	home := a.accountCodexHome(accountID)
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	idToken := fakeJWTClaims(map[string]any{
		"email": email,
		"https://api.openai.com/auth": map[string]any{
			"chatgpt_account_id":   upstreamAccountID,
			"chatgpt_account_name": "Workspace",
			"chatgpt_plan_type":    "team",
		},
	})
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"id_token":%q,"access_token":%q,"refresh_token":"<refresh-token>","account_id":%q}}`, idToken, fakeJWT(time.Now().Add(time.Hour)), upstreamAccountID)
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
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
	request := httptest.NewRequest(http.MethodPost, "/admin/api/accounts", strings.NewReader(`{"email":"User@Example.Test","planType":"plus","allowedModels":["gpt-5.5"],"codexHome":"/tmp/evil/.codex"}`))
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
	for _, expected := range []string{`"id":"acct-credential"`, `"email":""`, `"displayName":"acct-credential"`} {
		if !strings.Contains(recorder.Body.String(), expected) {
			t.Fatalf("admin account create response missing %s: %s", expected, recorder.Body.String())
		}
	}
	if strings.Contains(recorder.Body.String(), "user@example.test") {
		t.Fatalf("admin account create response exposed full email: %s", recorder.Body.String())
	}
	if strings.Contains(recorder.Body.String(), `"planType":"plus"`) {
		t.Fatalf("admin account create accepted caller-supplied plan type: %s", recorder.Body.String())
	}
	if strings.Contains(recorder.Body.String(), "/tmp/evil") {
		t.Fatalf("admin account create accepted caller-supplied codexHome: %s", recorder.Body.String())
	}
	if strings.Contains(recorder.Body.String(), `"allowedModels":["`) {
		t.Fatalf("admin account create unexpectedly stored user-selected models: %s", recorder.Body.String())
	}

	secondRequest := httptest.NewRequest(http.MethodPost, "/admin/api/accounts", strings.NewReader(`{"email":"User@Example.Test","planType":"pro"}`))
	secondRequest.AddCookie(sessionCookie)
	secondRequest.AddCookie(csrfCookie)
	secondRequest.Header.Set("X-CSRF-Token", response.CSRFToken)
	secondRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(secondRecorder, secondRequest)
	if secondRecorder.Code != http.StatusCreated {
		t.Fatalf("second admin account create returned %d: %s", secondRecorder.Code, secondRecorder.Body.String())
	}
	if !strings.Contains(secondRecorder.Body.String(), `"id":"acct-credential-2"`) || !strings.Contains(secondRecorder.Body.String(), `"displayName":"acct-credential-2"`) {
		t.Fatalf("second device-auth account did not use an independent credential id: %s", secondRecorder.Body.String())
	}
	if strings.Contains(secondRecorder.Body.String(), "user@example.test") || strings.Contains(secondRecorder.Body.String(), "us***@example.test") || strings.Contains(secondRecorder.Body.String(), `"planType":"pro"`) {
		t.Fatalf("second device-auth account used caller-supplied identity metadata: %s", secondRecorder.Body.String())
	}

	providerRequest := httptest.NewRequest(http.MethodPost, "/admin/api/accounts", strings.NewReader(`{"authType":"provider_api_key","email":"Provider@Example.Test","planType":"team","upstreamBaseUrl":"https://upstream.example.test","upstreamApiKey":"provider-secret"}`))
	providerRequest.AddCookie(sessionCookie)
	providerRequest.AddCookie(csrfCookie)
	providerRequest.Header.Set("X-CSRF-Token", response.CSRFToken)
	providerRecorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(providerRecorder, providerRequest)
	if providerRecorder.Code != http.StatusCreated {
		t.Fatalf("provider account create returned %d: %s", providerRecorder.Code, providerRecorder.Body.String())
	}
	providerBody := providerRecorder.Body.String()
	for _, expected := range []string{`"id":"acct-provider"`, `"displayName":"pr***er@example.test"`, `"email":"pr***er@example.test"`} {
		if !strings.Contains(providerBody, expected) {
			t.Fatalf("provider account create response missing %s: %s", expected, providerBody)
		}
	}
	if strings.Contains(providerBody, "provider@example.test") || strings.Contains(providerBody, "provider-secret") || strings.Contains(providerBody, "provider-team") {
		t.Fatalf("provider account create response used sensitive metadata as identity: %s", providerBody)
	}
}
