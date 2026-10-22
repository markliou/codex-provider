package main

import (
	"bytes"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

// Each upstream account presents one fixed Codex installation id.
//
// A native Codex keeps a random UUID in CODEX_HOME/installation_id and sends
// it with every Responses request: as client_metadata["x-codex-installation-id"]
// and as "installation_id" inside the x-codex-turn-metadata JSON, both in the
// body's client_metadata and in the x-codex-turn-metadata header (core
// responses_metadata.rs client_metadata, compatibility_headers,
// turn_metadata_payload). Relayed as the client wrote it, one client's id went
// to every account the pool placed it on, and each account saw the ids of
// every client that ever used it: several accounts tied to one installation,
// and one account to many. Upstream may route or rate by that id, so each
// account now presents its own, as if a single Codex installation used it.
//
// The id lives where Codex keeps it, in the account's own CODEX_HOME, and is
// made the way Codex makes it (core installation_id.rs
// resolve_installation_id): a v4 UUID in lowercase, mode 0644, an existing
// valid one kept. Only the id's own occurrences in client_metadata and the
// turn-metadata header change; the rest of the request is forwarded byte for
// byte, so the conversation is never touched even if it quotes an id. This is
// the one rewrite the native endpoint makes (SPEC.md 2.3.2, installation id).
const codexInstallationIDFile = "installation_id"

var installationIDMu sync.Mutex

// accountInstallationID returns the account's installation id, creating it
// on first use.
func (a *app) accountInstallationID(item account) (string, error) {
	home := a.accountCodexHome(item.ID)
	path := filepath.Join(home, codexInstallationIDFile)
	installationIDMu.Lock()
	defer installationIDMu.Unlock()
	if data, err := os.ReadFile(path); err == nil {
		if id, ok := canonicalUUID(strings.TrimSpace(string(data))); ok {
			return id, nil
		}
	} else if !os.IsNotExist(err) {
		return "", err
	}
	id, err := newUUIDv4()
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(home, 0o700); err != nil {
		return "", err
	}
	temp := path + ".tmp"
	if err := os.WriteFile(temp, []byte(id), 0o644); err != nil {
		return "", err
	}
	if err := os.Chmod(temp, 0o644); err != nil {
		_ = os.Remove(temp)
		return "", err
	}
	if err := os.Rename(temp, path); err != nil {
		_ = os.Remove(temp)
		return "", err
	}
	return id, nil
}

// presentedInstallationID is the account's installation id for a request
// about to go upstream. When it cannot be had the request goes out as the
// client wrote it, logged, rather than failing a turn over metadata.
func (a *app) presentedInstallationID(item account) (string, bool) {
	id, err := a.accountInstallationID(item)
	if err != nil {
		if a.logger != nil {
			a.logger.Printf("installation id unavailable for %s: %s", item.ID, err)
		}
		return "", false
	}
	return id, true
}

// setTurnMetadataInstallationID puts id in the x-codex-turn-metadata header.
func setTurnMetadataInstallationID(header http.Header, id string) {
	if value := header.Get("X-Codex-Turn-Metadata"); value != "" {
		header.Set("X-Codex-Turn-Metadata", turnMetadataWithInstallationID(value, id))
	}
}

// withInstallationID replaces the installation id in a Responses body's
// top-level client_metadata with id. Bytes outside client_metadata are kept
// as they are; a body without one is returned unchanged.
func withInstallationID(body []byte, id string) []byte {
	start, end, ok := clientMetadataSpan(body)
	if !ok {
		return body
	}
	var metadata map[string]any
	if json.Unmarshal(body[start:end], &metadata) != nil {
		return body
	}
	client, _ := metadata["x-codex-installation-id"].(string)
	if client == "" {
		turn, _ := metadata["x-codex-turn-metadata"].(string)
		client = turnMetadataInstallationID(turn)
	}
	if client == "" || client == id {
		return body
	}
	span := bytes.ReplaceAll(body[start:end], []byte(client), []byte(id))
	out := make([]byte, 0, len(body)-(end-start)+len(span))
	out = append(out, body[:start]...)
	out = append(out, span...)
	return append(out, body[end:]...)
}

// clientMetadataSpan finds the bytes of the top-level client_metadata value.
func clientMetadataSpan(body []byte) (int, int, bool) {
	decoder := json.NewDecoder(bytes.NewReader(body))
	if token, err := decoder.Token(); err != nil || token != json.Delim('{') {
		return 0, 0, false
	}
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return 0, 0, false
		}
		key, _ := token.(string)
		var value json.RawMessage
		if err := decoder.Decode(&value); err != nil {
			return 0, 0, false
		}
		if key == "client_metadata" {
			end := int(decoder.InputOffset())
			start := end - len(value)
			if start < 0 || !bytes.Equal(body[start:end], value) || len(value) == 0 || value[0] != '{' {
				return 0, 0, false
			}
			return start, end, true
		}
	}
	return 0, 0, false
}

func turnMetadataInstallationID(value string) string {
	var payload map[string]any
	if json.Unmarshal([]byte(value), &payload) != nil {
		return ""
	}
	id, _ := payload["installation_id"].(string)
	return id
}

// turnMetadataWithInstallationID replaces the installation_id inside an
// x-codex-turn-metadata JSON value, keeping the rest of it as written.
func turnMetadataWithInstallationID(value, id string) string {
	client := turnMetadataInstallationID(value)
	if client == "" || client == id {
		return value
	}
	return strings.ReplaceAll(value, client, id)
}

func canonicalUUID(value string) (string, bool) {
	if len(value) != 36 {
		return "", false
	}
	for i, c := range value {
		switch i {
		case 8, 13, 18, 23:
			if c != '-' {
				return "", false
			}
		default:
			if !strings.ContainsRune("0123456789abcdefABCDEF", c) {
				return "", false
			}
		}
	}
	return strings.ToLower(value), true
}

func newUUIDv4() (string, error) {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "", err
	}
	b[6] = b[6]&0x0f | 0x40
	b[8] = b[8]&0x3f | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16]), nil
}
