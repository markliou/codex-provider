package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// A quota prime for a Codex account runs the Codex CLI against a throwaway
// home that can never rotate the pool's refresh token, and leaves the
// account's own credential untouched.
func TestQuotaPrimeRunsCodexWithoutTheRefreshToken(t *testing.T) {
	a := testApp(t, []account{{ID: "device", AccountID: "acct-device", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	writeCodexDeviceAuth(t, a, "device", "acct-device", "device@example.test")
	authPath := filepath.Join(a.accountCodexHome("device"), "auth.json")
	before, err := os.ReadFile(authPath)
	if err != nil {
		t.Fatal(err)
	}
	current, err := a.codexAuth(a.config.Accounts[0])
	if err != nil {
		t.Fatal(err)
	}
	var seenHome string
	a.codexPrimeRunner = func(_ context.Context, home, workdir, model string) error {
		seenHome = home
		raw, err := os.ReadFile(filepath.Join(home, "auth.json"))
		if err != nil {
			t.Fatal(err)
		}
		var file struct {
			AuthMode    string `json:"auth_mode"`
			LastRefresh string `json:"last_refresh"`
			Tokens      struct {
				AccessToken  string `json:"access_token"`
				RefreshToken string `json:"refresh_token"`
				AccountID    string `json:"account_id"`
			} `json:"tokens"`
		}
		if err := json.Unmarshal(raw, &file); err != nil {
			t.Fatal(err)
		}
		if file.Tokens.RefreshToken != "" {
			t.Fatal("the prime's Codex must have no refresh token: refreshing would revoke the pool's copy")
		}
		if file.AuthMode != "chatgpt" || file.Tokens.AccessToken != current.AccessToken || file.Tokens.AccountID != "acct-device" || file.LastRefresh == "" {
			t.Fatalf("prime credential = %s", raw)
		}
		config, _ := os.ReadFile(filepath.Join(home, "config.toml"))
		if !strings.Contains(string(config), `model = "gpt-test"`) || !strings.Contains(string(config), `cli_auth_credentials_store = "file"`) {
			t.Fatalf("prime config = %s", config)
		}
		if model != "gpt-test" || workdir == "" {
			t.Fatalf("prime model %q workdir %q", model, workdir)
		}
		return nil
	}
	if err := a.sendQuotaPrimeRequest(context.Background(), a.config.Accounts[0], "gpt-test"); err != nil {
		t.Fatal(err)
	}
	if seenHome == "" {
		t.Fatal("a Codex account was not primed by Codex")
	}
	if _, err := os.Stat(seenHome); !os.IsNotExist(err) {
		t.Fatalf("the prime's home must be removed afterwards: %v", err)
	}
	after, _ := os.ReadFile(authPath)
	if string(after) != string(before) {
		t.Fatal("the prime changed the account's own credential")
	}
}

func TestCodexChildEnvironmentCarriesNoPoolSecrets(t *testing.T) {
	t.Setenv("CODEX_POOL_API_KEY", "<pool-key>")
	t.Setenv("CODEX_POOL_ADMIN_PASSWORD_HASH", "<admin-hash>")
	t.Setenv("SSL_CERT_FILE", "/certs/ca.pem")
	env := strings.Join(codexChildEnvironment("/tmp/prime/home"), "\n")
	if strings.Contains(env, "CODEX_POOL_") {
		t.Fatalf("Codex child saw pool secrets: %s", env)
	}
	if !strings.Contains(env, "CODEX_HOME=/tmp/prime/home") || !strings.Contains(env, "SSL_CERT_FILE=/certs/ca.pem") {
		t.Fatalf("Codex child environment = %s", env)
	}
}

// The pool's own calls carry the identity of the Codex that uses the account,
// Codex's header set, and nothing a browser would send.
func TestPoolOwnCallsPresentTheAccountsCodex(t *testing.T) {
	var refreshHeaders, usageHeaders http.Header
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/oauth/token":
			refreshHeaders = r.Header.Clone()
			_, _ = fmt.Fprintf(w, `{"access_token":%q,"refresh_token":"<rotated>"}`, fakeJWT(time.Now().Add(time.Hour)))
		case "/backend-api/wham/usage":
			usageHeaders = r.Header.Clone()
			_, _ = w.Write([]byte(`{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":5,"limit_window_seconds":18000}}}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer upstream.Close()
	t.Setenv("CODEX_REFRESH_TOKEN_URL_OVERRIDE", upstream.URL+"/oauth/token")
	a := testApp(t, []account{{ID: "device", AccountID: "acct-device", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	a.codexBaseURL = upstream.URL + "/backend-api"
	writeCodexDeviceAuth(t, a, "device", "acct-device", "device@example.test")

	client := http.Header{}
	client.Set("User-Agent", "codex-tui/0.163.0 (Mac OS 26.0.0; arm64) iTerm.app/3.6 (Codex_Desktop; 0.163.0)")
	client.Set("Originator", "Codex_Desktop")
	a.noteClientAgent("device", client)

	if _, err := a.requestCodexTokenRefreshContext(context.Background(), "device", "<refresh>"); err != nil {
		t.Fatal(err)
	}
	// A native Codex's refresh carries its default client's agent, without
	// the suffix its conversation requests add.
	if refreshHeaders.Get("User-Agent") != "codex-tui/0.163.0 (Mac OS 26.0.0; arm64) iTerm.app/3.6" || refreshHeaders.Get("Originator") != "Codex_Desktop" || refreshHeaders.Get("Content-Type") != "application/json" {
		t.Fatalf("refresh headers = %v", refreshHeaders)
	}
	if _, err := a.refreshAccountQuota(context.Background(), "device"); err != nil {
		t.Fatal(err)
	}
	if usageHeaders.Get("User-Agent") != client.Get("User-Agent") || usageHeaders.Get("ChatGPT-Account-Id") == "" || !strings.HasPrefix(usageHeaders.Get("Authorization"), "Bearer ") {
		t.Fatalf("usage headers = %v", usageHeaders)
	}
	for _, name := range []string{"Origin", "Referer", "Sec-Fetch-Mode", "Originator"} {
		if usageHeaders.Get(name) != "" {
			t.Fatalf("Codex's backend client sends no %s, the pool did: %v", name, usageHeaders)
		}
	}

	// A foreign client never becomes the identity of the pool's own calls.
	foreign := http.Header{}
	foreign.Set("User-Agent", "Python-urllib/3.12")
	a.noteClientAgent("device", foreign)
	if got := a.clientAgentFor("device").UserAgent; got != client.Get("User-Agent") {
		t.Fatalf("agent after a foreign request = %q", got)
	}
	// An account no Codex has used takes the last Codex seen.
	if got := a.clientAgentFor("unused").UserAgent; got != client.Get("User-Agent") {
		t.Fatalf("agent for an unused account = %q", got)
	}
}

func TestCodexJSONMatchesSerde(t *testing.T) {
	body, err := codexJSON(map[string]string{"refresh_token": "<a&b>", "client_id": "app"})
	if err != nil || string(body) != `{"client_id":"app","refresh_token":"<a&b>"}` {
		t.Fatalf("codexJSON = %s, %v", body, err)
	}
	if got := codexDefaultClientUserAgent("codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown (codex_exec; 0.163.0)"); got != "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown" {
		t.Fatalf("default client agent = %q", got)
	}
	if got := codexDefaultClientUserAgent("codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown"); got != "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown" {
		t.Fatalf("an agent without a suffix must stay whole: %q", got)
	}
}

// Through codex-upstream the pool's own calls name their native header order,
// since the hop to codex-upstream cannot carry one.
func TestPoolOwnCallsNameTheirHeaderOrder(t *testing.T) {
	var gotOrder, gotURL string
	transport := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotOrder = r.Header.Get("X-Codex-Upstream-Header-Order")
		gotURL = r.Header.Get("X-Codex-Upstream-Url")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"plan_type":"plus","rate_limit":{}}`))
	}))
	defer transport.Close()
	a := testApp(t, []account{{ID: "device", AccountID: "acct-device", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	a.codexBaseURL = "https://chatgpt.example.test/backend-api"
	a.codexTransportURL = transport.URL
	writeCodexDeviceAuth(t, a, "device", "acct-device", "device@example.test")
	_, _ = a.refreshAccountQuota(context.Background(), "device")
	if gotURL != "https://chatgpt.example.test/backend-api/wham/usage" || gotOrder != codexBackendHeaderOrder {
		t.Fatalf("usage hop url %q order %q", gotURL, gotOrder)
	}
}

// Behind a reverse proxy the login lockout must count each real client, and a
// forwarded address must be believed only from the proxy.
func TestAdminLoginKeyTrustsOnlyTheConfiguredProxy(t *testing.T) {
	t.Setenv("CODEX_POOL_TRUSTED_PROXIES", "172.18.0.0/16")
	if err := applyTrustedProxiesEnv(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { trustedProxyNetworks = nil })
	request := func(remote, forwarded string) *http.Request {
		r := httptest.NewRequest(http.MethodPost, "/admin/api/login", nil)
		r.RemoteAddr = remote
		if forwarded != "" {
			r.Header.Set("X-Forwarded-For", forwarded)
		}
		return r
	}
	if got := adminLoginKey(request("172.18.0.5:41000", "203.0.113.9")); got != "203.0.113.9" {
		t.Fatalf("behind the proxy the key = %q, want the client", got)
	}
	if got := adminLoginKey(request("172.18.0.5:41000", "198.51.100.1, 203.0.113.9")); got != "203.0.113.9" {
		t.Fatalf("a client-supplied hop must not win over the proxy's: %q", got)
	}
	if got := adminLoginKey(request("203.0.113.50:5000", "10.0.0.1")); got != "203.0.113.50" {
		t.Fatalf("a direct client's own forwarded header must be ignored: %q", got)
	}
	t.Setenv("CODEX_POOL_TRUSTED_PROXIES", "not-a-network")
	if err := applyTrustedProxiesEnv(); err == nil {
		t.Fatal("an invalid trusted proxy must stop startup")
	}
}
