package main

import (
	"bytes"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
)

// fakeClassifier stands in for codex-upstream's /classify and records what the
// pool asked. Its answers are fixed: these tests hold the pool's side of the
// exchange, and upstream/src/classify.rs tests hold Codex's reading.
type fakeClassifier struct {
	mu     sync.Mutex
	asked  []map[string]any
	answer codexVerdict
}

func (f *fakeClassifier) serve(t *testing.T) *httptest.Server {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/classify" || r.Method != http.MethodPost {
			http.NotFound(w, r)
			return
		}
		var input map[string]any
		body, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(body, &input)
		f.mu.Lock()
		f.asked = append(f.asked, input)
		f.mu.Unlock()
		_ = json.NewEncoder(w).Encode(f.answer)
	}))
	t.Cleanup(server.Close)
	return server
}

// A terminal stream failure is logged with Codex's verdict on the very event
// upstream sent, and the event is labelled with the stream it came on: Codex
// reads a WebSocket message for a wrapped error first, an SSE event never.
func TestTerminalFailureLogCarriesCodexVerdict(t *testing.T) {
	classifier := &fakeClassifier{answer: codexVerdict{Acted: true, API: "Retryable", Kind: "stream", Retryable: true}}
	server := classifier.serve(t)
	a := testApp(t, nil)
	a.codexTransportURL = server.URL
	var logged bytes.Buffer
	a.logger = log.New(&logged, "", 0)

	event := `{"type":"response.failed","response":{"id":"resp_1","error":{"code":"server_error","message":"boom"}}}`
	sse := responseInfoFromSSEBlock("event: response.failed\ndata: " + event + "\n\n")
	if sse.TerminalData != event || sse.TerminalSource != "" {
		t.Fatalf("SSE terminal data = %q source %q", sse.TerminalData, sse.TerminalSource)
	}
	a.logTerminalResponseFailure("acct", "gpt-test", sse, false)

	ws := responseInfoFromSSEBlock("data: " + event + "\n\n")
	markWebSocketTerminal(&ws)
	a.logTerminalResponseFailure("acct", "gpt-test", ws, true)

	lines := strings.Split(strings.TrimSpace(logged.String()), "\n")
	if len(lines) != 2 {
		t.Fatalf("log lines = %q", lines)
	}
	for _, line := range lines {
		if !strings.Contains(line, "class=transient") || !strings.HasSuffix(line, "codex=stream/Retryable retry=yes") {
			t.Fatalf("log line lacks the pool class or Codex's verdict: %s", line)
		}
	}
	if len(classifier.asked) != 2 || classifier.asked[0]["source"] != "sse" || classifier.asked[1]["source"] != "ws" || classifier.asked[0]["data"] != event {
		t.Fatalf("classifier asked %v", classifier.asked)
	}
}

// A completed response is no failure, and nothing about it is kept for Codex.
func TestCompletedResponseKeepsNoTerminalData(t *testing.T) {
	info := responseInfoFromSSEBlock("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\"}}\n\n")
	if info.TerminalEvent != "response.completed" || info.TerminalData != "" {
		t.Fatalf("completed response info = %+v", info)
	}
}

// Without codex-upstream the pool runs alone and the log is what it was: no
// verdict, no call, no new HTTP failure line.
func TestNoCodexVerdictWithoutTransport(t *testing.T) {
	a := testApp(t, nil)
	var logged bytes.Buffer
	a.logger = log.New(&logged, "", 0)
	info := responseInfoFromSSEBlock("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\"}}}\n\n")
	a.logTerminalResponseFailure("acct", "gpt-test", info, false)
	a.logCodexVerdictForHTTP("acct", "gpt-test", http.StatusTooManyRequests, http.Header{}, []byte(`{}`), "rate_limited")
	if strings.Contains(logged.String(), "codex=") || strings.Contains(logged.String(), "upstream http failure") {
		t.Fatalf("log without codex-upstream = %s", logged.String())
	}
}

// An HTTP refusal is handed over with its status, headers and body, the way
// Codex's HTTP client hands one on, and logged beside the pool's own reason.
func TestHTTPFailureLogCarriesCodexVerdict(t *testing.T) {
	after := uint64(5000)
	classifier := &fakeClassifier{answer: codexVerdict{Acted: true, API: "Transport", Kind: "usage_limit_reached", RetryAfterMS: &after}}
	server := classifier.serve(t)
	a := testApp(t, nil)
	a.codexTransportURL = server.URL
	var logged bytes.Buffer
	a.logger = log.New(&logged, "", 0)

	header := http.Header{}
	header.Set("Retry-After", "5")
	body := []byte(`{"error":{"type":"usage_limit_reached","message":"limit"}}`)
	a.logCodexVerdictForHTTP("acct", "gpt-test", http.StatusTooManyRequests, header, body, "rate_limited")

	line := strings.TrimSpace(logged.String())
	want := "upstream http failure: account=acct model=gpt-test status=429 pool=rate_limited code=usage_limit_reached codex=usage_limit_reached/Transport retry=no"
	if line != want {
		t.Fatalf("log line\n got %s\nwant %s", line, want)
	}
	asked := classifier.asked[0]
	headers, _ := asked["headers"].(map[string]any)
	if asked["source"] != "http" || asked["status"] != float64(429) || asked["body"] != string(body) || headers["retry-after"] != "5" {
		t.Fatalf("classifier asked %v", asked)
	}
}
