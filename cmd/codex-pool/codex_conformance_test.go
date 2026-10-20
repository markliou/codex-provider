package main

import (
	"encoding/json"
	"os"
	"regexp"
	"testing"
)

// The pool must read upstream's answers the way Codex reads them. Hand-written
// fake upstreams encoded what the pool assumed instead: a usage limit reported
// by error.type, a stream without Content-Type, and informational socket
// events all passed every test and broke only against the real backend. These
// tests use shapes copied from Codex's own source (see the fixture's comment),
// and scripts/conformance/codex-conformance.sh check verifies each one still
// exists there.

type codexShapeFixture struct {
	CodexRelease string `json:"codexRelease"`
	Failures     []struct {
		Name   string          `json:"name"`
		Source string          `json:"source"`
		Marker string          `json:"marker"`
		Event  json.RawMessage `json:"event"`
		Class  string          `json:"class"`
		Code   string          `json:"code"`
	} `json:"failures"`
	Lifecycle []struct {
		Name  string          `json:"name"`
		Event json.RawMessage `json:"event"`
	} `json:"lifecycle"`
}

func loadCodexShapeFixture(t *testing.T) codexShapeFixture {
	t.Helper()
	raw, err := os.ReadFile("testdata/codex-upstream-failures.json")
	if err != nil {
		t.Fatal(err)
	}
	var fixture codexShapeFixture
	if err := json.Unmarshal(raw, &fixture); err != nil {
		t.Fatal(err)
	}
	return fixture
}

// The fixture describes one Codex release, and it must be the release the
// transport is built from: bumping the Codex tag without revisiting the shapes
// that release parses is exactly how the pool drifts from Codex.
func TestCodexShapeFixtureTracksThePinnedCodexRelease(t *testing.T) {
	fixture := loadCodexShapeFixture(t)
	cargo, err := os.ReadFile("../../upstream/Cargo.toml")
	if err != nil {
		t.Fatal(err)
	}
	tags := regexp.MustCompile(`openai/codex", tag = "([^"]+)"`).FindAllSubmatch(cargo, -1)
	if len(tags) == 0 {
		t.Fatal("upstream/Cargo.toml names no Codex tag")
	}
	for _, tag := range tags {
		if string(tag[1]) != fixture.CodexRelease {
			t.Fatalf("upstream/Cargo.toml pins Codex %s but the shape fixture describes %s; re-run scripts/conformance/codex-conformance.sh and update the fixture", tag[1], fixture.CodexRelease)
		}
	}
}

func TestCodexUpstreamShapesClassifyAsCodexReadsThem(t *testing.T) {
	fixture := loadCodexShapeFixture(t)
	if len(fixture.Failures) == 0 || len(fixture.Lifecycle) == 0 {
		t.Fatal("empty shape fixture")
	}
	for _, shape := range fixture.Failures {
		t.Run(shape.Name, func(t *testing.T) {
			if shape.Source == "" || shape.Marker == "" {
				t.Fatal("every shape must name the Codex source it was taken from")
			}
			info := responseInfoFromSSEBlock("data: " + string(shape.Event) + "\n\n")
			if info.TerminalEvent == "" {
				t.Fatalf("a failure Codex treats as terminal was not terminal: %s", shape.Event)
			}
			if info.TerminalFailureClass != shape.Class || info.TerminalErrorCode != shape.Code {
				t.Fatalf("class/code = %q/%q, want %q/%q for %s", info.TerminalFailureClass, info.TerminalErrorCode, shape.Class, shape.Code, shape.Event)
			}
		})
	}
	for _, shape := range fixture.Lifecycle {
		t.Run("lifecycle "+shape.Name, func(t *testing.T) {
			block := "data: " + string(shape.Event) + "\n\n"
			if info := responseInfoFromSSEBlock(block); info.TerminalEvent != "" {
				t.Fatalf("lifecycle event read as terminal: %#v", info)
			}
			if !precommitLifecycleSSEBlock(block) {
				t.Fatalf("an event Codex treats as informational must stay before the commit point: %s", shape.Event)
			}
		})
	}
}

// Routing's identity and the dashboard's allowance must draw the same line
// between accounts. They once disagreed: the dashboard counted each member of a
// Business workspace as its own allowance while routing collapsed them into one
// account, and the gap went unnoticed for a month.
func TestRoutingIdentityAndCapacityAllowanceAgree(t *testing.T) {
	a := testApp(t, []account{
		{ID: "member-a", AuthType: "codex_device_auth", Enabled: true, InPool: true, AccountID: "workspace-1", Email: "a@example.test"},
		{ID: "member-b", AuthType: "codex_device_auth", Enabled: true, InPool: true, AccountID: "workspace-1", Email: "b@example.test"},
		{ID: "copy-of-a", AuthType: "codex_device_auth", Enabled: true, InPool: true, AccountID: "workspace-1", Email: "A@example.test"},
		{ID: "personal", AuthType: "codex_device_auth", Enabled: true, InPool: true, AccountID: "personal-1", Email: "p@example.test"},
		{ID: "other-workspace", AuthType: "codex_device_auth", Enabled: true, InPool: true, AccountID: "workspace-2", Email: "a@example.test"},
	})
	for _, item := range a.config.Accounts {
		writeTestCodexAuth(t, a, item.ID)
	}
	a.mu.RLock()
	defer a.mu.RUnlock()
	for i, left := range a.config.Accounts {
		for _, right := range a.config.Accounts[i+1:] {
			sameRoute := a.upstreamIdentityKeyLocked(left) == a.upstreamIdentityKeyLocked(right)
			sameAllowance := capacityAllowanceKeyLocked(a, left) == capacityAllowanceKeyLocked(a, right)
			if sameRoute != sameAllowance {
				t.Fatalf("%s and %s: same routing identity %v, same allowance %v", left.ID, right.ID, sameRoute, sameAllowance)
			}
		}
	}
	if a.upstreamIdentityKeyLocked(a.config.Accounts[0]) != a.upstreamIdentityKeyLocked(a.config.Accounts[2]) {
		t.Fatal("two slots from one login must stay one identity")
	}
	if a.upstreamIdentityKeyLocked(a.config.Accounts[0]) == a.upstreamIdentityKeyLocked(a.config.Accounts[1]) {
		t.Fatal("two members of one workspace must be separate identities")
	}
}

// An unclassified failure is the production signal that upstream sent a shape
// the pool does not read as Codex does, so it must reach the dashboard; a code
// Codex handles itself must not raise that alarm.
func TestUnrecognizedUpstreamFailuresAreCounted(t *testing.T) {
	a := testApp(t, []account{{ID: "first", AuthType: "codex_device_auth", Enabled: true, InPool: true}})
	for _, payload := range []string{
		`{"type":"response.failed","response":{"error":{"code":"brand_new_condition"}}}`,
		`{"type":"error","status":400,"error":{"type":"invalid_request_error","code":"websocket_connection_limit_reached"}}`,
	} {
		info := responseInfoFromSSEBlock("data: " + payload + "\n\n")
		a.markTerminalResponseFailure(routingDecision{}, "gpt-test", "first", info, nil, true)
	}
	a.mu.RLock()
	window := a.promptCacheWindowLocked()
	a.mu.RUnlock()
	if got := window["unrecognizedFailureCount"].(uint64); got != 1 {
		t.Fatalf("unrecognized failures = %d, want 1 (the unknown code, not the passthrough one)", got)
	}
}
