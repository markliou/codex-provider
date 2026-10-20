package main

import (
	"bytes"
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
// byte in both directions.
//
// The account is chosen at the handshake, from the session, thread and model
// the handshake names (X-Codex-Routing-Hint, else X-Codex-Turn-Metadata), and
// the upstream socket is opened before the client's is accepted. That order is
// what lets the client receive the upstream handshake's own headers, as a
// native Codex does: Codex reads its turn state from the handshake response
// alone on a socket (codex-api endpoint/responses_websocket.rs; its event
// parser takes turn state only from response.metadata, which a socket does not
// carry) and sends it back in every later message's client_metadata, and it
// reads x-reasoning-included there, which changes its token accounting and so
// when it compacts. Accepting first and dialing at the first message, as the
// pool once did, left Codex with neither, so its later messages differed from
// a native Codex's.
//
// When the next request belongs on another account (a lineage move, a
// cooled-down account) or the account fails mid-connection, the pool closes
// the client socket instead of switching upstream sockets underneath it:
// incremental requests reference responses that exist only on the old
// connection, and a native Codex answers a closed socket by reconnecting and
// resending the request in full, which the pool then routes like any new
// request. The one exception is the first request on a socket, which may
// settle the account differently from the handshake's guess (a model the
// guessed account cannot serve, a response chain bound elsewhere): nothing
// has gone out on that upstream socket yet, so it is replaced before the
// request is sent, rather than sending Codex to reconnect into the same guess.

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
	// upstreamHeaders is the current upstream socket's handshake response.
	upstreamHeaders http.Header
	// sent is true once a request has gone out on the current upstream socket.
	sent bool
}

// wsRefusal is why no upstream socket could be opened for a handshake.
type wsRefusal struct {
	status       int
	code, detail string
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
	if strings.TrimSpace(r.Header.Get("Sec-WebSocket-Key")) == "" || r.Header.Get("Sec-WebSocket-Version") != "13" {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid websocket handshake")
		return
	}
	session := &wsSession{a: a, handshake: r, apiKey: requestAPIKey(r), closed: make(chan struct{})}
	model := a.wsHandshakeModel(r)
	route := a.routingDecision(r, map[string]any{}, model, session.apiKey)
	// No account could take the socket: answer the handshake itself, as the
	// Codex backend does when it refuses one, so Codex maps the status (426
	// means HTTP, 429 and 5xx are retried) before any socket exists.
	if refusal := session.dial(&route, model); refusal != nil {
		writeOpenAIError(w, refusal.status, refusal.code, refusal.detail)
		return
	}
	client, err := acceptWebSocket(w, r, session.clientHandshakeHeaders())
	if err != nil {
		session.upstream.Close()
		return
	}
	session.client = client
	go session.relayUpstream(session.upstream)
	session.run()
}

// wsHandshakeHeaders are the upstream handshake response headers a native
// Codex reads (codex-api endpoint/responses_websocket.rs): turn state, whether
// reasoning is included, and the serving model. X-Models-Etag joins them only
// on the native endpoint, where the catalog is upstream's (see
// relayNativeCodexResponseHeaders).
var wsHandshakeHeaders = []string{"X-Codex-Turn-State", "X-Reasoning-Included", "OpenAI-Model"}

func (s *wsSession) clientHandshakeHeaders() http.Header {
	headers := http.Header{}
	names := wsHandshakeHeaders
	if isNativeCodexRequest(s.handshake) {
		names = append(append([]string(nil), names...), "X-Models-Etag")
	}
	for _, name := range names {
		if value := s.upstreamHeaders.Get(name); value != "" {
			headers.Set(name, value)
		}
	}
	return headers
}

// wsHandshakeModel is the model a handshake names: the routing hint a native
// Codex sends the Codex backend, else the turn metadata every Codex sends,
// else the pool's default, with the pool's alias applied for routing only.
func (a *app) wsHandshakeModel(r *http.Request) string {
	model := ""
	for _, part := range strings.Split(r.Header.Get("X-Codex-Routing-Hint"), ";") {
		if value, ok := strings.CutPrefix(strings.TrimSpace(part), "model="); ok {
			model = strings.TrimSpace(value)
		}
	}
	if model == "" {
		model = metadataString(metadataObject(r.Header.Get("X-Codex-Turn-Metadata")), "model")
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	if model == "" {
		model = a.config.DefaultModel
	}
	model, _ = parseModel(model)
	if alias, ok := a.config.ModelAliases[model]; ok {
		model = alias
	}
	return model
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
	bound := s.account
	sent := s.sent
	s.mu.Unlock()
	candidate, err := a.selectAccountForRoute(&route, model, map[string]bool{})
	if err != nil || candidate.ID != bound.ID {
		if sent {
			// Nothing of this request reached upstream; Codex resends it in full
			// after reconnecting, and routing places it then.
			s.client.CloseWith(1012, "account changed")
			return false
		}
		// The first request settled the account differently from the
		// handshake's guess, and nothing has gone out on this socket: move it.
		if !s.redial(&route, model) {
			return false
		}
	}
	s.mu.Lock()
	bound = s.account
	s.mu.Unlock()
	a.noteSessionPlacement(route.BalanceKey, bound.ID)
	message = s.gateTurnState(message, payload, route)

	s.mu.Lock()
	response := &wsResponse{route: route, model: model, accountID: s.account.ID, warmup: warmup, message: message}
	if !warmup {
		response.throughput = a.beginThroughputMeasurement(true)
		response.throughput.AccountID = s.account.ID
	}
	s.active = response
	s.sent = true
	upstream := s.upstream
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
// same way an HTTP request does before anything reaches upstream. It does not
// start the relay; the caller does once the client's socket exists.
func (s *wsSession) dial(route *routingDecision, model string) *wsRefusal {
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
		conn, handshake, refusal, err := s.a.dialCodexWebSocket(s.handshake.Context(), candidate, s.handshake.Header, model)
		// A socket that could not be opened at all is retried on the same
		// account before the lineage moves (transportSameAccountRetryLimit).
		for retry := 1; err != nil && retry <= transportSameAccountRetryLimit && s.handshake.Context().Err() == nil; retry++ {
			a.logger.Printf("upstream socket unreachable for %s (try %d of %d on this account)", candidate.ID, retry, transportSameAccountRetryLimit+1)
			select {
			case <-s.handshake.Context().Done():
			case <-time.After(transportRetryBackoff << (retry - 1)):
			}
			conn, handshake, refusal, err = s.a.dialCodexWebSocket(s.handshake.Context(), candidate, s.handshake.Header, model)
		}
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
				// Upstream wants HTTP, which is the backend's answer, not this
				// account's: tell the client at once, as a native Codex hears it
				// on its one handshake, rather than knocking with every account.
				return &wsRefusal{status: status, code: code, detail: detail}
			default:
				a.markFailure(candidate.ID, model, "upstream_5xx", 30*time.Second)
			}
			continue
		}
		if turnState := handshake.Get("X-Codex-Turn-State"); turnState != "" {
			a.noteTurnStateIssuer(turnState, candidate.ID)
		}
		s.mu.Lock()
		s.upstream = conn
		s.account = candidate
		s.upstreamHeaders = handshake
		s.sent = false
		s.mu.Unlock()
		return nil
	}
	return &wsRefusal{status: status, code: code, detail: detail}
}

// redial replaces the upstream socket for the first request on a client
// socket (see the comment at the top of this file). The old socket's relay
// sees it close and, no longer current, exits without touching the client.
func (s *wsSession) redial(route *routingDecision, model string) bool {
	s.mu.Lock()
	old := s.upstream
	s.upstream = nil
	s.mu.Unlock()
	if old != nil {
		old.Close()
	}
	if refusal := s.dial(route, model); refusal != nil {
		// The wrapped error event is what the Codex backend itself sends on a
		// socket; Codex maps its status the way it maps an HTTP status, so a
		// 426 still means "fall back to HTTP" and a 429 or 5xx is retried.
		event, _ := json.Marshal(map[string]any{"type": "error", "status": refusal.status, "error": map[string]any{"code": refusal.code, "message": refusal.detail}})
		_ = s.client.WriteMessage(wsOpText, event)
		s.client.CloseWith(1011, "no upstream")
		return false
	}
	s.mu.Lock()
	upstream := s.upstream
	s.mu.Unlock()
	go s.relayUpstream(upstream)
	return true
}

// gateTurnState applies the HTTP turn-state rule (turnStateForAttempt) to a
// socket, where Codex carries the turn state in the message's client_metadata
// rather than a header: only the account that issued it may receive it. After
// the session moved accounts, Codex still holds the old account's token for
// the turn, and a native Codex never presents one account's turn token to
// another, so it is removed from the message, leaving every other byte as
// Codex wrote it.
func (s *wsSession) gateTurnState(message []byte, payload map[string]any, route routingDecision) []byte {
	metadata, _ := payload["client_metadata"].(map[string]any)
	value, _ := metadata["x-codex-turn-state"].(string)
	if value == "" {
		return message
	}
	s.mu.Lock()
	bound := s.account.ID
	s.mu.Unlock()
	issuer := s.a.turnStateIssuer(value)
	if issuer == "" {
		issuer = s.a.stickySessionAccountID(route.StickyKey)
	}
	if issuer == bound {
		return message
	}
	return withoutClientMetadataField(message, payload, "x-codex-turn-state", value)
}

// withoutClientMetadataField removes one client_metadata field from a message
// without re-encoding the rest. Codex writes compact JSON, so the field is a
// literal "key":"value" pair; only if that pair cannot be found is the message
// re-encoded.
func withoutClientMetadataField(message []byte, payload map[string]any, key, value string) []byte {
	pair, err := codexJSON(map[string]string{key: value})
	if err == nil {
		pair = pair[1 : len(pair)-1]
		for _, candidate := range [][]byte{append([]byte(","), pair...), append(append([]byte(nil), pair...), ','), pair} {
			if index := bytes.Index(message, candidate); index >= 0 {
				edited := append(append([]byte(nil), message[:index]...), message[index+len(candidate):]...)
				var check map[string]any
				if json.Unmarshal(edited, &check) == nil {
					if metadata, _ := check["client_metadata"].(map[string]any); metadata != nil {
						if _, still := metadata[key]; !still {
							return edited
						}
					}
				}
			}
		}
	}
	if metadata, _ := payload["client_metadata"].(map[string]any); metadata != nil {
		delete(metadata, key)
	}
	if encoded, err := codexJSON(payload); err == nil {
		return encoded
	}
	return message
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
	// The account this socket belongs to, fixed for the socket's life even if
	// the session later redials elsewhere.
	s.mu.Lock()
	accountID := s.account.ID
	s.mu.Unlock()
	for {
		opcode, message, err := upstream.ReadMessage()
		if err != nil {
			s.upstreamGone(upstream)
			return
		}
		s.mu.Lock()
		response := s.active
		s.mu.Unlock()
		if opcode == wsOpText {
			s.a.noteEventTurnState(message, accountID)
		}
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
	if s.upstream != upstream {
		// A socket replaced by redial: the session has already moved on.
		s.mu.Unlock()
		upstream.Close()
		return
	}
	response := s.active
	s.active = nil
	accountID := s.account.ID
	s.mu.Unlock()
	if response != nil {
		// Recorded, but without a cooldown: a dropped socket is no evidence
		// against the account, and a native Codex reconnects to the same one.
		// A cooldown here sent Codex's immediate reconnect, and so the whole
		// lineage, to another account; a reconnect that cannot open a socket
		// is retried on this account and only then moves (see dial).
		s.a.markFailure(accountID, response.model, "upstream_transport_error", 0)
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
// handshake carries no turn state (core client.rs build_websocket_headers).
// It returns the upstream handshake's response headers. A 401 is answered like
// on HTTP: refresh once and redial the same account.
func (a *app) dialCodexWebSocket(ctx context.Context, candidate account, clientHeaders http.Header, model string) (*wsConn, http.Header, *http.Response, error) {
	auth, err := a.refreshedCodexAuthContext(ctx, candidate)
	if err != nil {
		return nil, nil, nil, err
	}
	conn, handshake, refusal, err := a.dialCodexWebSocketOnce(candidate, clientHeaders, model, auth)
	if err != nil || refusal == nil || refusal.StatusCode != http.StatusUnauthorized {
		return conn, handshake, refusal, err
	}
	refreshed, refreshErr := a.refreshCodexAuthAfterRejection(ctx, candidate, auth.AccessToken)
	if refreshErr != nil || refreshed.AccessToken == "" || refreshed.AccessToken == auth.AccessToken {
		return nil, nil, refusal, nil
	}
	return a.dialCodexWebSocketOnce(candidate, clientHeaders, model, refreshed)
}

func (a *app) dialCodexWebSocketOnce(candidate account, clientHeaders http.Header, model string, auth codexAuthInfo) (*wsConn, http.Header, *http.Response, error) {
	a.noteClientAgent(candidate.ID, clientHeaders)
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
	// A native Codex names the model in its handshake's routing hint; a
	// client that sent none gets the handshake's model. The service tier is
	// only in the request body, so such a client's hint carries no tier.
	if strings.TrimSpace(clientHeaders.Get("X-Codex-Routing-Hint")) == "" && !strings.EqualFold(strings.TrimSpace(clientHeaders.Get("X-Codex-Guardian")), "reviewer") && model != "" {
		headers.Set("X-Codex-Routing-Hint", "model="+model)
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
		return nil, nil, nil, err
	}
	if response != nil && conn == nil && response.Header.Get("X-Codex-Upstream-Error") != "" {
		_ = response.Body.Close()
		return nil, nil, nil, io.ErrUnexpectedEOF
	}
	if conn == nil {
		return nil, nil, response, nil
	}
	return conn, response.Header, nil, nil
}

// turnStateIssuer records which account issued a turn state, and when.
type turnStateIssuer struct {
	accountID string
	at        time.Time
}

// A turn state lives for one Codex turn; a day is far longer, and the bound
// keeps the map small however many sockets come and go.
const (
	turnStateIssuerTTL = 24 * time.Hour
	turnStateIssuerMax = 4096
)

func (a *app) noteTurnStateIssuer(value, accountID string) {
	if value == "" || accountID == "" {
		return
	}
	now := time.Now()
	a.turnStateMu.Lock()
	defer a.turnStateMu.Unlock()
	if a.turnStateIssuers == nil {
		a.turnStateIssuers = map[string]turnStateIssuer{}
	}
	if len(a.turnStateIssuers) >= turnStateIssuerMax {
		for key, issuer := range a.turnStateIssuers {
			if now.Sub(issuer.at) > turnStateIssuerTTL || len(a.turnStateIssuers) >= turnStateIssuerMax {
				delete(a.turnStateIssuers, key)
			}
		}
	}
	a.turnStateIssuers[value] = turnStateIssuer{accountID: accountID, at: now}
}

func (a *app) turnStateIssuer(value string) string {
	a.turnStateMu.Lock()
	defer a.turnStateMu.Unlock()
	issuer, ok := a.turnStateIssuers[value]
	if !ok || time.Since(issuer.at) > turnStateIssuerTTL {
		return ""
	}
	return issuer.accountID
}

// noteEventTurnState records a turn state an upstream event carries in its
// headers, so a later message presenting it is known to be this account's.
func (a *app) noteEventTurnState(message []byte, accountID string) {
	if !bytes.Contains(message, []byte("x-codex-turn-state")) {
		return
	}
	var event struct {
		Headers map[string]any `json:"headers"`
	}
	if json.Unmarshal(message, &event) != nil {
		return
	}
	for name, value := range event.Headers {
		if text, ok := value.(string); ok && strings.EqualFold(name, "x-codex-turn-state") {
			a.noteTurnStateIssuer(text, accountID)
		}
	}
}
