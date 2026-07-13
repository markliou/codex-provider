package main

import (
	"encoding/json"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"
)

const responseText = "CODEX_PROVIDER_CONFIG_TOML_OK"

const deviceFailoverText = "DEVICE_AUTH_FAILOVER_B"

const subagentParentText = "SUBAGENT_PARENT_OK"

const subagentChildText = "SUBAGENT_CHILD_OK"

type mockServer struct {
	mu                  sync.RWMutex
	last                map[string]any
	events              []map[string]any
	spawnIssued         bool
	subagentIntegration bool
}

func main() {
	server := &mockServer{subagentIntegration: os.Getenv("CODEX_MOCK_SUBAGENT_INTEGRATION") == "true"}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{"ok": true})
	})
	mux.HandleFunc("GET /requests", func(w http.ResponseWriter, _ *http.Request) {
		server.mu.RLock()
		defer server.mu.RUnlock()
		writeJSON(w, http.StatusOK, map[string]any{"last": server.last, "events": server.events})
	})
	mux.HandleFunc("POST /v1/responses", server.handleResponse)
	mux.HandleFunc("POST /backend-api/responses", server.handleDeviceAuthResponse)
	mux.HandleFunc("GET /backend-api/wham/usage", server.handleDeviceAuthUsage)
	log.Printf("mock upstream listening on :4010")
	log.Fatal(http.ListenAndServe(":4010", mux))
}

func (s *mockServer) handleResponse(w http.ResponseWriter, r *http.Request) {
	var request map[string]any
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"error": map[string]string{"message": "invalid JSON"}})
		return
	}
	if r.Header.Get("Authorization") == "Bearer cliproxy-test-key" {
		s.handleCliproxyResponse(w, request)
		return
	}
	if s.subagentIntegration {
		s.handleSubagentIntegrationResponse(w, r, request)
		return
	}
	if r.Header.Get("Authorization") != "Bearer upstream-test-key" {
		writeJSON(w, http.StatusUnauthorized, map[string]any{"error": map[string]string{"message": "missing upstream credential"}})
		return
	}
	s.mu.Lock()
	s.last = request
	s.events = append(s.events, map[string]any{"kind": "provider", "status": http.StatusOK})
	s.mu.Unlock()
	model, _ := request["model"].(string)
	response := completedResponse(model, responseText)
	stream, _ := request["stream"].(bool)
	if !stream {
		writeJSON(w, http.StatusOK, response)
		return
	}
	writeStreamingResponse(w, model, response, responseText)
}

func (s *mockServer) handleSubagentIntegrationResponse(w http.ResponseWriter, r *http.Request, request map[string]any) {
	account := ""
	switch r.Header.Get("Authorization") {
	case "Bearer upstream-parent-key":
		account = "parent-account"
	case "Bearer upstream-preferred-key":
		account = "preferred-account"
	default:
		writeJSON(w, http.StatusUnauthorized, map[string]any{"error": map[string]string{"message": "missing subagent test credential"}})
		return
	}
	metadata := requestCodexMetadata(request)
	threadID := metadataString(metadata, "thread_id")
	parentThreadID := metadataString(metadata, "parent_thread_id")
	subagentKind := metadataString(metadata, "subagent_kind")
	isSubagent := parentThreadID != "" || subagentKind != ""
	promptCacheKey, _ := request["prompt_cache_key"].(string)
	event := map[string]any{"kind": "subagent-integration", "account": account, "threadId": threadID, "parentThreadId": parentThreadID, "subagentKind": subagentKind, "promptCacheKey": promptCacheKey}

	s.mu.Lock()
	spawnIssued := s.spawnIssued
	if isSubagent {
		event["role"] = "child"
		s.events = append(s.events, event)
		s.last = request
		s.mu.Unlock()
		model, _ := request["model"].(string)
		response := completedResponseWithID("resp_subagent_child", model, subagentChildText)
		writeStreamingResponse(w, model, response, subagentChildText)
		return
	}
	if !spawnIssued && account == "preferred-account" {
		event["role"] = "parent"
		event["status"] = http.StatusTooManyRequests
		s.events = append(s.events, event)
		s.last = request
		s.mu.Unlock()
		w.Header().Set("Retry-After", "1")
		w.WriteHeader(http.StatusTooManyRequests)
		return
	}
	if !spawnIssued {
		event["role"] = "parent"
		event["status"] = http.StatusOK
		event["spawnSchemaHasForkTurns"] = spawnAgentSchemaHasForkTurns(request["tools"])
		s.events = append(s.events, event)
		s.last = request
		s.spawnIssued = true
		s.mu.Unlock()
		time.Sleep(1500 * time.Millisecond)
		writeStreamingFunctionCall(w, "resp_spawn", "call_spawn", "spawn_agent", `{"message":"Reply with the exact token SUBAGENT_CHILD_OK and nothing else.","task_name":"child_probe","fork_turns":"none"}`)
		return
	}
	event["role"] = "parent"
	event["status"] = http.StatusOK
	spawnOutput, sawSpawnOutput := requestFunctionOutput(request, "call_spawn")
	event["sawSpawnOutput"] = sawSpawnOutput
	event["spawnOutput"] = spawnOutput
	s.events = append(s.events, event)
	s.last = request
	s.mu.Unlock()
	time.Sleep(2 * time.Second)
	model, _ := request["model"].(string)
	response := completedResponseWithID("resp_subagent_parent", model, subagentParentText)
	writeStreamingResponse(w, model, response, subagentParentText)
}

func requestCodexMetadata(request map[string]any) map[string]any {
	clientMetadata, _ := request["client_metadata"].(map[string]any)
	if raw, ok := clientMetadata["x-codex-turn-metadata"].(string); ok {
		var metadata map[string]any
		if json.Unmarshal([]byte(raw), &metadata) == nil {
			return metadata
		}
	}
	return clientMetadata
}

func metadataString(metadata map[string]any, key string) string {
	value, _ := metadata[key].(string)
	return strings.TrimSpace(value)
}

func spawnAgentSchemaHasForkTurns(raw any) bool {
	tools, _ := raw.([]any)
	for _, value := range tools {
		tool, _ := value.(map[string]any)
		if tool["name"] != "spawn_agent" {
			continue
		}
		parameters, _ := tool["parameters"].(map[string]any)
		properties, _ := parameters["properties"].(map[string]any)
		_, ok := properties["fork_turns"]
		return ok
	}
	return false
}

func requestFunctionOutput(request map[string]any, callID string) (string, bool) {
	input, _ := request["input"].([]any)
	for _, value := range input {
		item, _ := value.(map[string]any)
		if item["type"] == "function_call_output" && item["call_id"] == callID {
			output, _ := item["output"].(string)
			return output, true
		}
	}
	return "", false
}

func writeStreamingFunctionCall(w http.ResponseWriter, responseID, callID, name, arguments string) {
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	flusher, _ := w.(http.Flusher)
	writeEvent(w, flusher, "response.created", map[string]any{"type": "response.created", "response": map[string]any{"id": responseID}})
	item := map[string]any{"type": "function_call", "call_id": callID, "name": name, "arguments": arguments}
	writeEvent(w, flusher, "response.output_item.done", map[string]any{"type": "response.output_item.done", "item": item})
	writeEvent(w, flusher, "response.completed", map[string]any{"type": "response.completed", "response": map[string]any{"id": responseID, "output": []any{item}, "usage": map[string]any{"input_tokens": 2048, "input_tokens_details": map[string]any{"cached_tokens": 0}, "output_tokens": 64, "total_tokens": 2112}}})
}

func (s *mockServer) handleCliproxyResponse(w http.ResponseWriter, request map[string]any) {
	model, _ := request["model"].(string)
	account := ""
	if strings.HasPrefix(model, "codex-pool-device-a/") {
		account = "test-acct-a"
	} else if strings.HasPrefix(model, "codex-pool-device-b/") {
		account = "test-acct-b"
	}
	status := http.StatusUnauthorized
	text := ""
	switch account {
	case "test-acct-a":
		status = http.StatusTooManyRequests
	case "test-acct-b":
		status = http.StatusOK
		text = deviceFailoverText
	}
	s.mu.Lock()
	s.last = request
	s.events = append(s.events, map[string]any{"kind": "sidecar", "account": account, "status": status})
	s.mu.Unlock()
	if status == http.StatusTooManyRequests {
		w.Header().Set("Retry-After", "1")
		w.WriteHeader(status)
		return
	}
	if status != http.StatusOK {
		writeJSON(w, status, map[string]any{"error": map[string]string{"message": "sidecar account rejected"}})
		return
	}
	response := completedResponse(model, text)
	if stream, _ := request["stream"].(bool); stream {
		writeStreamingResponse(w, model, response, text)
		return
	}
	writeJSON(w, http.StatusOK, response)
}

func writeStreamingResponse(w http.ResponseWriter, model string, response map[string]any, text string) {
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	flusher, _ := w.(http.Flusher)
	responseID, _ := response["id"].(string)
	writeEvent(w, flusher, "response.created", map[string]any{"type": "response.created", "response": inProgressResponse(responseID, model)})
	message := map[string]any{"id": "msg_mock", "type": "message", "status": "in_progress", "role": "assistant", "content": []any{}}
	writeEvent(w, flusher, "response.output_item.added", map[string]any{"type": "response.output_item.added", "output_index": 0, "item": message})
	writeEvent(w, flusher, "response.content_part.added", map[string]any{"type": "response.content_part.added", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "part": map[string]any{"type": "output_text", "text": "", "annotations": []any{}}})
	writeEvent(w, flusher, "response.output_text.delta", map[string]any{"type": "response.output_text.delta", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "delta": text})
	writeEvent(w, flusher, "response.output_text.done", map[string]any{"type": "response.output_text.done", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "text": text})
	writeEvent(w, flusher, "response.content_part.done", map[string]any{"type": "response.content_part.done", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "part": map[string]any{"type": "output_text", "text": text, "annotations": []any{}}})
	writeEvent(w, flusher, "response.output_item.done", map[string]any{"type": "response.output_item.done", "output_index": 0, "item": response["output"].([]any)[0]})
	writeEvent(w, flusher, "response.completed", map[string]any{"type": "response.completed", "response": response})
	_, _ = w.Write([]byte("data: [DONE]\n\n"))
	if flusher != nil {
		flusher.Flush()
	}
}

func (s *mockServer) handleDeviceAuthResponse(w http.ResponseWriter, r *http.Request) {
	accountID := r.Header.Get("ChatGPT-Account-ID")
	accessToken := r.Header.Get("Authorization")
	status := http.StatusUnauthorized
	responseText := ""
	switch accountID {
	case "test-acct-a":
		if accessToken == "Bearer <test-device-access-a>" {
			status = http.StatusTooManyRequests
		}
	case "test-acct-b":
		if accessToken == "Bearer <test-device-access-b>" {
			status = http.StatusOK
			responseText = deviceFailoverText
		}
	}
	s.mu.Lock()
	s.events = append(s.events, map[string]any{"kind": "device", "account": accountID, "status": status})
	s.mu.Unlock()
	if status == http.StatusTooManyRequests {
		w.Header().Set("Retry-After", "1")
		w.WriteHeader(status)
		return
	}
	if status != http.StatusOK {
		writeJSON(w, status, map[string]any{"error": map[string]string{"message": "device auth rejected"}})
		return
	}
	var request map[string]any
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"error": map[string]string{"message": "invalid JSON"}})
		return
	}
	model, _ := request["model"].(string)
	response := completedResponse(model, responseText)
	if stream, _ := request["stream"].(bool); stream {
		writeStreamingResponse(w, model, response, responseText)
		return
	}
	writeJSON(w, http.StatusOK, response)
}

func (s *mockServer) handleDeviceAuthUsage(w http.ResponseWriter, r *http.Request) {
	accountID := r.Header.Get("ChatGPT-Account-ID")
	if accountID != "test-acct-a" && accountID != "test-acct-b" {
		writeJSON(w, http.StatusUnauthorized, map[string]any{"error": map[string]string{"code": "invalid_account"}})
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"plan_type": "team",
		"rate_limit": map[string]any{
			"allowed":          true,
			"primary_window":   map[string]any{"used_percent": 0, "limit_window_seconds": 18000, "reset_after_seconds": 18000},
			"secondary_window": map[string]any{"used_percent": 0, "limit_window_seconds": 604800, "reset_after_seconds": 604800},
		},
	})
}

func inProgressResponse(responseID, model string) map[string]any {
	return map[string]any{"id": responseID, "object": "response", "created_at": time.Now().Unix(), "status": "in_progress", "model": model, "output": []any{}}
}

func completedResponse(model, text string) map[string]any {
	return completedResponseWithID("resp_mock", model, text)
}

func completedResponseWithID(responseID, model, text string) map[string]any {
	return map[string]any{
		"id": responseID, "object": "response", "created_at": time.Now().Unix(), "status": "completed", "model": model,
		"output": []any{map[string]any{"id": "msg_mock", "type": "message", "status": "completed", "role": "assistant", "content": []any{map[string]any{"type": "output_text", "text": text, "annotations": []any{}}}}},
		"usage":  map[string]any{"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
	}
}

func writeEvent(w http.ResponseWriter, flusher http.Flusher, event string, value any) {
	data, _ := json.Marshal(value)
	_, _ = w.Write([]byte("event: " + event + "\ndata: " + string(data) + "\n\n"))
	if flusher != nil {
		flusher.Flush()
	}
}

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}
