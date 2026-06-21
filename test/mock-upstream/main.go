package main

import (
	"encoding/json"
	"log"
	"net/http"
	"strings"
	"sync"
	"time"
)

const responseText = "CODEX_PROVIDER_CONFIG_TOML_OK"

const deviceFailoverText = "DEVICE_AUTH_FAILOVER_B"

type mockServer struct {
	mu     sync.RWMutex
	last   map[string]any
	events []map[string]any
}

func main() {
	server := &mockServer{}
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
	writeEvent(w, flusher, "response.created", map[string]any{"type": "response.created", "response": inProgressResponse(model)})
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

func inProgressResponse(model string) map[string]any {
	return map[string]any{"id": "resp_mock", "object": "response", "created_at": time.Now().Unix(), "status": "in_progress", "model": model, "output": []any{}}
}

func completedResponse(model, text string) map[string]any {
	return map[string]any{
		"id": "resp_mock", "object": "response", "created_at": time.Now().Unix(), "status": "completed", "model": model,
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
