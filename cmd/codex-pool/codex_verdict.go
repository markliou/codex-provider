package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// Codex's own verdict on an upstream failure (shadow mode, RUST_PORT.md 2b).
//
// The pool classifies every upstream failure with its own table
// (terminalFailureClass, statusFailureClass), which was copied by hand from
// Codex's source. codex-upstream compiles Codex's own reader of the same
// failures, vendored unchanged (upstream/src/classify.rs), and answers
// /classify with what a native Codex concludes: the error kind it maps the
// failure to, and whether it would send the same request again. The verdict
// is only logged next to the pool's own class, so the two can be compared on
// real traffic before the pool relies on it. Nothing here changes a retry, a
// failover or a cooldown; asking is best-effort, bounded, loopback-only, and
// never sends anything upstream.

// codexVerdictTimeout bounds the loopback call. It is made on a failure the
// pool is already handling, so a slow answer must not hold the request.
const codexVerdictTimeout = 2 * time.Second

// codexVerdictDataLimit bounds what the pool hands /classify: an error event
// or an error body, never a whole stream.
const codexVerdictDataLimit = 64 << 10

type codexVerdict struct {
	// Acted is false when Codex skips the message itself and the stream's end
	// decides; the rest is then what Codex reports when the stream closes.
	Acted        bool    `json:"acted"`
	API          string  `json:"api"`
	Kind         string  `json:"kind"`
	Info         string  `json:"info"`
	Retryable    bool    `json:"retryable"`
	RetryAfterMS *uint64 `json:"retry_after_ms"`
}

// logField renders the verdict for a log line: "codex=<kind>/<api> retry=yes",
// with "skipped" when Codex itself does not act on the message.
func (v codexVerdict) logField() string {
	retry := "no"
	if v.Retryable {
		retry = "yes"
		if v.RetryAfterMS != nil {
			retry += fmt.Sprintf("(%dms)", *v.RetryAfterMS)
		}
	}
	field := fmt.Sprintf("codex=%s/%s retry=%s", codeOr(v.Kind, "-"), codeOr(v.API, "-"), retry)
	if !v.Acted {
		field += " codex_skips"
	}
	return field
}

func boundedVerdictData(data string) string {
	if len(data) > codexVerdictDataLimit {
		return data[:codexVerdictDataLimit]
	}
	return data
}

// askCodexVerdict posts one failure to codex-upstream's /classify. Without
// codex-upstream (tests run the pool alone) there is no verdict.
func (a *app) askCodexVerdict(input map[string]any) (codexVerdict, bool) {
	if a.codexTransportURL == "" {
		return codexVerdict{}, false
	}
	body, err := json.Marshal(input)
	if err != nil {
		return codexVerdict{}, false
	}
	ctx, cancel := context.WithTimeout(context.Background(), codexVerdictTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, a.codexTransportURL+"/classify", bytes.NewReader(body))
	if err != nil {
		return codexVerdict{}, false
	}
	req.Header.Set("Content-Type", "application/json")
	response, err := a.streamClient.Do(req)
	if err != nil {
		return codexVerdict{}, false
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return codexVerdict{}, false
	}
	var verdict codexVerdict
	if err := json.NewDecoder(io.LimitReader(response.Body, 64<<10)).Decode(&verdict); err != nil {
		return codexVerdict{}, false
	}
	return verdict, true
}

// codexVerdictForTerminal asks for Codex's verdict on a terminal stream
// failure, the event as upstream sent it on SSE or the Responses WebSocket.
func (a *app) codexVerdictForTerminal(info proxyResponseInfo) (codexVerdict, bool) {
	if info.TerminalData == "" {
		return codexVerdict{}, false
	}
	source := "sse"
	if info.TerminalSource == "ws" {
		source = "ws"
	}
	return a.askCodexVerdict(map[string]any{"source": source, "data": info.TerminalData})
}

// logCodexVerdictForHTTP records an upstream HTTP refusal (401/403, 429, 5xx)
// with the pool's reason and Codex's verdict. These answers had no log line of
// their own; the line exists for the comparison, so it is written only when a
// verdict came back.
func (a *app) logCodexVerdictForHTTP(accountID, model string, status int, header http.Header, body []byte, poolReason string) {
	if a.logger == nil || a.codexTransportURL == "" {
		return
	}
	headers := map[string]string{}
	for name, values := range header {
		headers[strings.ToLower(name)] = strings.Join(values, ", ")
	}
	verdict, ok := a.askCodexVerdict(map[string]any{
		"source":  "http",
		"status":  status,
		"headers": headers,
		"body":    boundedVerdictData(string(body)),
	})
	if !ok {
		return
	}
	a.logger.Printf("upstream http failure: account=%s model=%s status=%d pool=%s code=%s %s",
		accountID, model, status, codeOr(poolReason, "-"), codeOr(httpFailureCode(body), "-"), verdict.logField())
}

// httpFailureCode names an HTTP refusal by its error code, or by its type when
// it has none, as terminalErrorCode does for a stream: a spent allowance comes
// as {"error":{"type":"usage_limit_reached",...}} with no code.
func httpFailureCode(body []byte) string {
	var payload map[string]any
	if json.Unmarshal(body, &payload) != nil {
		return ""
	}
	errorPayload, _ := payload["error"].(map[string]any)
	if errorPayload == nil {
		return ""
	}
	return terminalErrorCode(errorPayload)
}
