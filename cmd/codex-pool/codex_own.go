package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"runtime"
	"strings"
	"time"
)

// The pool's own upstream requests.
//
// Besides relaying Codex, the pool calls upstream on each account's behalf:
// token refresh, quota and reset-credit reads, reset-credit redemption, and
// account metadata. Each is a call a native Codex also makes, so each is sent
// the way Codex sends it (SPEC.md 2.3.2): through codex-upstream (Codex's own
// HTTP client, TLS and HTTP/2), to Codex's endpoint, with Codex's header set in
// Codex's order. They were once sent from Go, some with a Chrome browser's
// headers through Node, which no native Codex ever sends.

// pinnedCodexVersion is the Codex release the pool is held to. It must equal
// the tag in upstream/Cargo.toml and the bundled CLI in the Dockerfile; a test
// keeps the three together.
const pinnedCodexVersion = "0.163.0"

// Wire orders captured from a native Codex (see UPSTREAM_HEADER_ORDER in
// codex-upstream): names before the body step, "|", names after it. Codex's
// HTTP client adds `accept: */*` last on its own.
const (
	// backend-client (wham/usage, rate-limit-reset-credits, accounts/check):
	// backend-client/src/client.rs headers().
	codexBackendHeaderOrder = "user-agent,authorization,chatgpt-account-id,x-openai-fedramp|"
	// login auth/manager.rs request_chatgpt_token_refresh: the JSON body step,
	// then the default client's originator and user-agent.
	codexTokenRefreshHeaderOrder = "|originator,user-agent"
	// The models catalog request: provider headers, auth, then the default
	// client's originator and user-agent.
	codexModelsHeaderOrder = "version,authorization,chatgpt-account-id,originator,user-agent|"
)

// clientAgent is the identity a Codex client presents: its User-Agent and
// originator. The pool's own calls for an account carry the agent of the Codex
// that last used that account, since that is the Codex the call stands in for.
type clientAgent struct {
	UserAgent  string    `json:"userAgent"`
	Originator string    `json:"originator"`
	SeenAt     time.Time `json:"seenAt"`
}

// noteClientAgent records the agent of a Codex request routed to accountID,
// and as the pool-wide latest. Only a Codex product token is recorded, so a
// foreign client never becomes the identity of the pool's own calls.
func (a *app) noteClientAgent(accountID string, inbound http.Header) {
	userAgent := strings.TrimSpace(inbound.Get("User-Agent"))
	if codexClientVersion(inbound) == "" {
		return
	}
	agent := clientAgent{UserAgent: userAgent, Originator: strings.TrimSpace(inbound.Get("Originator")), SeenAt: time.Now().UTC()}
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.state.ClientAgents == nil {
		a.state.ClientAgents = map[string]clientAgent{}
	}
	if accountID != "" {
		a.state.ClientAgents[accountID] = agent
	}
	a.state.ClientAgents[""] = agent
}

// clientAgentFor picks the agent for one of the pool's own calls: the Codex
// that last used the account, else the last Codex seen at all (persisted, so a
// restart keeps it). Only before any Codex has ever connected does the pool
// fall back to an agent of its own making, in the shape the bundled Codex CLI
// would send; SPEC.md 2.3.2 records that exception.
func (a *app) clientAgentFor(accountID string) clientAgent {
	a.mu.RLock()
	agent, ok := a.state.ClientAgents[accountID]
	if !ok || agent.UserAgent == "" {
		agent, ok = a.state.ClientAgents[""]
	}
	a.mu.RUnlock()
	if ok && agent.UserAgent != "" {
		if agent.Originator == "" {
			agent.Originator = codexOriginatorFromUserAgent(agent.UserAgent)
		}
		return agent
	}
	return fallbackClientAgent()
}

func codexOriginatorFromUserAgent(userAgent string) string {
	product, _, _ := strings.Cut(userAgent, " ")
	name, _, _ := strings.Cut(product, "/")
	return name
}

func fallbackClientAgent() clientAgent {
	osName, osVersion := "Linux", ""
	if release, err := os.ReadFile("/etc/os-release"); err == nil {
		for _, line := range strings.Split(string(release), "\n") {
			key, value, ok := strings.Cut(line, "=")
			if !ok {
				continue
			}
			value = strings.Trim(value, `"`)
			switch key {
			case "NAME":
				osName = value
			case "VERSION_ID":
				osVersion = value
			}
		}
	}
	platform := strings.TrimSpace(osName + " " + osVersion)
	arch := map[string]string{"amd64": "x86_64", "arm64": "aarch64"}[runtime.GOARCH]
	if arch == "" {
		arch = runtime.GOARCH
	}
	return clientAgent{
		UserAgent:  "codex_exec/" + pinnedCodexVersion + " (" + platform + "; " + arch + ") unknown (codex_exec; " + pinnedCodexVersion + ")",
		Originator: "codex_exec",
	}
}

// codexJSON encodes a body as Codex's serde_json does: compact, and without
// Go's HTML escaping, which writes <, > and & as \u003c and so on where Codex
// writes the characters themselves. wire-capture.sh caught it in a refresh.
func codexJSON(value any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// codexDefaultClientUserAgent is the User-Agent of Codex's default HTTP client,
// which a Codex builds before its user-agent suffix is set: the token refresh
// of a native Codex carries "codex_exec/0.163.0 (OS; arch) terminal" while its
// conversation requests add " (codex_exec; 0.163.0)". The agent the pool
// records comes from conversation requests, so the suffix is removed here.
func codexDefaultClientUserAgent(userAgent string) string {
	open := strings.Index(userAgent, "(")
	last := strings.LastIndex(userAgent, " (")
	if open < 0 || last <= open || !strings.HasSuffix(userAgent, ")") {
		return userAgent
	}
	return userAgent[:last]
}

// codexBackendHeaders is Codex's backend-client header set for an account.
func codexBackendHeaders(agent clientAgent, auth codexAuthInfo, accountID string) http.Header {
	header := http.Header{}
	header.Set("User-Agent", agent.UserAgent)
	header.Set("Authorization", "Bearer "+auth.AccessToken)
	if accountID != "" {
		header.Set("ChatGPT-Account-Id", accountID)
	}
	if auth.FedRAMP {
		header.Set("X-OpenAI-Fedramp", "true")
	}
	return header
}

// sendCodexOwnRequest sends one of the pool's own upstream calls through
// codex-upstream in the given native header order. Without codex-upstream
// (tests) it is sent from Go with the same headers.
func (a *app) sendCodexOwnRequest(ctx context.Context, accountID, method, target, order string, header http.Header, body []byte) (*http.Response, error) {
	viaTransport := a.codexTransportURL != ""
	endpoint := target
	if viaTransport {
		endpoint = a.codexTransportURL + "/forward"
	}
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, endpoint, reader)
	if err != nil {
		return nil, err
	}
	for name, values := range header {
		req.Header[name] = append([]string(nil), values...)
	}
	if !viaTransport {
		return a.client.Do(req)
	}
	req.Header.Set("X-Codex-Upstream-Url", target)
	req.Header.Set("X-Codex-Upstream-Identity", accountID)
	if order != "" {
		req.Header.Set("X-Codex-Upstream-Header-Order", order)
	}
	response, err := a.streamClient.Do(req)
	if err != nil {
		return nil, err
	}
	if response.Header.Get("X-Codex-Upstream-Error") != "" {
		_ = response.Body.Close()
		return nil, errors.New("codex-upstream could not reach upstream")
	}
	return response, nil
}
