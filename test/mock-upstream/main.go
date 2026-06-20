package main

import (
	"encoding/json"
	"log"
	"net/http"
	"sync"
	"time"
)

const responseText = "CODEX_PROVIDER_CONFIG_TOML_OK"

type mockServer struct {
	mu   sync.RWMutex
	last map[string]any
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
		writeJSON(w, http.StatusOK, server.last)
	})
	mux.HandleFunc("POST /v1/responses", server.handleResponse)
	log.Printf("mock upstream listening on :4010")
	log.Fatal(http.ListenAndServe(":4010", mux))
}

func (s *mockServer) handleResponse(w http.ResponseWriter, r *http.Request) {
	if r.Header.Get("Authorization") != "Bearer upstream-test-key" {
		writeJSON(w, http.StatusUnauthorized, map[string]any{"error": map[string]string{"message": "missing upstream credential"}})
		return
	}
	var request map[string]any
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]any{"error": map[string]string{"message": "invalid JSON"}})
		return
	}
	s.mu.Lock()
	s.last = request
	s.mu.Unlock()
	model, _ := request["model"].(string)
	response := completedResponse(model)
	stream, _ := request["stream"].(bool)
	if !stream {
		writeJSON(w, http.StatusOK, response)
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	flusher, _ := w.(http.Flusher)
	writeEvent(w, flusher, "response.created", map[string]any{"type": "response.created", "response": inProgressResponse(model)})
	message := map[string]any{"id": "msg_mock", "type": "message", "status": "in_progress", "role": "assistant", "content": []any{}}
	writeEvent(w, flusher, "response.output_item.added", map[string]any{"type": "response.output_item.added", "output_index": 0, "item": message})
	writeEvent(w, flusher, "response.content_part.added", map[string]any{"type": "response.content_part.added", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "part": map[string]any{"type": "output_text", "text": "", "annotations": []any{}}})
	writeEvent(w, flusher, "response.output_text.delta", map[string]any{"type": "response.output_text.delta", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "delta": responseText})
	writeEvent(w, flusher, "response.output_text.done", map[string]any{"type": "response.output_text.done", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "text": responseText})
	writeEvent(w, flusher, "response.content_part.done", map[string]any{"type": "response.content_part.done", "item_id": "msg_mock", "output_index": 0, "content_index": 0, "part": map[string]any{"type": "output_text", "text": responseText, "annotations": []any{}}})
	writeEvent(w, flusher, "response.output_item.done", map[string]any{"type": "response.output_item.done", "output_index": 0, "item": response["output"].([]any)[0]})
	writeEvent(w, flusher, "response.completed", map[string]any{"type": "response.completed", "response": response})
	_, _ = w.Write([]byte("data: [DONE]\n\n"))
	if flusher != nil {
		flusher.Flush()
	}
}

func inProgressResponse(model string) map[string]any {
	return map[string]any{"id": "resp_mock", "object": "response", "created_at": time.Now().Unix(), "status": "in_progress", "model": model, "output": []any{}}
}

func completedResponse(model string) map[string]any {
	return map[string]any{
		"id": "resp_mock", "object": "response", "created_at": time.Now().Unix(), "status": "completed", "model": model,
		"output": []any{map[string]any{"id": "msg_mock", "type": "message", "status": "completed", "role": "assistant", "content": []any{map[string]any{"type": "output_text", "text": responseText, "annotations": []any{}}}}},
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
