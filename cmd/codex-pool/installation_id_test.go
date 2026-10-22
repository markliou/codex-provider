package main

import (
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// The client's installation id, and an id in the shape of Codex's own test
// fixture (app-server-daemon remote_control_client.rs TEST_INSTALLATION_ID).
const (
	clientInstallationID = "11111111-1111-4111-8111-111111111111"
	clientTurnMetadata   = `{"installation_id":"` + clientInstallationID + `","session_id":"01a15f63-427f-7fa2-884a-01ebe2c60a47","thread_id":"01a15f63-427f-7fa2-884a-01ebe2c60a47","request_kind":"turn"}`
)

// clientMetadataBody is a Responses body whose client_metadata has the keys a
// native Codex 0.163.0 sends (measured by wire-capture.sh), and whose input
// quotes the client's installation id, which must not change.
func clientMetadataBody() string {
	escaped := strings.ReplaceAll(clientTurnMetadata, `"`, `\"`)
	return `{"model":"gpt-test","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"my id is ` + clientInstallationID + `"}]}],"stream":true,"store":false,"client_metadata":{"x-codex-window-id":"01a15f63-427f-7fa2-884a-01ebe2c60a47:0","x-codex-installation-id":"` + clientInstallationID + `","x-codex-turn-metadata":"` + escaped + `"},"prompt_cache_key":"thread-native"}`
}

func TestAccountInstallationIDIsMadeOnceAsCodexMakesIt(t *testing.T) {
	a := testApp(t, []account{{ID: "device", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	item := a.config.Accounts[0]
	first, err := a.accountInstallationID(item)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := canonicalUUID(first); !ok || first[14] != '4' || !strings.ContainsRune("89ab", rune(first[19])) || strings.ToLower(first) != first {
		t.Fatalf("not a lowercase v4 UUID: %q", first)
	}
	path := filepath.Join(a.accountCodexHome(item.ID), codexInstallationIDFile)
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0o644 {
		t.Fatalf("installation_id file: %v %v", info, err)
	}
	if again, _ := a.accountInstallationID(item); again != first {
		t.Fatalf("the id changed: %q then %q", first, again)
	}
	// An existing valid id is kept, as Codex keeps it; garbage is replaced.
	if err := os.WriteFile(path, []byte(" 22222222-2222-4222-8222-22222222222A\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if kept, _ := a.accountInstallationID(item); kept != "22222222-2222-4222-8222-22222222222a" {
		t.Fatalf("an existing id was not kept: %q", kept)
	}
	if err := os.WriteFile(path, []byte("not-an-id"), 0o644); err != nil {
		t.Fatal(err)
	}
	if replaced, _ := a.accountInstallationID(item); replaced == "not-an-id" || len(replaced) != 36 {
		t.Fatalf("an invalid id was not replaced: %q", replaced)
	}
}

func TestWithInstallationIDChangesOnlyClientMetadata(t *testing.T) {
	const account = "33333333-3333-4333-8333-333333333333"
	body := clientMetadataBody()
	got := string(withInstallationID([]byte(body), account))
	start := strings.Index(body, `"client_metadata":`)
	end := strings.Index(body, `,"prompt_cache_key"`)
	want := body[:start] + strings.ReplaceAll(body[start:end], clientInstallationID, account) + body[end:]
	if got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
	if !strings.Contains(got, "my id is "+clientInstallationID) {
		t.Fatal("the conversation was changed")
	}
	if strings.Count(got, account) != 2 {
		t.Fatalf("the id and the turn metadata's id were not both replaced: %s", got)
	}
	for _, unchanged := range []string{
		`{"model":"gpt-test","input":[]}`,
		`{"model":"gpt-test","client_metadata":{"session_id":"s"}}`,
		`not json`,
	} {
		if out := string(withInstallationID([]byte(unchanged), account)); out != unchanged {
			t.Fatalf("%s became %s", unchanged, out)
		}
	}
	if out := turnMetadataWithInstallationID(clientTurnMetadata, account); out != strings.ReplaceAll(clientTurnMetadata, clientInstallationID, account) {
		t.Fatalf("turn metadata header = %s", out)
	}
	if out := turnMetadataWithInstallationID(`{"thread_id":"t"}`, account); out != `{"thread_id":"t"}` {
		t.Fatalf("a header without an id changed: %s", out)
	}
}

// Through the native endpoint, upstream sees the account's id in the body and
// the header, never the client's, and the rest of the body as Codex wrote it.
func TestNativeEndpointPresentsTheAccountInstallationID(t *testing.T) {
	var gotBody, gotHeader string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		gotBody = string(body)
		gotHeader = r.Header.Get("X-Codex-Turn-Metadata")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"id":"resp_native","object":"response","output":[]}`))
	}))
	defer upstream.Close()
	a := testApp(t, []account{{ID: "device", AccountID: "acct-pool", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.codexBaseURL = upstream.URL + "/backend-api"
	writeCodexDeviceAuth(t, a, "device", "acct-pool", "pool@example.test")
	req := httptest.NewRequest(http.MethodPost, "/k/client-key/backend-api/codex/responses", strings.NewReader(clientMetadataBody()))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-Codex-Turn-Metadata", clientTurnMetadata)
	recorder := httptest.NewRecorder()
	a.publicMux().ServeHTTP(recorder, req)
	if recorder.Code != http.StatusOK {
		t.Fatalf("native endpoint returned %d: %s", recorder.Code, recorder.Body.String())
	}
	id, _ := a.accountInstallationID(a.config.Accounts[0])
	if want := string(withInstallationID([]byte(clientMetadataBody()), id)); gotBody != want {
		t.Fatalf("upstream body\n got %s\nwant %s", gotBody, want)
	}
	if gotHeader != strings.ReplaceAll(clientTurnMetadata, clientInstallationID, id) {
		t.Fatalf("upstream turn metadata header = %s", gotHeader)
	}
}

// On the WebSocket, the handshake and each message present the id of the
// account the socket is bound to.
func TestWebSocketPresentsTheAccountInstallationID(t *testing.T) {
	upstream := &fakeCodexUpstream{script: func(string) []string { return completedEvents("resp_ws_id") }}
	a, url := wsTestPool(t, upstream)
	headers := http.Header{}
	headers.Set("Authorization", "Bearer client-key")
	headers.Set("OpenAI-Beta", "responses_websockets=2026-02-06")
	headers.Set("X-Codex-Turn-Metadata", clientTurnMetadata)
	conn, response, err := dialWebSocket(url, headers)
	if err != nil || conn == nil {
		t.Fatalf("dial pool: err=%v response=%v", err, response)
	}
	defer conn.Close()
	message := strings.Replace(clientMetadataBody(), `{"model"`, `{"type":"response.create","model"`, 1)
	if err := conn.WriteMessage(wsOpText, []byte(message)); err != nil {
		t.Fatal(err)
	}
	readUntilTerminal(t, conn)
	waitInFlightZero(t, a)

	upstream.mu.Lock()
	defer upstream.mu.Unlock()
	if len(upstream.handshakes) == 0 || len(upstream.messages) == 0 {
		t.Fatalf("upstream saw %d handshakes, %d messages", len(upstream.handshakes), len(upstream.messages))
	}
	handshake := upstream.handshakes[len(upstream.handshakes)-1]
	var bound account
	for _, item := range a.config.Accounts {
		if item.ID == handshake.Get("X-Codex-Upstream-Identity") {
			bound = item
		}
	}
	id, _ := a.accountInstallationID(bound)
	if got := handshake.Get("X-Codex-Turn-Metadata"); got != strings.ReplaceAll(clientTurnMetadata, clientInstallationID, id) {
		t.Fatalf("handshake turn metadata = %s", got)
	}
	if got := upstream.messages[len(upstream.messages)-1]; got != string(withInstallationID([]byte(message), id)) {
		t.Fatalf("upstream message = %s", got)
	}
	other := a.config.Accounts[0]
	if other.ID == bound.ID {
		other = a.config.Accounts[1]
	}
	if otherID, _ := a.accountInstallationID(other); otherID == id {
		t.Fatal("two accounts share an installation id")
	}
}
