package main

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

func writeTestCodexAuthTokens(t *testing.T, a *app, accountID, accessToken, refreshToken string) {
	t.Helper()
	home := a.accountCodexHome(accountID)
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	authJSON := fmt.Sprintf(`{"auth_mode":"chatgpt","tokens":{"id_token":"","access_token":%q,"refresh_token":%q}}`, accessToken, refreshToken)
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte(authJSON), 0o600); err != nil {
		t.Fatal(err)
	}
}

// An account whose access token upstream revoked is polled once, then left
// alone until it logs in again; the new login is polled.
func TestRevokedCredentialStopsQuotaPollingUntilLoginChangesIt(t *testing.T) {
	var usageCalls atomic.Int32
	revoked := atomic.Bool{}
	revoked.Store(true)
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		usageCalls.Add(1)
		w.Header().Set("Content-Type", "application/json")
		if revoked.Load() {
			// The code is the one upstream answered for a deactivated account
			// (production log, 2026-10-21); Codex has no fixture for it.
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":{"code":"token_revoked"}}`))
			return
		}
		_, _ = w.Write([]byte(`{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000}}}`))
	}))
	defer usage.Close()
	a := testApp(t, []account{{ID: "gone", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("gone")
	a.codexBaseURL = usage.URL + "/backend-api"
	writeTestCodexAuth(t, a, "gone")

	a.refreshAllCodexQuotas(context.Background())
	first := usageCalls.Load()
	if first == 0 {
		t.Fatal("the first poll did not ask upstream")
	}
	if !a.credentialRefused(a.config.Accounts[0]) {
		t.Fatal("a 401 token_revoked was not recorded as a refused credential")
	}
	for i := 0; i < 3; i++ {
		a.refreshAllCodexQuotas(context.Background())
	}
	if got := usageCalls.Load(); got != first {
		t.Fatalf("polls after the refusal asked upstream %d more times", got-first)
	}

	// Someone logs in again: new tokens, and the next poll asks.
	revoked.Store(false)
	writeTestCodexAuthTokens(t, a, "gone", "<new-access-token>", "<new-refresh-token>")
	a.refreshAllCodexQuotas(context.Background())
	if usageCalls.Load() == first {
		t.Fatal("a new login was not polled")
	}
	if a.credentialRefused(a.config.Accounts[0]) {
		t.Fatal("the refusal outlived the login that replaced its tokens")
	}
}

// A failure that does not refuse the credential (an outage) keeps polling.
func TestTransientQuotaFailureKeepsPolling(t *testing.T) {
	var usageCalls atomic.Int32
	usage := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		usageCalls.Add(1)
		w.WriteHeader(http.StatusBadGateway)
	}))
	defer usage.Close()
	a := testApp(t, []account{{ID: "busy", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("busy")
	a.codexBaseURL = usage.URL + "/backend-api"
	writeTestCodexAuth(t, a, "busy")
	a.refreshAllCodexQuotas(context.Background())
	a.refreshAllCodexQuotas(context.Background())
	if usageCalls.Load() < 2 {
		t.Fatalf("a 502 stopped polling: %d calls", usageCalls.Load())
	}
}

// A refused token refresh is not sent again with the same refresh token, as
// Codex's cached permanent_refresh_failure fails fast without network.
func TestRefusedTokenRefreshFailsFastUntilTokensChange(t *testing.T) {
	var refreshCalls atomic.Int32
	refresh := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		refreshCalls.Add(1)
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusUnauthorized)
		// app-server tests/suite/v2/thread_start.rs, verbatim.
		_, _ = w.Write([]byte(`{"error": { "code": "refresh_token_invalidated" }}`))
	}))
	defer refresh.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", refresh.URL)
	a := testApp(t, []account{{ID: "stale", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("stale")
	expiring := fakeJWT(time.Now().Add(time.Minute))
	writeTestCodexAuthTokens(t, a, "stale", expiring, "<refresh-token>")
	item := a.config.Accounts[0]

	for i := 0; i < 3; i++ {
		err := a.refreshCodexAuthIfNeededContext(context.Background(), item)
		if !errors.Is(err, errAccountAuthFailed) || !errors.Is(err, errCodexCredentialRefused) {
			t.Fatalf("refresh %d: %v", i, err)
		}
	}
	if got := refreshCalls.Load(); got != 1 {
		t.Fatalf("a refused refresh was sent %d times", got)
	}
	if !a.credentialRefused(item) {
		t.Fatal("the refused refresh does not stop the quota poll")
	}

	writeTestCodexAuthTokens(t, a, "stale", fakeJWT(time.Now().Add(2*time.Minute)), "<new-refresh-token>")
	_ = a.refreshCodexAuthIfNeededContext(context.Background(), item)
	if got := refreshCalls.Load(); got != 2 {
		t.Fatalf("new tokens were not refreshed: %d calls", got)
	}
}

// Codex's verdict on a failed refresh (login auth/manager.rs
// classify_refresh_token_failure); the bodies come from login
// oauth/error_tests.rs and app-server tests/suite/v2/thread_start.rs.
func TestCodexRefreshFailureIsPermanentAsCodexDecides(t *testing.T) {
	for _, tc := range []struct {
		status int
		body   string
		want   bool
	}{
		{400, `{"code":"refresh_token_expired"}`, true},
		{401, `{"error": { "code": "refresh_token_invalidated" }}`, true},
		{400, `{"error":"invalid_grant","error_description":"refresh token expired"}`, true},
		{401, `service unavailable`, true},
		{400, `{"error":"temporarily_unavailable"}`, false},
		{400, `{"error":{"code":"proxy_auth_required","message":"proxy authentication required"}}`, false},
		{503, `service unavailable`, false},
		{500, `  `, false},
	} {
		if got := codexRefreshFailureIsPermanent(tc.status, []byte(tc.body)); got != tc.want {
			t.Fatalf("%d %s: permanent=%v, want %v", tc.status, tc.body, got, tc.want)
		}
	}
}
