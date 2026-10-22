package main

import (
	"bytes"
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
		// The prime's Codex presents the account's installation id, not one
		// made up for the throwaway home.
		seenID, _ := os.ReadFile(filepath.Join(home, codexInstallationIDFile))
		if want, _ := a.accountInstallationID(a.config.Accounts[0]); string(seenID) != want || want == "" {
			t.Fatalf("prime installation id = %q, want the account's %q", seenID, want)
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

// Through codex-upstream the pool's own calls are named, not written: Go sends
// /own the call and its credential, and codex-upstream composes the request.
func TestPoolOwnCallsGoToCodexUpstreamAsCalls(t *testing.T) {
	var got codexOwnCall
	var gotPath, gotIdentity string
	transport := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		gotIdentity = r.Header.Get("X-Codex-Upstream-Identity")
		_ = json.NewDecoder(r.Body).Decode(&got)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"plan_type":"plus","rate_limit":{}}`))
	}))
	defer transport.Close()
	a := testApp(t, []account{{ID: "device", AccountID: "acct-device", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	a.codexBaseURL = "https://chatgpt.example.test/backend-api"
	a.codexTransportURL = transport.URL
	writeCodexDeviceAuth(t, a, "device", "acct-device", "device@example.test")
	_, _ = a.refreshAccountQuota(context.Background(), "device")
	if gotPath != "/own" || gotIdentity != "device" {
		t.Fatalf("usage hop path %q identity %q", gotPath, gotIdentity)
	}
	if got.Call != ownCallUsage || got.URL != "https://chatgpt.example.test/backend-api/wham/usage" || got.AccountID != "acct-device" || got.AccessToken == "" || got.UserAgent == "" {
		t.Fatalf("usage call = %+v", got)
	}
}

const ownCallVectorsPath = "testdata/own-call-vectors.json"

// ownCallVector is one of the pool's own calls and the request it must become:
// headers in wire order, the body step's content-type included.
type ownCallVector struct {
	Input   codexOwnCall `json:"input"`
	Method  string       `json:"method"`
	URL     string       `json:"url"`
	Headers [][2]string  `json:"headers"`
	Body    *string      `json:"body"`
}

// The Go composition (codexOwnCall.request) and codex-upstream's
// (upstream/src/pool/own_call.rs) are held together by this file: Go writes it,
// and the Rust tests compose every input and must produce the same request.
// Fake credentials only.
func TestOwnCallVectorsMatchTheGoCode(t *testing.T) {
	backend := func(call string) codexOwnCall {
		return codexOwnCall{
			Call:        call,
			URL:         "https://chatgpt.example.test/backend-api/wham/" + call,
			UserAgent:   "codex_exec/0.163.0 (Ubuntu 24.4.0; x86_64) xterm-256color (codex_exec; 0.163.0)",
			AccessToken: "replace-access-token",
			AccountID:   "fake-account-id",
		}
	}
	fedramp := backend(ownCallAccountsCheck)
	fedramp.FedRAMP = true
	noAccount := backend(ownCallUsage)
	noAccount.AccountID = ""
	consume := backend(ownCallConsumeResetCredit)
	consume.RedeemRequestID = "8f14e45f-ceea-467f-9575-c2d1e2a3b4c5"
	consume.CreditID = "fake-credit-id"
	consumeAny := consume
	consumeAny.CreditID = ""
	inputs := []codexOwnCall{
		backend(ownCallUsage),
		noAccount,
		fedramp,
		backend(ownCallResetCredits),
		consume,
		consumeAny,
		{
			Call:         ownCallTokenRefresh,
			URL:          "https://auth.example.test/oauth/token",
			UserAgent:    "codex-tui/0.163.0 (Mac OS 26.0.0; arm64) iTerm.app/3.6",
			Originator:   "codex_tui",
			ClientID:     codexOAuthClientIDDefault,
			RefreshToken: "replace-refresh<&>token",
		},
	}
	vectors := make([]ownCallVector, 0, len(inputs))
	for _, input := range inputs {
		method, order, header, body, err := input.request()
		if err != nil {
			t.Fatal(err)
		}
		vector := ownCallVector{Input: input, Method: method, URL: input.URL, Headers: [][2]string{}}
		before, after, _ := strings.Cut(order, "|")
		add := func(names string) {
			for _, name := range strings.Split(names, ",") {
				if value := header.Get(name); name != "" && value != "" {
					vector.Headers = append(vector.Headers, [2]string{name, value})
				}
			}
		}
		add(before)
		if body != nil {
			text := string(body)
			vector.Body = &text
			vector.Headers = append(vector.Headers, [2]string{"content-type", header.Get("Content-Type")})
		}
		add(after)
		vectors = append(vectors, vector)
	}
	// Written without HTML escaping, so the bodies read as they go out.
	var data bytes.Buffer
	encoder := json.NewEncoder(&data)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(vectors); err != nil {
		t.Fatal(err)
	}
	checkGeneratedFile(t, ownCallVectorsPath, data.Bytes())
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

func TestRotatingLogFileStaysBounded(t *testing.T) {
	path := filepath.Join(t.TempDir(), "logs", "pool.log")
	logFile, err := newRotatingLogFile(path, 100, 2)
	if err != nil {
		t.Fatal(err)
	}
	line := []byte(strings.Repeat("x", 39) + "\n")
	for i := 0; i < 20; i++ {
		if _, err := logFile.Write(line); err != nil {
			t.Fatal(err)
		}
	}
	for _, name := range []string{path, path + ".1", path + ".2"} {
		info, err := os.Stat(name)
		if err != nil || info.Size() > 100 {
			t.Fatalf("%s: %v size %v", name, err, info)
		}
	}
	if _, err := os.Stat(path + ".3"); !os.IsNotExist(err) {
		t.Fatal("only the configured number of rotated logs may be kept")
	}
}
