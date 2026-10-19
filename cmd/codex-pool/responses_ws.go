package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"
)

// The Responses WebSocket is how a native Codex signed in with ChatGPT talks
// to the Codex backend: one long-lived socket per session, a generate=false
// prewarm that uploads tools and instructions once, and then requests that
// carry only new input plus the previous_response_id. That state lives on the
// upstream connection, which is what spares the backend from re-reading the
// whole conversation on every request, so the pool relays the socket instead
// of turning it back into full HTTP requests.
//
// One client socket is bound to one account's upstream socket, opened by
// codex-upstream with Codex's own connector. Messages pass through byte for
// byte in both directions. The account is chosen at the first response.create
// (the handshake carries no model) and kept for the connection. When the next
// request belongs on another account (a lineage move, a cooled-down account)
// or the account fails mid-connection, the pool closes the client socket
// instead of switching upstream sockets underneath it: incremental requests
// reference responses that exist only on the old connection, and a native
// Codex answers a closed socket by reconnecting and resending the request in
// full, which the pool then routes like any new request.

type wsSession struct {
	a         *app
	client    *wsConn
	handshake *http.Request
	apiKey    string

	mu       sync.Mutex
	upstream *wsConn
	account  account
	active   *wsResponse
	closed   chan struct{}
}

type wsResponse struct {
	route      routingDecision
	model      string
	accountID  string
	warmup     bool
	info       proxyResponseInfo
	throughput *throughputMeasurement
	finished   sync.Once

	// The fields below belong to the upstream relay goroutine. message is the
	// request as sent, kept so an overload can be retried with the same bytes;
	// buffered holds the lifecycle preamble Codex has not been shown yet.
	message         []byte
	buffered        [][]byte
	committed       bool
	overloadRetries int
}

// handleResponsesWebSocket serves GET /v1/responses upgrades. Without
// codex-upstream (sidecar mode, tests) it answers 426, which a native Codex
// treats as "use HTTP" for the session.
func (a *app) handleResponsesWebSocket(w http.ResponseWriter, r *http.Request) {
	if a.codexTransportURL == "" || !isWebSocketUpgrade(r) {
		writeOpenAIError(w, http.StatusUpgradeRequired, "websocket_unavailable", "this pool serves Responses over HTTP")
		return
	}
	client, err := acceptWebSocket(w, r, nil)
	if err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid websocket handshake")
		return
	}
	session := &wsSession{a: a, client: client, handshake: r, apiKey: requestAPIKey(r), closed: make(chan struct{})}
	session.run()
}

// wsClientPingInterval keeps the client leg alive between messages. That leg
// crosses the internet to a remote Codex, through NATs and proxies that drop a
// silent connection, while upstream can be silent for as long as a model
// reasons. Pings are control frames Codex answers itself; they never reach
// upstream.
const wsClientPingInterval = 20 * time.Second

func (s *wsSession) run() {
	done := make(chan struct{})
	defer close(done)
	defer s.shutdown()
	go func() {
		ticker := time.NewTicker(wsClientPingInterval)
		defer ticker.Stop()
		for {
			select {
			case <-done:
				return
			case <-ticker.C:
				if s.client.writeFrame(wsOpPing, nil) != nil {
					return
				}
			}
		}
	}()
	for {
		opcode, message, err := s.client.ReadMessage()
		if err != nil {
			return
		}
		var payload map[string]any
		if opcode == wsOpText && json.Unmarshal(message, &payload) == nil && payload["type"] == "response.create" {
			if !s.startResponse(message, payload) {
				return
			}
			continue
		}
		// Anything else (an interrupt, a binary frame) belongs to the response
		// already running on the bound upstream socket.
		s.mu.Lock()
		upstream := s.upstream
		s.mu.Unlock()
		if upstream == nil || upstream.WriteMessage(opcode, message) != nil {
			return
		}
	}
}

func (s *wsSession) startResponse(message []byte, payload map[string]any) bool {
	a := s.a
	requested, _ := payload["model"].(string)
	a.mu.RLock()
	defaultModel := a.config.DefaultModel
	a.mu.RUnlock()
	if requested == "" {
		requested = defaultModel
	}
	model, tier := parseModel(requested)
	a.mu.RLock()
	if alias, ok := a.config.ModelAliases[model]; ok {
		model = alias
	}
	a.mu.RUnlock()
	// Only a model suffix or alias changes the message; everything else is
	// forwarded as Codex sent it.
	if model != requested || (tier != "" && tier != "none") {
		payload["model"] = model
		if tier != "" && tier != "none" {
			payload["reasoning"] = map[string]any{"effort": tier}
		}
		rewritten, err := json.Marshal(payload)
		if err != nil {
			return false
		}
		message = rewritten
	}
	route := a.routingDecision(s.handshake, payload, model, s.apiKey)
	warmup := payload["generate"] == false

	s.mu.Lock()
	upstream := s.upstream
	bound := s.account
	s.mu.Unlock()
	if upstream != nil {
		candidate, err := a.selectAccountForRoute(&route, model, map[string]bool{})
		if err != nil || candidate.ID != bound.ID {
			// Nothing of this request reached upstream; Codex resends it in full
			// after reconnecting, and routing places it then.
			s.client.CloseWith(1012, "account changed")
			return false
		}
	} else if !s.dial(&route, model, message) {
		return false
	}
	a.noteSessionPlacement(route.BalanceKey, bound.ID)

	s.mu.Lock()
	response := &wsResponse{route: route, model: model, accountID: s.account.ID, warmup: warmup, message: message}
	if !warmup {
		response.throughput = a.beginThroughputMeasurement(true)
		response.throughput.AccountID = s.account.ID
	}
	s.active = response
	upstream = s.upstream
	s.mu.Unlock()
	// A running response holds the drain and the deploy idle wait like an
	// HTTP request; an idle socket between turns does not.
	a.inFlight.Add(1)
	if err := upstream.WriteMessage(wsOpText, message); err != nil {
		s.finish(response, false)
		return false
	}
	return true
}

// dial binds the session to an account's upstream socket, failing over the
// same way an HTTP request does before anything reaches upstream.
func (s *wsSession) dial(route *routingDecision, model string, firstMessage []byte) bool {
	a := s.a
	excluded := map[string]bool{}
	status, code, detail := http.StatusServiceUnavailable, "all_accounts_cooling_down", "no eligible upstream account"
	for attempt := 0; attempt < a.proxyAttemptLimit(); attempt++ {
		candidate, err := a.selectAccountForRoute(route, model, excluded)
		if err != nil {
			break
		}
		excluded[candidate.ID] = true
		a.noteSessionPlacement(route.BalanceKey, candidate.ID)
		if !a.usesCodexBackendDirectly(candidate) {
			// API-key providers have no Codex WebSocket; their sessions use HTTP.
			continue
		}
		conn, refusal, err := s.a.dialCodexWebSocket(s.handshake.Context(), candidate, s.handshake.Header, firstMessage)
		if err != nil {
			a.markFailure(candidate.ID, model, "upstream_transport_error", 30*time.Second)
			status, code, detail = http.StatusBadGateway, "bad_gateway", "upstream websocket unreachable"
			continue
		}
		if refusal != nil {
			body, _ := io.ReadAll(io.LimitReader(refusal.Body, maxRequestBody))
			_ = refusal.Body.Close()
			status = refusal.StatusCode
			code = codeOr(extractUpstreamErrorCode(body), "upstream_refused")
			detail = "upstream refused the websocket"
			switch {
			case upstreamAuthFailureStatus(status):
				a.markAccountAuthFailure(candidate.ID, model, code)
			case status == http.StatusTooManyRequests:
				a.markFailure(candidate.ID, model, "rate_limited", retryAfter(refusal.Header.Get("Retry-After")))
			case status == http.StatusUpgradeRequired:
				// Upstream wants HTTP; the client is told the same below.
			default:
				a.markFailure(candidate.ID, model, "upstream_5xx", 30*time.Second)
			}
			continue
		}
		s.mu.Lock()
		s.upstream = conn
		s.account = candidate
		s.mu.Unlock()
		go s.relayUpstream(conn)
		return true
	}
	// The wrapped error event is what the Codex backend itself sends on a
	// socket; Codex maps its status the way it maps an HTTP status, so a 426
	// still means "fall back to HTTP" and a 429 or 5xx is retried.
	event, _ := json.Marshal(map[string]any{"type": "error", "status": status, "error": map[string]any{"code": code, "message": detail}})
	_ = s.client.WriteMessage(wsOpText, event)
	s.client.CloseWith(1011, "no upstream")
	return false
}

// wsPrecommitMaxBlocks bounds the lifecycle preamble held back from Codex.
const wsPrecommitMaxBlocks = 64

// relayUpstream forwards upstream events to Codex with the same pre-commit
// rule as the HTTP stream. Lifecycle events are held until the first event
// carrying content, because a native Codex does not retry an overload that
// arrives without retry advice: it shows "Selected model is at capacity" and
// stops the turn. While nothing has reached Codex, an overload is retried on
// this socket, so on this account and against the same connection state, by
// resending the identical request after the same-account backoff, which is
// exactly what a Codex retry would send. When that schedule is spent, or the
// account's allowance or credential failed, the account is cooled and the
// socket closed without showing the failure: Codex treats a dropped socket as
// retryable, reconnects and resends in full, and routing moves the whole
// lineage. A failure after content has reached Codex is relayed as it is.
func (s *wsSession) relayUpstream(upstream *wsConn) {
	for {
		opcode, message, err := upstream.ReadMessage()
		if err != nil {
			s.upstreamGone(upstream)
			return
		}
		s.mu.Lock()
		response := s.active
		s.mu.Unlock()
		if opcode != wsOpText || response == nil || response.committed {
			if s.client.WriteMessage(opcode, message) != nil {
				upstream.Close()
				return
			}
			if opcode == wsOpText && response != nil && !s.record(response, message) {
				upstream.Close()
				return
			}
			continue
		}
		block := "data: " + string(message) + "\n\n"
		info := responseInfoFromSSEBlock(block)
		if info.TerminalEvent == "" && precommitLifecycleSSEBlock(block) && len(response.buffered) < wsPrecommitMaxBlocks {
			response.info.merge(info)
			response.buffered = append(response.buffered, message)
			continue
		}
		if info.TerminalEvent != "" && info.TerminalEvent != "response.completed" {
			if transientOverloadCode(info.TerminalErrorCode) && response.overloadRetries < overloadSameAccountRetryLimit {
				response.overloadRetries++
				response.buffered = nil
				response.info = proxyResponseInfo{}
				s.a.markRetryableTerminalResponseFailure(response.accountID, response.model, info, false)
				select {
				case <-s.closed:
					return
				case <-time.After(overloadRetryBackoff << (response.overloadRetries - 1)):
				}
				if upstream.WriteMessage(wsOpText, response.message) != nil {
					s.upstreamGone(upstream)
					return
				}
				continue
			}
			if retryableTerminalFailureClass(info.TerminalFailureClass) || info.TerminalFailureClass == "authentication" {
				s.a.markRetryableTerminalResponseFailure(response.accountID, response.model, info, true)
				s.mu.Lock()
				s.active = nil
				s.mu.Unlock()
				s.finish(response, false)
				s.client.CloseWith(1012, "upstream account unavailable")
				upstream.Close()
				return
			}
		}
		// First content, or a failure that is the caller's own: release the
		// held preamble and stream from here on.
		response.committed = true
		for _, held := range response.buffered {
			if s.client.WriteMessage(wsOpText, held) != nil {
				upstream.Close()
				return
			}
		}
		response.buffered = nil
		if s.client.WriteMessage(wsOpText, message) != nil || !s.record(response, message) {
			upstream.Close()
			return
		}
	}
}

// record accumulates a relayed event into the response and finishes it at its
// terminal event. It reports false when the socket was closed because the
// account must not serve the next request.
func (s *wsSession) record(response *wsResponse, message []byte) bool {
	info := responseInfoFromSSEBlock("data: " + string(message) + "\n\n")
	response.info.merge(info)
	if info.TerminalEvent == "" {
		return true
	}
	s.mu.Lock()
	if s.active == response {
		s.active = nil
	}
	s.mu.Unlock()
	failed := info.TerminalEvent != "response.completed"
	overloaded := failed && transientOverloadCode(response.info.TerminalErrorCode)
	s.finishResponse(response, true, !overloaded)
	if failed && !overloaded && (retryableTerminalFailureClass(response.info.TerminalFailureClass) || response.info.TerminalFailureClass == "authentication") {
		// The failure reached Codex after output; the next request must not
		// land on this account's socket.
		s.client.CloseWith(1012, "upstream account unavailable")
		return false
	}
	return true
}

// finish records a response exactly once. delivered is false when the
// request never reached upstream or the socket died before a terminal event.
func (s *wsSession) finish(response *wsResponse, delivered bool) {
	s.finishResponse(response, delivered, true)
}

// finishResponse is finish with control over the account cooldown a failure
// applies; an overload still inside the same-account schedule applies none.
func (s *wsSession) finishResponse(response *wsResponse, delivered, coolDown bool) {
	response.finished.Do(func() {
		a := s.a
		// Released last, after the response is recorded, so the drain never
		// sees an idle pool while a result is still being saved.
		defer a.inFlight.Add(-1)
		if response.warmup {
			// A prewarm is connection setup, not a model request.
			return
		}
		info := response.info
		info.CompletedAt = time.Now().UTC()
		measurement := response.throughput
		measurement.CompletedAt = info.CompletedAt
		measurement.Usage = info.Usage
		switch {
		case !delivered:
			measurement.Cancelled = true
			a.finishThroughputMeasurement(measurement)
		case info.TerminalEvent == "response.completed":
			measurement.Success = true
			a.markSuccessWithMeasurement(response.route, response.model, response.accountID, info, measurement)
		default:
			a.markTerminalResponseFailure(response.route, response.model, response.accountID, info, measurement, coolDown)
		}
	})
}

func (s *wsSession) upstreamGone(upstream *wsConn) {
	s.mu.Lock()
	response := s.active
	s.active = nil
	accountID := s.account.ID
	s.mu.Unlock()
	if response != nil {
		s.a.markFailure(accountID, response.model, "upstream_transport_error", 30*time.Second)
		s.finish(response, false)
	}
	upstream.Close()
	s.client.CloseWith(1011, "upstream closed")
}

func (s *wsSession) shutdown() {
	close(s.closed)
	s.mu.Lock()
	upstream := s.upstream
	response := s.active
	s.active = nil
	s.mu.Unlock()
	if response != nil {
		s.finish(response, false)
	}
	if upstream != nil {
		upstream.CloseWith(1000, "")
	}
	s.client.Close()
}

// dialCodexWebSocket opens an account's upstream Responses socket through
// codex-upstream with the handshake a native Codex sends: the client's own
// Codex headers, the account's credential and ChatGPT-Account-ID, and the
// routing hint and version a native Codex adds for the Codex backend. A native
// handshake carries no turn state. A 401 is answered like on HTTP: refresh
// once and redial the same account.
func (a *app) dialCodexWebSocket(ctx context.Context, candidate account, clientHeaders http.Header, firstMessage []byte) (*wsConn, *http.Response, error) {
	auth, err := a.refreshedCodexAuthContext(ctx, candidate)
	if err != nil {
		return nil, nil, err
	}
	conn, refusal, err := a.dialCodexWebSocketOnce(candidate, clientHeaders, firstMessage, auth)
	if err != nil || refusal == nil || refusal.StatusCode != http.StatusUnauthorized {
		return conn, refusal, err
	}
	refreshed, refreshErr := a.refreshCodexAuthAfterRejection(ctx, candidate, auth.AccessToken)
	if refreshErr != nil || refreshed.AccessToken == "" || refreshed.AccessToken == auth.AccessToken {
		return nil, refusal, nil
	}
	return a.dialCodexWebSocketOnce(candidate, clientHeaders, firstMessage, refreshed)
}

func (a *app) dialCodexWebSocketOnce(candidate account, clientHeaders http.Header, firstMessage []byte, auth codexAuthInfo) (*wsConn, *http.Response, error) {
	headers := http.Header{}
	forwardCodexMetadataHeaders(headers, clientHeaders)
	headers.Del("X-Codex-Turn-State")
	if auth.AccountID == "" {
		auth.AccountID = candidate.AccountID
	}
	headers.Set("Authorization", "Bearer "+auth.AccessToken)
	if auth.AccountID != "" {
		headers.Set("ChatGPT-Account-ID", auth.AccountID)
	}
	if auth.FedRAMP {
		headers.Set("X-OpenAI-Fedramp", "true")
	}
	if hint := codexRoutingHint(firstMessage, clientHeaders); hint != "" {
		headers.Set("X-Codex-Routing-Hint", hint)
	}
	if version := codexClientVersion(clientHeaders); version != "" && headers.Get("Version") == "" {
		headers.Set("Version", version)
	}
	upstreamURL := a.codexResponsesBaseURL() + "/responses"
	upstreamURL = "ws" + strings.TrimPrefix(upstreamURL, "http")
	headers.Set("X-Codex-Upstream-Url", upstreamURL)
	headers.Set("X-Codex-Upstream-Identity", candidate.ID)
	transport := "ws" + strings.TrimPrefix(a.codexTransportURL, "http") + "/ws"
	conn, response, err := dialWebSocket(transport, headers)
	if err != nil {
		return nil, nil, err
	}
	if response != nil && conn == nil && response.Header.Get("X-Codex-Upstream-Error") != "" {
		_ = response.Body.Close()
		return nil, nil, io.ErrUnexpectedEOF
	}
	if conn == nil {
		return nil, response, nil
	}
	return conn, nil, nil
}
