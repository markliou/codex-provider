package main

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net/http"
	"sort"
	"strings"
	"time"

	"github.com/klauspost/compress/zstd"
)

// Native Codex endpoint.
//
// A Codex pointed at the pool as a custom provider sends a reduced request: it
// adds its internal metadata, guardian fields and Codex-backend tools only for
// its built-in OpenAI provider with ChatGPT auth and a base URL ending in
// /backend-api/codex (core/src/client.rs uses_codex_backend,
// model-provider-info supports_codex_backend_routes, model-provider/src/
// provider.rs include_internal_metadata, which configuration cannot set). So
// the pool also serves the Codex backend's own paths, under a key-bearing
// prefix:
//
//	openai_base_url = "https://<pool>/k/<pool-api-key>/backend-api/codex"
//
// Codex then sends exactly what it sends OpenAI, measured equal to a native
// Codex by scripts/conformance/wire-capture.sh. The built-in provider's headers
// cannot be configured, and Codex puts its own ChatGPT login in Authorization,
// so the pool API key travels in the path: Codex only requires the base URL to
// end in /backend-api/codex. The key is checked here and the request then runs
// through the same handlers as /v1, with the client's own credential removed.
// The key must never reach a log line; nothing in the pool logs request paths,
// and this file must keep it that way.

const nativeCodexPrefix = "/k/{key}/backend-api/codex/"

type nativeCodexRequestKey struct{}

// isNativeCodexRequest reports whether r arrived on the native endpoint, whose
// requests the pool forwards exactly as Codex wrote them.
func isNativeCodexRequest(r *http.Request) bool {
	native, _ := r.Context().Value(nativeCodexRequestKey{}).(bool)
	return native
}

// nativeCodexPassthroughPaths are the other Codex-backend paths Codex calls on
// its provider base URL (codex-api/src/endpoint: models, images, memories,
// search, realtime calls). They are not conversation turns the pool routes,
// but a native Codex makes them with the same login as the conversation they
// belong to, so one that names its thread or session goes to the account that
// conversation is on (see nativeCodexPassthroughAccount); only one that names
// none, like the models catalog, takes the account a new session would.
var nativeCodexPassthroughPaths = map[string]bool{
	"models":                   true,
	"images/generations":       true,
	"images/edits":             true,
	"memories/trace_summarize": true,
	"alpha/search":             true,
	"realtime/calls":           true,
}

// nativeCodexResponseHeaders adds what Codex reads from these answers (the
// models catalog's ETag) to the headers relayed from a routed response.
var nativeCodexResponseHeaders = append([]string{"ETag"}, proxyResponseHeaderAllowlist...)

func (a *app) registerNativeCodexRoutes(mux *http.ServeMux) {
	mux.HandleFunc("POST "+nativeCodexPrefix+"responses", a.trackInFlight(a.nativeCodexClient("/v1/responses", a.handleResponses)))
	mux.HandleFunc("POST "+nativeCodexPrefix+"responses/compact", a.trackInFlight(a.nativeCodexClient("/v1/responses/compact", a.handleResponses)))
	// As on /v1, the socket is not in flight; each response running on it is.
	mux.HandleFunc("GET "+nativeCodexPrefix+"responses", a.nativeCodexClient("/v1/responses", a.handleResponsesWebSocket))
	mux.HandleFunc(nativeCodexPrefix+"{rest...}", a.nativeCodexClient("", a.handleNativeCodexPassthrough))
}

// nativeCodexClient authenticates a native-endpoint request by the key in its
// path and hands it to next as the equivalent /v1 request. The client's own
// ChatGPT credential and account id are dropped before anything reads the
// request: the pool sends the selected account's credential instead, and the
// client's must never be relayed, logged, or mistaken for a pool API key.
func (a *app) nativeCodexClient(path string, next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		key := r.PathValue("key")
		if !a.validAPIKey(key) {
			writeOpenAIError(w, http.StatusUnauthorized, "invalid_api_key", "invalid or missing API key")
			return
		}
		a.noteUnrelayedNativeHeaders(r.Header)
		inner := r.Clone(context.WithValue(r.Context(), nativeCodexRequestKey{}, true))
		inner.Header.Del("Authorization")
		inner.Header.Del("ChatGPT-Account-ID")
		inner.Header.Set("Authorization", "Bearer "+key)
		if path != "" {
			inner.URL.Path = path
			inner.URL.RawPath = ""
		}
		// A native Codex zstd-compresses its Responses requests to the Codex
		// backend (codex-api/src/endpoint/responses.rs); the handlers read
		// JSON, and codex-upstream compresses again on the way out.
		if strings.EqualFold(strings.TrimSpace(inner.Header.Get("Content-Encoding")), "zstd") {
			body, err := decodeZstdRequestBody(r.Body)
			if err != nil {
				writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "request body is invalid or too large")
				return
			}
			inner.Body = io.NopCloser(bytes.NewReader(body))
			inner.ContentLength = int64(len(body))
			inner.Header.Del("Content-Encoding")
			inner.Header.Del("Content-Length")
		}
		next(w, inner)
	}
}

// decodeZstdRequestBody bounds both the compressed and the decoded size, so a
// small body cannot expand past the limit every other request is held to.
func decodeZstdRequestBody(body io.Reader) ([]byte, error) {
	decoder, err := zstd.NewReader(io.LimitReader(body, maxRequestBody+1), zstd.WithDecoderConcurrency(1), zstd.WithDecoderMaxMemory(2*maxRequestBody))
	if err != nil {
		return nil, err
	}
	defer decoder.Close()
	decoded, err := io.ReadAll(io.LimitReader(decoder, maxRequestBody+1))
	if err != nil {
		return nil, err
	}
	if len(decoded) > maxRequestBody {
		return nil, errors.New("decoded request body too large")
	}
	return decoded, nil
}

func (a *app) handleNativeCodexPassthrough(w http.ResponseWriter, r *http.Request) {
	rest := strings.Trim(r.PathValue("rest"), "/")
	if !nativeCodexPassthroughPaths[rest] {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "this Codex backend path is not served by the pool")
		return
	}
	body, err := io.ReadAll(io.LimitReader(r.Body, maxRequestBody+1))
	if err != nil || len(body) > maxRequestBody {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "request body is invalid or too large")
		return
	}
	item, ok := a.nativeCodexPassthroughAccount(r)
	if !ok || !a.usesCodexBackendDirectly(item) {
		writeOpenAIError(w, http.StatusServiceUnavailable, "no_available_account", "no account can serve this request")
		return
	}
	target := a.codexResponsesBaseURL() + "/" + rest
	if r.URL.RawQuery != "" {
		target += "?" + r.URL.RawQuery
	}
	auth, err := a.refreshedCodexAuthContext(r.Context(), item)
	if err != nil {
		writeOpenAIError(w, http.StatusServiceUnavailable, "account_auth_failed", "account credential is unavailable")
		return
	}
	// The catalog request has a native order of its own; the others share
	// the Responses request's order, which codex-upstream applies by default.
	order := ""
	if rest == "models" {
		order = codexModelsHeaderOrder
	}
	response, err := a.sendCodexBackend(r.Context(), item, auth, r.Method, target, order, body, r.Header)
	// On 401 refresh once and retry the same account, as a native Codex does.
	if err == nil && response.StatusCode == http.StatusUnauthorized {
		_ = response.Body.Close()
		if auth, err = a.refreshCodexAuthAfterRejection(r.Context(), item, auth.AccessToken); err == nil {
			response, err = a.sendCodexBackend(r.Context(), item, auth, r.Method, target, order, body, r.Header)
		}
	}
	if err != nil {
		writeOpenAIError(w, http.StatusBadGateway, "upstream_unavailable", "upstream request failed")
		return
	}
	defer response.Body.Close()
	for _, header := range nativeCodexResponseHeaders {
		if value := response.Header.Get(header); value != "" {
			w.Header().Set(header, value)
		}
	}
	w.WriteHeader(response.StatusCode)
	_, _ = io.Copy(w, response.Body)
}

// nativeCodexPassthroughAccount is the account a passthrough request belongs
// on: the lineage's or the thread's when the request names a conversation and
// that account can still serve, else the account a new session would take for
// the default model. It only reads routing state; it never creates a binding
// or a placement, since these requests are not turns.
func (a *app) nativeCodexPassthroughAccount(r *http.Request) (account, bool) {
	a.mu.RLock()
	model := a.config.DefaultModel
	a.mu.RUnlock()
	route := a.routingDecision(r, map[string]any{}, model, requestAPIKey(r))
	candidates := []string{route.LineageAccountID}
	if route.Identity.ThreadID != "" || route.Identity.SessionID != "" {
		candidates = append(candidates, a.stickySessionAccountID(route.StickyKey))
	}
	now := time.Now().UTC()
	a.mu.RLock()
	defer a.mu.RUnlock()
	for _, id := range candidates {
		if id == "" {
			continue
		}
		if item := a.accountLocked(id); item != nil && a.usableLocked(*item, model, now) {
			return *item, true
		}
	}
	return a.preferredAccountLocked(model, nil, now)
}

// sendCodexBackend sends one non-conversation request to the Codex backend
// with an account's credential, through codex-upstream when it runs, carrying
// the client's Codex headers as forwardOnce does for a conversation.
func (a *app) sendCodexBackend(ctx context.Context, item account, auth codexAuthInfo, method, target, order string, body []byte, inbound http.Header) (*http.Response, error) {
	a.noteClientAgent(item.ID, inbound)
	a.noteAccountActivity(item.ID)
	viaTransport := a.codexTransportURL != ""
	endpoint := target
	if viaTransport {
		endpoint = a.codexTransportURL + "/forward"
	}
	// Whatever Codex request this is, the account presents its own
	// installation id (installation_id.go).
	installationID, presentID := a.presentedInstallationID(item)
	if presentID && len(body) > 0 {
		body = withInstallationID(body, installationID)
	}
	var reader io.Reader
	if len(body) > 0 {
		reader = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, endpoint, reader)
	if err != nil {
		return nil, err
	}
	for _, header := range []string{"Accept", "Content-Type", "If-None-Match"} {
		if value := inbound.Get(header); value != "" {
			req.Header.Set(header, value)
		}
	}
	forwardCodexMetadataHeaders(req.Header, inbound)
	if presentID {
		setTurnMetadataInstallationID(req.Header, installationID)
	}
	if auth.AccountID == "" {
		auth.AccountID = item.AccountID
	}
	req.Header.Set("Authorization", "Bearer "+auth.AccessToken)
	if auth.AccountID != "" {
		req.Header.Set("ChatGPT-Account-ID", auth.AccountID)
	}
	if auth.FedRAMP {
		req.Header.Set("X-OpenAI-Fedramp", "true")
	}
	if viaTransport {
		req.Header.Set("X-Codex-Upstream-Url", target)
		req.Header.Set("X-Codex-Upstream-Identity", item.ID)
		if order != "" {
			req.Header.Set("X-Codex-Upstream-Header-Order", order)
		}
	}
	response, err := a.streamClient.Do(req)
	if err != nil {
		return nil, err
	}
	if viaTransport && response.Header.Get("X-Codex-Upstream-Error") != "" {
		_ = response.Body.Close()
		return nil, errors.New("codex-upstream could not reach upstream")
	}
	return response, nil
}

// nativeCodexHandledHeaders are the client headers the pool consumes or
// replaces on purpose rather than relaying: framing and transport, the
// client's own credential, and the WebSocket handshake.
var nativeCodexHandledHeaders = map[string]bool{
	"authorization": true, "chatgpt-account-id": true, "content-type": true, "content-length": true,
	"content-encoding": true, "accept": true, "accept-encoding": true, "connection": true, "upgrade": true,
	"host": true, "te": true, "keep-alive": true, "via": true, "x-forwarded-for": true, "x-forwarded-host": true,
	"x-forwarded-proto": true, "sec-websocket-key": true, "sec-websocket-version": true,
	"sec-websocket-extensions": true, "sec-websocket-protocol": true, "cookie": true,
}

// noteUnrelayedNativeHeaders logs, by name only and once per distinct set,
// the headers a native-endpoint client sent that the pool does not relay
// upstream. A native Codex's request must reach upstream whole, and the relay
// is an allowlist: x-oai-attestation, which only the Desktop sends, was once
// dropped this way without a trace. Values are never logged.
func (a *app) noteUnrelayedNativeHeaders(header http.Header) {
	var names []string
	for name := range header {
		lower := strings.ToLower(name)
		if nativeCodexHandledHeaders[lower] || relayedCodexRequestHeader(name) {
			continue
		}
		names = append(names, lower)
	}
	if len(names) == 0 || a.logger == nil {
		return
	}
	sort.Strings(names)
	key := strings.Join(names, ",")
	a.turnStateMu.Lock()
	if a.unrelayedHeaderSets == nil {
		a.unrelayedHeaderSets = map[string]bool{}
	}
	seen := a.unrelayedHeaderSets[key]
	if !seen && len(a.unrelayedHeaderSets) < 256 {
		a.unrelayedHeaderSets[key] = true
	}
	a.turnStateMu.Unlock()
	if !seen {
		a.logger.Printf("native endpoint client headers not relayed upstream: %s (originator %s)", key, telemetryLabel(header.Get("Originator")))
	}
}
