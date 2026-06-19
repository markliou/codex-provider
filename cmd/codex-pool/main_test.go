package main

import (
	"encoding/json"
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
		sessionKey: []byte("01234567890123456789012345678901"), client: &http.Client{Timeout: time.Second},
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
	request := httptest.NewRequest(http.MethodPost, "/admin/api/accounts", strings.NewReader(`{"id":"provider","upstreamBaseUrl":"https://example.test/v1"}`))
	request.AddCookie(sessionCookie)
	request.AddCookie(csrfCookie)
	request.Header.Set("X-CSRF-Token", response.CSRFToken)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	if recorder.Code != http.StatusCreated {
		t.Fatalf("admin account create returned %d: %s", recorder.Code, recorder.Body.String())
	}
}
