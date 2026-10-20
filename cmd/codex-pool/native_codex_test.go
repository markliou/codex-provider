package main

import (
	"bytes"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/klauspost/compress/zstd"
)

// The native endpoint exists so Codex's built-in provider can send the pool
// exactly what it sends OpenAI. What must hold: the key in the path is the
// only way in, the client's own ChatGPT credential and account never go
// upstream, the account's do, and the body Codex compressed reaches upstream as
// the JSON Codex wrote, with Codex's own headers relayed rather than doubled.
func TestNativeCodexEndpointSendsTheClientRequestWithThePoolAccount(t *testing.T) {
	// image_gen is the namespace tool a native Codex sends its own backend; the
	// custom-provider path strips it (dropHostedToolConflicts), the native
	// endpoint must not. Keys are deliberately unsorted so a re-encode shows.
	const raw = `{"model":"gpt-test","tools":[{"type":"namespace","name":"image_gen","tools":[{"type":"function","name":"imagegen","parameters":{"type":"object"}}]}],"stream":true,"store":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t"}}],"client_metadata":{"guardian_credits_requested":"true","mcp_attribution":"{\"status\":\"none\"}"},"prompt_cache_key":"thread-native"}`
	var gotPath, gotBody string
	var gotHeaders http.Header
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		gotHeaders = r.Header.Clone()
		body, _ := io.ReadAll(r.Body)
		gotBody = string(body)
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("X-Models-Etag", "catalog-v2")
		_, _ = w.Write([]byte(`{"id":"resp_native","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "device", AccountID: "acct-pool", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexBaseURL = upstream.URL + "/backend-api"
	writeCodexDeviceAuth(t, a, "device", "acct-pool", "pool@example.test")
	auth, err := a.codexAuth(a.config.Accounts[0])
	if err != nil {
		t.Fatal(err)
	}

	var compressed bytes.Buffer
	encoder, _ := zstd.NewWriter(&compressed)
	_, _ = encoder.Write([]byte(raw))
	_ = encoder.Close()
	send := func(key string) *httptest.ResponseRecorder {
		req := httptest.NewRequest(http.MethodPost, "/k/"+key+"/backend-api/codex/responses", bytes.NewReader(compressed.Bytes()))
		req.Header.Set("Content-Encoding", "zstd")
		req.Header.Set("Authorization", "Bearer <client-chatgpt-login>")
		req.Header.Set("ChatGPT-Account-ID", "acct-client")
		req.Header.Set("X-OpenAI-Account-Routing-Override", "us")
		req.Header.Set("X-Codex-Routing-Hint", "model=gpt-test;tier=priority")
		req.Header.Set("Version", "0.163.0")
		req.Header.Set("Session-Id", "session-native")
		req.Header.Set("Originator", "codex_exec")
		recorder := httptest.NewRecorder()
		a.publicMux().ServeHTTP(recorder, req)
		return recorder
	}

	if recorder := send("wrong-key"); recorder.Code != http.StatusUnauthorized || gotPath != "" {
		t.Fatalf("a wrong path key must be refused before anything goes upstream: %d, upstream path %q", recorder.Code, gotPath)
	}
	recorder := send("client-key")
	if recorder.Code != http.StatusOK {
		t.Fatalf("native endpoint returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if recorder.Header().Get("X-Models-Etag") != "catalog-v2" {
		t.Fatal("a native client refetches its catalog on a new X-Models-Etag, which must reach it")
	}
	// Local telemetry says the request came through the native endpoint.
	if events := a.state.RoutingCacheEvents; len(events) == 0 || events[len(events)-1].ClientEntry != "native/http" {
		t.Fatalf("routing event entry = %#v", events)
	}
	if gotPath != "/backend-api/codex/responses" {
		t.Fatalf("upstream path = %q", gotPath)
	}
	if gotBody != raw {
		t.Fatalf("the JSON Codex compressed must reach upstream byte for byte:\n got %s\nwant %s", gotBody, raw)
	}
	for name, want := range map[string]string{
		"Authorization":        "Bearer " + auth.AccessToken,
		"ChatGPT-Account-ID":   "acct-pool",
		"X-Codex-Routing-Hint": "model=gpt-test;tier=priority",
		"Version":              "0.163.0",
		"Session-Id":           "session-native",
		"Originator":           "codex_exec",
	} {
		if got := gotHeaders.Values(name); len(got) != 1 || got[0] != want {
			t.Fatalf("upstream %s = %q, want exactly %q", name, got, want)
		}
	}
	if got := gotHeaders.Get("X-OpenAI-Account-Routing-Override"); got != "" {
		t.Fatalf("the client account's workspace routing must not follow the request to a pool account: %q", got)
	}
	if strings.Contains(strings.Join(gotHeaders.Values("Authorization"), ""), "client-chatgpt-login") {
		t.Fatal("the client's own ChatGPT login reached upstream")
	}
}

func TestNativeCodexEndpointPassesModelsThroughWithAPoolAccount(t *testing.T) {
	var gotURL, gotAuth, gotAccount string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotURL = r.URL.String()
		gotAuth = r.Header.Get("Authorization")
		gotAccount = r.Header.Get("ChatGPT-Account-ID")
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("ETag", `"models-v1"`)
		w.Header().Set("Set-Cookie", "upstream=private")
		_, _ = w.Write([]byte(`{"models":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "device", AccountID: "acct-pool", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexBaseURL = upstream.URL + "/backend-api"
	writeCodexDeviceAuth(t, a, "device", "acct-pool", "pool@example.test")
	auth, _ := a.codexAuth(a.config.Accounts[0])

	req := httptest.NewRequest(http.MethodGet, "/k/client-key/backend-api/codex/models?client_version=0.163.0", nil)
	req.Header.Set("Authorization", "Bearer <client-chatgpt-login>")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK || recorder.Body.String() != `{"models":[]}` {
		t.Fatalf("models returned %d: %s", recorder.Code, recorder.Body.String())
	}
	if gotURL != "/backend-api/codex/models?client_version=0.163.0" || gotAuth != "Bearer "+auth.AccessToken || gotAccount != "acct-pool" {
		t.Fatalf("upstream got %q with auth %q account %q", gotURL, gotAuth, gotAccount)
	}
	if recorder.Header().Get("ETag") != `"models-v1"` {
		t.Fatal("Codex caches the catalog by its ETag, which must reach it")
	}
	if recorder.Header().Get("Set-Cookie") != "" {
		t.Fatal("upstream cookies belong to the pool account's session, not the client")
	}

	unknown := httptest.NewRequest(http.MethodGet, "/k/client-key/backend-api/codex/wham/usage", nil)
	recorder = httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, unknown)
	if recorder.Code != http.StatusNotFound {
		t.Fatalf("a path the pool does not serve must be refused, got %d", recorder.Code)
	}
}

func TestNativeCodexEndpointServesTheResponsesWebSocket(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(string) []string { return completedEvents("resp_ws_native") }}
	a, url := wsTestPool(t, upstream)
	native := strings.Replace(url, "/v1/responses", "/k/client-key/backend-api/codex/responses", 1)
	conn := dialPool(t, native)
	if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-test","input":[]}`)); err != nil {
		t.Fatal(err)
	}
	if events := readUntilTerminal(t, conn); len(events) != len(completedEvents("x")) {
		t.Fatalf("native socket relayed %d events", len(events))
	}
	waitInFlightZero(t, a)
	a.mu.RLock()
	defer a.mu.RUnlock()
	if events := a.state.RoutingCacheEvents; len(events) == 0 || events[len(events)-1].ClientEntry != "native/ws" {
		t.Fatalf("routing event entry = %#v", events)
	}
}

// A native client's header the relay does not carry is reported by name,
// once, so a Codex header the allowlist misses cannot vanish silently.
func TestNativeEndpointReportsHeadersItDoesNotRelay(t *testing.T) {
	var logged bytes.Buffer
	a := testApp(t, nil)
	a.logger = log.New(&logged, "", 0)
	header := http.Header{}
	header.Set("Authorization", "Bearer <client-login>")
	header.Set("X-Codex-Turn-Metadata", "{}")
	header.Set("X-Oai-Attestation", "<attestation>")
	header.Set("X-Some-Future-Header", "<value>")
	header.Set("Originator", "Codex_Desktop")
	a.noteUnrelayedNativeHeaders(header)
	a.noteUnrelayedNativeHeaders(header)
	out := logged.String()
	if strings.Count(out, "not relayed") != 1 || !strings.Contains(out, "x-some-future-header") {
		t.Fatalf("log = %q", out)
	}
	for _, leaked := range []string{"<value>", "<client-login>", "<attestation>", "x-oai-attestation", "x-codex-turn-metadata", "authorization"} {
		if strings.Contains(out, leaked) {
			t.Fatalf("the report must name only unrelayed headers and never a value: %q", out)
		}
	}
}
