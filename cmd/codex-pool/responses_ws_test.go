package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// fakeCodexUpstream stands in for codex-upstream's /ws: it records each
// handshake and answers response.create with a scripted event sequence.
type fakeCodexUpstream struct {
	mu         sync.Mutex
	handshakes []http.Header
	messages   []string
	refuse     map[string]int                 // identity -> handshake status
	script     func(identity string) []string // events per response.create
}

func (f *fakeCodexUpstream) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	identity := r.Header.Get("X-Codex-Upstream-Identity")
	f.mu.Lock()
	f.handshakes = append(f.handshakes, r.Header.Clone())
	status := f.refuse[identity]
	f.mu.Unlock()
	if status != 0 {
		w.WriteHeader(status)
		return
	}
	conn, err := acceptWebSocket(w, r, http.Header{"X-Codex-Turn-State": {"ts-ws"}})
	if err != nil {
		return
	}
	defer conn.Close()
	for {
		_, message, err := conn.ReadMessage()
		if err != nil {
			return
		}
		f.mu.Lock()
		f.messages = append(f.messages, string(message))
		f.mu.Unlock()
		for _, event := range f.script(identity) {
			if conn.WriteMessage(wsOpText, []byte(event)) != nil {
				return
			}
		}
	}
}

func completedEvents(id string) []string {
	return []string{
		`{"type":"response.created","response":{"id":"` + id + `"}}`,
		`{"type":"response.completed","response":{"id":"` + id + `","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":5}}}`,
	}
}

func wsTestPool(t *testing.T, upstream *fakeCodexUpstream) (*app, string) {
	t.Helper()
	transport := httptest.NewServer(upstream)
	t.Cleanup(transport.Close)
	a := testApp(t, []account{
		{ID: "first", AccountID: "acct-first", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100},
		{ID: "second", AccountID: "acct-second", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 10},
	})
	a.codexBaseURL = "https://chatgpt.example.test/backend-api"
	a.codexTransportURL = transport.URL
	writeCodexDeviceAuth(t, a, "first", "acct-first", "first@example.test")
	writeCodexDeviceAuth(t, a, "second", "acct-second", "second@example.test")
	pool := httptest.NewServer(a.publicMux())
	t.Cleanup(pool.Close)
	return a, "ws" + strings.TrimPrefix(pool.URL, "http") + "/v1/responses"
}

func dialPool(t *testing.T, url string) *wsConn {
	t.Helper()
	headers := http.Header{}
	headers.Set("Authorization", "Bearer client-key")
	headers.Set("OpenAI-Beta", "responses_websockets=2026-02-06")
	headers.Set("Session-Id", "session-ws")
	headers.Set("Thread-Id", "thread-ws")
	headers.Set("X-Codex-Turn-Metadata", `{"thread_id":"thread-ws","session_id":"session-ws"}`)
	headers.Set("X-Codex-Turn-State", "stale-turn-state")
	headers.Set("User-Agent", "codex-tui/0.163.0 (Mac OS 26.0.0; arm64)")
	headers.Set("Originator", "Codex_Desktop")
	conn, response, err := dialWebSocket(url, headers)
	if err != nil || conn == nil {
		t.Fatalf("dial pool: err=%v response=%v", err, response)
	}
	t.Cleanup(conn.Close)
	return conn
}

func readUntilTerminal(t *testing.T, conn *wsConn) []string {
	t.Helper()
	var events []string
	for {
		_, message, err := conn.ReadMessage()
		if err != nil {
			return events
		}
		events = append(events, string(message))
		var event map[string]any
		_ = json.Unmarshal(message, &event)
		if kind, _ := event["type"].(string); kind == "response.completed" || kind == "response.failed" || kind == "error" {
			return events
		}
	}
}

func waitInFlightZero(t *testing.T, a *app) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for a.inFlight.Load() != 0 {
		if time.Now().After(deadline) {
			t.Fatalf("in-flight count stuck at %d", a.inFlight.Load())
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// The socket is relayed, not rebuilt: prewarm and incremental requests reach
// upstream byte for byte over one upstream socket opened with the native
// handshake, and only real responses are recorded.
func TestResponsesWebSocketRelaysCodexMessagesUnchanged(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(string) []string { return completedEvents("resp_ws") }}
	a, url := wsTestPool(t, upstream)
	conn := dialPool(t, url)
	prewarm := `{"type":"response.create","model":"gpt-test","generate":false,"input":[],"tools":[{"type":"function","name":"z_tool"}],"store":false,"stream":true}`
	turn := `{"type":"response.create","model":"gpt-test","previous_response_id":"resp_ws","input":[{"type":"message","role":"user","content":"hi"}],"store":false,"stream":true}`
	for _, message := range []string{prewarm, turn} {
		if err := conn.WriteMessage(wsOpText, []byte(message)); err != nil {
			t.Fatal(err)
		}
		if events := readUntilTerminal(t, conn); len(events) != 2 || !strings.Contains(events[1], "response.completed") {
			t.Fatalf("events not relayed: %q", events)
		}
	}
	waitInFlightZero(t, a)

	upstream.mu.Lock()
	defer upstream.mu.Unlock()
	if len(upstream.handshakes) != 1 {
		t.Fatalf("one client socket must use one upstream socket, got %d handshakes", len(upstream.handshakes))
	}
	if len(upstream.messages) != 2 || upstream.messages[0] != prewarm || upstream.messages[1] != turn {
		t.Fatalf("messages must reach upstream byte for byte: %q", upstream.messages)
	}
	first, err := a.codexAuth(a.config.Accounts[0])
	if err != nil {
		t.Fatal(err)
	}
	handshake := upstream.handshakes[0]
	for name, want := range map[string]string{
		"X-Codex-Upstream-Url":      "wss://chatgpt.example.test/backend-api/codex/responses",
		"X-Codex-Upstream-Identity": "first",
		"Authorization":             "Bearer " + first.AccessToken,
		"Chatgpt-Account-Id":        "acct-first",
		"X-Codex-Routing-Hint":      "model=gpt-test",
		"Version":                   "0.163.0",
		"Openai-Beta":               "responses_websockets=2026-02-06",
		"Session-Id":                "session-ws",
		"Thread-Id":                 "thread-ws",
		"Originator":                "Codex_Desktop",
	} {
		if got := handshake.Get(name); got != want {
			t.Fatalf("upstream handshake %s = %q, want %q", name, got, want)
		}
	}
	if got := handshake.Get("X-Codex-Turn-State"); got != "" {
		t.Fatalf("a native handshake carries no turn state, got %q", got)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	if len(a.state.RoutingCacheEvents) != 1 || a.state.RoutingCacheEvents[0].CachedTokens != 80 {
		t.Fatalf("only the real response is recorded, with its usage: %#v", a.state.RoutingCacheEvents)
	}
}

// A refused upstream handshake fails over before anything reaches upstream,
// like an HTTP request.
func TestResponsesWebSocketFailsOverARefusedHandshake(t *testing.T) {
	upstream := &fakeCodexUpstream{refuse: map[string]int{"first": http.StatusTooManyRequests}, script: func(string) []string { return completedEvents("resp_second") }}
	a, url := wsTestPool(t, upstream)
	conn := dialPool(t, url)
	if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-test","input":[]}`)); err != nil {
		t.Fatal(err)
	}
	if events := readUntilTerminal(t, conn); len(events) != 2 {
		t.Fatalf("events = %q", events)
	}
	waitInFlightZero(t, a)
	upstream.mu.Lock()
	defer upstream.mu.Unlock()
	if len(upstream.handshakes) != 2 || upstream.handshakes[1].Get("X-Codex-Upstream-Identity") != "second" {
		t.Fatalf("expected failover to the second account: %d handshakes", len(upstream.handshakes))
	}
}

// After a capacity failure the event reaches Codex and the socket closes, so
// Codex reconnects and resends in full instead of reusing the failing account.
func TestResponsesWebSocketClosesAfterACapacityFailure(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(string) []string {
		return []string{
			`{"type":"response.created","response":{"id":"resp_busy"}}`,
			`{"type":"response.failed","response":{"id":"resp_busy","error":{"code":"server_is_overloaded","message":"busy"}}}`,
		}
	}}
	a, url := wsTestPool(t, upstream)
	conn := dialPool(t, url)
	if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-test","input":[]}`)); err != nil {
		t.Fatal(err)
	}
	events := readUntilTerminal(t, conn)
	if len(events) != 2 || !strings.Contains(events[1], "server_is_overloaded") {
		t.Fatalf("the failure must reach Codex unchanged: %q", events)
	}
	if _, _, err := conn.ReadMessage(); err == nil {
		t.Fatal("the client socket must close after a capacity failure")
	}
	waitInFlightZero(t, a)
	a.mu.RLock()
	defer a.mu.RUnlock()
	cooled := false
	for _, entry := range a.state.Cooldowns["first"] {
		cooled = cooled || entry.NextRetryAt.After(time.Now())
	}
	if !cooled {
		t.Fatal("the overloaded account must cool down so the reconnect goes elsewhere")
	}
}

func TestResponsesWebSocketWithoutTransportAsksForHTTP(t *testing.T) {
	a := testApp(t, []account{{ID: "provider", Enabled: true, InPool: true, Priority: 100, UpstreamBaseURL: "http://127.0.0.1:1/v1", UpstreamAPIKey: "upstream-key"}})
	req := httptest.NewRequest(http.MethodGet, "/v1/responses", nil)
	req.Header.Set("Authorization", "Bearer client-key")
	req.Header.Set("Upgrade", "websocket")
	req.Header.Set("Connection", "Upgrade")
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusUpgradeRequired {
		t.Fatalf("status = %d, want 426 so Codex falls back to HTTP", recorder.Code)
	}
}
