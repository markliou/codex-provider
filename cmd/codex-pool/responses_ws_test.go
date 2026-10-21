package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
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
	// handshake overrides the 101's extra headers (default: a turn state).
	handshake http.Header
	// unreachable makes the next N handshakes for an identity fail as
	// codex-upstream does when it cannot reach upstream at all.
	unreachable map[string]int
}

func (f *fakeCodexUpstream) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	// codex-upstream answers /classify apart from the relay; it is not a
	// handshake, and with no verdict the pool only leaves the log field out.
	if r.URL.Path == "/classify" {
		http.NotFound(w, r)
		return
	}
	identity := r.Header.Get("X-Codex-Upstream-Identity")
	f.mu.Lock()
	f.handshakes = append(f.handshakes, r.Header.Clone())
	status := f.refuse[identity]
	unreachable := f.unreachable[identity] > 0
	if unreachable {
		f.unreachable[identity]--
	}
	f.mu.Unlock()
	if unreachable {
		w.Header().Set("X-Codex-Upstream-Error", "connection")
		w.WriteHeader(http.StatusBadGateway)
		return
	}
	if status != 0 {
		w.WriteHeader(status)
		return
	}
	extra := f.handshake
	if extra == nil {
		extra = http.Header{"X-Codex-Turn-State": {"ts-ws-" + identity}}
	}
	conn, err := acceptWebSocket(w, r, extra)
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

// nativePreamble is what the real Codex backend sends on a socket before
// response.created, captured from a live session.
var nativePreamble = []string{
	`{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":3}}}`,
	`{"type":"codex.response.metadata","metadata":{}}`,
}

func completedEvents(id string) []string {
	return append(append([]string(nil), nativePreamble...),
		`{"type":"response.created","response":{"id":"`+id+`"}}`,
		`{"type":"response.output_text.delta","delta":"ok"}`,
		`{"type":"responsesapi.websocket_timing","timing":{}}`,
		`{"type":"response.completed","response":{"id":"`+id+`","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":5}}}`,
	)
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
		if events := readUntilTerminal(t, conn); len(events) != len(completedEvents("x")) || !strings.Contains(events[len(events)-1], "response.completed") {
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
	if events := readUntilTerminal(t, conn); len(events) != len(completedEvents("x")) {
		t.Fatalf("events = %q", events)
	}
	waitInFlightZero(t, a)
	upstream.mu.Lock()
	defer upstream.mu.Unlock()
	if len(upstream.handshakes) != 2 || upstream.handshakes[1].Get("X-Codex-Upstream-Identity") != "second" {
		t.Fatalf("expected failover to the second account: %d handshakes", len(upstream.handshakes))
	}
}

// When an account's allowance is spent before any output, Codex is shown
// nothing: the socket closes, Codex reconnects and resends in full, and the
// cooled account is not chosen again.
func TestResponsesWebSocketClosesAfterACapacityFailure(t *testing.T) {
	for _, tc := range []struct{ name, failure string }{
		{"rate limit", `{"type":"response.failed","response":{"id":"resp_busy","error":{"code":"rate_limit_exceeded","message":"limit"}}}`},
		// The shape the Codex backend sends for a spent allowance: the kind is
		// in error.type and there is no code. Read by code only, it went to
		// Codex as a final error and the lineage stayed on the spent account.
		{"usage limit", `{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"team","resets_at":1738888888}}`},
	} {
		t.Run(tc.name, func(t *testing.T) {
			upstream := &fakeCodexUpstream{script: func(string) []string {
				return append(append([]string(nil), nativePreamble...),
					`{"type":"response.created","response":{"id":"resp_busy"}}`,
					tc.failure,
				)
			}}
			a, url := wsTestPool(t, upstream)
			conn := dialPool(t, url)
			if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-test","input":[]}`)); err != nil {
				t.Fatal(err)
			}
			if events := readUntilTerminal(t, conn); len(events) != 0 {
				t.Fatalf("nothing of a failure before output may reach Codex, which would stop the turn: %q", events)
			}
			waitInFlightZero(t, a)
			a.mu.RLock()
			defer a.mu.RUnlock()
			cooled := false
			for _, entry := range a.state.Cooldowns["first"] {
				cooled = cooled || entry.NextRetryAt.After(time.Now())
			}
			if !cooled {
				t.Fatal("the spent account must cool down so the reconnect goes elsewhere")
			}
		})
	}
}

// An overload stays on the socket: Codex retries on the same connection and so
// on the same account, keeping the lineage and its cache, and the account is
// not cooled down while it is inside the same-account schedule.
func TestResponsesWebSocketKeepsTheSocketThroughAnOverload(t *testing.T) {
	for _, code := range []string{"server_is_overloaded", "server_error"} {
		t.Run(code, func(t *testing.T) { testWebSocketKeepsTheSocketThrough(t, code) })
	}
}

func testWebSocketKeepsTheSocketThrough(t *testing.T, code string) {
	var mu sync.Mutex
	calls := 0
	upstream := &fakeCodexUpstream{script: func(string) []string {
		mu.Lock()
		calls++
		n := calls
		mu.Unlock()
		if n == 1 {
			return append(append([]string(nil), nativePreamble...), `{"type":"response.created","response":{"id":"resp_busy"}}`, `{"type":"error","status":503,"error":{"code":"`+code+`","message":"busy"}}`)
		}
		return completedEvents("resp_ok")
	}}
	a, url := wsTestPool(t, upstream)
	conn := dialPool(t, url)
	turn := `{"type":"response.create","model":"gpt-test","previous_response_id":"resp_prev","input":[]}`
	for round := 0; round < 2; round++ {
		if err := conn.WriteMessage(wsOpText, []byte(turn)); err != nil {
			t.Fatalf("round %d: socket closed: %v", round, err)
		}
		events := readUntilTerminal(t, conn)
		if len(events) != len(completedEvents("x")) || strings.Contains(strings.Join(events, ""), code) {
			t.Fatalf("round %d: the %s must be retried before Codex sees it: %q", round, code, events)
		}
	}
	waitInFlightZero(t, a)
	upstream.mu.Lock()
	handshakes, messages := len(upstream.handshakes), append([]string(nil), upstream.messages...)
	upstream.mu.Unlock()
	if handshakes != 1 || len(messages) != 3 || messages[0] != turn || messages[1] != turn {
		t.Fatalf("the retry must resend the same request on the same socket: handshakes=%d messages=%q", handshakes, messages)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	for _, entry := range a.state.Cooldowns["first"] {
		if entry.NextRetryAt.After(time.Now()) {
			t.Fatal("an overload inside the same-account schedule must not cool the account")
		}
	}
}

// An overload that outlasts the same-account schedule cools the account and
// drops the socket without showing Codex the failure, so Codex reconnects and
// the lineage moves.
func TestResponsesWebSocketMovesOnWhenTheOverloadOutlastsTheSchedule(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(string) []string {
		return append(append([]string(nil), nativePreamble...), `{"type":"response.created","response":{"id":"resp_busy"}}`, `{"type":"response.failed","response":{"id":"resp_busy","error":{"code":"server_is_overloaded"}}}`)
	}}
	a, url := wsTestPool(t, upstream)
	conn := dialPool(t, url)
	if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-test","input":[]}`)); err != nil {
		t.Fatal(err)
	}
	if events := readUntilTerminal(t, conn); len(events) != 0 {
		t.Fatalf("the exhausted overload must not reach Codex: %q", events)
	}
	waitInFlightZero(t, a)
	upstream.mu.Lock()
	sent := len(upstream.messages)
	upstream.mu.Unlock()
	if sent != overloadSameAccountRetryLimit+1 {
		t.Fatalf("upstream got %d sends, want %d", sent, overloadSameAccountRetryLimit+1)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	cooled := false
	for _, entry := range a.state.Cooldowns["first"] {
		cooled = cooled || entry.NextRetryAt.After(time.Now())
	}
	if !cooled {
		t.Fatal("the account that kept refusing must cool down")
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

// TestMain shortens the same-account overload backoff so tests that exercise
// overload retries do not each wait the production 1s, 2s and 4s.
func TestMain(m *testing.M) {
	overloadRetryBackoff = time.Millisecond
	transportRetryBackoff = time.Millisecond
	os.Exit(m.Run())
}

// The upstream handshake's headers reach Codex, as they reach a native Codex:
// Codex takes its socket turn state from the handshake alone and sends it back
// in client_metadata, and reads x-reasoning-included there. The account is
// chosen, and its socket opened, before the client's socket is accepted.
func TestResponsesWebSocketRelaysTheUpstreamHandshake(t *testing.T) {
	upstream := &fakeCodexUpstream{
		handshake: http.Header{"X-Codex-Turn-State": {"ts-native"}, "X-Reasoning-Included": {"true"}, "Openai-Model": {"gpt-test"}, "X-Models-Etag": {"catalog"}},
		script:    func(string) []string { return completedEvents("resp_hs") },
	}
	_, url := wsTestPool(t, upstream)
	headers := http.Header{}
	headers.Set("Authorization", "Bearer client-key")
	headers.Set("Session-Id", "session-hs")
	conn, response, err := dialWebSocket(url, headers)
	if err != nil || conn == nil {
		t.Fatalf("dial: %v %v", err, response)
	}
	defer conn.Close()
	for name, want := range map[string]string{"X-Codex-Turn-State": "ts-native", "X-Reasoning-Included": "true", "OpenAI-Model": "gpt-test"} {
		if got := response.Header.Get(name); got != want {
			t.Fatalf("client handshake %s = %q, want %q", name, got, want)
		}
	}
	// On /v1 the catalog is the pool's own, so upstream's ETag is withheld.
	if got := response.Header.Get("X-Models-Etag"); got != "" {
		t.Fatalf("a /v1 socket must not carry upstream's catalog ETag: %q", got)
	}
	upstream.mu.Lock()
	opened := len(upstream.handshakes)
	upstream.mu.Unlock()
	if opened != 1 {
		t.Fatalf("the upstream socket must be open before the client's is accepted: %d handshakes", opened)
	}
}

func TestResponsesWebSocketRefusesTheHandshakeWhenNoAccountCan(t *testing.T) {
	upstream := &fakeCodexUpstream{refuse: map[string]int{"first": http.StatusTooManyRequests, "second": http.StatusTooManyRequests}, script: func(string) []string { return nil }}
	_, url := wsTestPool(t, upstream)
	headers := http.Header{}
	headers.Set("Authorization", "Bearer client-key")
	conn, response, err := dialWebSocket(url, headers)
	if err != nil || conn != nil || response == nil || response.StatusCode == http.StatusSwitchingProtocols {
		t.Fatalf("the handshake must be refused before a socket exists: conn=%v response=%v err=%v", conn, response, err)
	}
}

// The handshake can only guess the account; the first request may settle it
// elsewhere. Nothing has gone out on that upstream socket, so it is replaced
// without closing Codex's socket, which would only reconnect into the guess.
func TestResponsesWebSocketMovesTheSocketForItsFirstRequest(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(identity string) []string { return completedEvents("resp_" + identity) }}
	a, url := wsTestPool(t, upstream)
	a.config.Accounts[0].AllowedModels = []string{"gpt-test"}
	conn := dialPool(t, url)
	if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-other","input":[]}`)); err != nil {
		t.Fatal(err)
	}
	events := readUntilTerminal(t, conn)
	if len(events) != len(completedEvents("x")) || !strings.Contains(strings.Join(events, ""), "resp_second") {
		t.Fatalf("the first request must be served by the account that can serve it: %q", events)
	}
	waitInFlightZero(t, a)
	upstream.mu.Lock()
	defer upstream.mu.Unlock()
	if len(upstream.handshakes) != 2 || upstream.handshakes[0].Get("X-Codex-Upstream-Identity") != "first" || upstream.handshakes[1].Get("X-Codex-Upstream-Identity") != "second" || len(upstream.messages) != 1 {
		t.Fatalf("handshakes %d, messages %d", len(upstream.handshakes), len(upstream.messages))
	}
}

// A turn state goes back only to the account that issued it, as on HTTP; on a
// socket Codex carries it in client_metadata. Removing it leaves every other
// byte of the message as Codex wrote it.
func TestResponsesWebSocketWithholdsAnotherAccountsTurnState(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(string) []string { return completedEvents("resp_ts") }}
	a, url := wsTestPool(t, upstream)
	// The second account issued this one, on a socket the session left.
	a.noteTurnStateIssuer("ts-from-another-account", "second")
	conn := dialPool(t, url)
	own := `{"type":"response.create","model":"gpt-test","client_metadata":{"session_id":"s","x-codex-turn-state":"ts-ws-first","thread_id":"t"},"input":[]}`
	foreign := `{"type":"response.create","model":"gpt-test","client_metadata":{"session_id":"s","x-codex-turn-state":"ts-from-another-account","thread_id":"t"},"input":[]}`
	for _, message := range []string{own, foreign} {
		if err := conn.WriteMessage(wsOpText, []byte(message)); err != nil {
			t.Fatal(err)
		}
		readUntilTerminal(t, conn)
	}
	waitInFlightZero(t, a)
	upstream.mu.Lock()
	defer upstream.mu.Unlock()
	if len(upstream.messages) != 2 {
		t.Fatalf("messages = %q", upstream.messages)
	}
	if upstream.messages[0] != own {
		t.Fatalf("the issuing account's turn state must pass byte for byte:\n got %s\nwant %s", upstream.messages[0], own)
	}
	if want := `{"type":"response.create","model":"gpt-test","client_metadata":{"session_id":"s","thread_id":"t"},"input":[]}`; upstream.messages[1] != want {
		t.Fatalf("another account's turn state must be removed and nothing else:\n got %s\nwant %s", upstream.messages[1], want)
	}
}

// A socket that cannot be opened is retried on its account before the
// session moves: a connection failure says nothing against the account, and
// under strict binding a move is for good and costs a full prefill.
func TestResponsesWebSocketRetriesAnUnreachableAccountBeforeMoving(t *testing.T) {
	for _, tc := range []struct {
		name     string
		failures int
		want     []string
	}{
		{"recovers on its account", transportSameAccountRetryLimit, []string{"first", "first", "first"}},
		{"moves once the retries are spent", transportSameAccountRetryLimit + 1, []string{"first", "first", "first", "second"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			upstream := &fakeCodexUpstream{unreachable: map[string]int{"first": tc.failures}, script: func(string) []string { return completedEvents("resp_retry") }}
			a, url := wsTestPool(t, upstream)
			conn := dialPool(t, url)
			if err := conn.WriteMessage(wsOpText, []byte(`{"type":"response.create","model":"gpt-test","input":[]}`)); err != nil {
				t.Fatal(err)
			}
			if events := readUntilTerminal(t, conn); len(events) != len(completedEvents("x")) {
				t.Fatalf("events = %q", events)
			}
			waitInFlightZero(t, a)
			upstream.mu.Lock()
			defer upstream.mu.Unlock()
			var got []string
			for _, handshake := range upstream.handshakes {
				got = append(got, handshake.Get("X-Codex-Upstream-Identity"))
			}
			if strings.Join(got, ",") != strings.Join(tc.want, ",") {
				t.Fatalf("handshakes went to %v, want %v", got, tc.want)
			}
		})
	}
}

// A 426 is the backend asking for HTTP, the same for every account: the pool
// passes it on after the one handshake a native Codex would make, and cools
// nothing.
func TestResponsesWebSocketPassesUpgradeRequiredOnAtOnce(t *testing.T) {
	upstream := &fakeCodexUpstream{refuse: map[string]int{"first": http.StatusUpgradeRequired, "second": http.StatusUpgradeRequired}, script: func(string) []string { return nil }}
	a, url := wsTestPool(t, upstream)
	headers := http.Header{}
	headers.Set("Authorization", "Bearer client-key")
	_, response, _ := dialWebSocket(url, headers)
	if response == nil || response.StatusCode != http.StatusUpgradeRequired {
		t.Fatalf("handshake answer = %v, want 426", response)
	}
	upstream.mu.Lock()
	opened := len(upstream.handshakes)
	upstream.mu.Unlock()
	if opened != 1 {
		t.Fatalf("upstream saw %d handshakes, want the one a native Codex makes", opened)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	if len(a.state.Cooldowns["first"]) != 0 {
		t.Fatalf("a 426 must not cool the account: %+v", a.state.Cooldowns["first"])
	}
}
