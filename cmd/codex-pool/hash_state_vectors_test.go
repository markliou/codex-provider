package main

// This file is the compatibility contract for the step-4 Rust port.  The
// vectors are deliberately generated from the production helpers below rather
// than copied from a second implementation: changing a hash or JSON tag must
// make this test fail until the Rust implementation is updated with the same
// measured output.

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"testing"
	"time"
)

type hashVector struct {
	Kind             string  `json:"kind"`
	Input            string  `json:"input,omitempty"`
	KindInput        string  `json:"kindInput,omitempty"`
	Value            string  `json:"value,omitempty"`
	Model            string  `json:"model,omitempty"`
	Source           string  `json:"source,omitempty"`
	StickyKey        string  `json:"stickyKey,omitempty"`
	Identity         string  `json:"identity,omitempty"`
	Weight           float64 `json:"weight,omitempty"`
	WeightBits       uint64  `json:"weightBits,omitempty"`
	Buckets          int     `json:"buckets,omitempty"`
	DigestHex        string  `json:"digestHex,omitempty"`
	Output           string  `json:"output,omitempty"`
	Bucket           int     `json:"bucket,omitempty"`
	Score            float64 `json:"score,omitempty"`
	ScoreBits        uint64  `json:"scoreBits,omitempty"`
	SlotID           string  `json:"slotId,omitempty"`
	AccountID        string  `json:"accountId,omitempty"`
	Email            string  `json:"email,omitempty"`
	OrganizationName string  `json:"organizationName,omitempty"`
	AuthType         string  `json:"authType,omitempty"`
	PlanFamily       string  `json:"planFamily,omitempty"`
	PlanLimit        string  `json:"planLimit,omitempty"`
	SeatType         string  `json:"seatType,omitempty"`
	Remaining        int     `json:"remaining,omitempty"`
	Active           int     `json:"active,omitempty"`
}

func TestCompatibilityVectors(t *testing.T) {
	path := filepath.Join("testdata", "hash-vectors.json")
	if os.Getenv("CODEX_POOL_GENERATE_VECTORS") == "1" {
		if err := writeHashVectors(path); err != nil {
			t.Fatal(err)
		}
		if err := writeHashVectors(filepath.Join("..", "..", "upstream", "src", "pool", "testdata", "hash-vectors.json")); err != nil {
			t.Fatal(err)
		}
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s (set CODEX_POOL_GENERATE_VECTORS=1 once to create it): %v", path, err)
	}
	var vectors []hashVector
	if err := json.Unmarshal(data, &vectors); err != nil {
		t.Fatal(err)
	}
	for _, vector := range vectors {
		switch vector.Kind {
		case "short_hash":
			if got := shortHash([]byte(vector.Input)); got != vector.Output {
				t.Errorf("short hash %q: got %q want %q", vector.Input, got, vector.Output)
			}
		case "operational_identifier_hash":
			if got := operationalIdentifierHash(vector.KindInput, vector.Value); got != vector.Output {
				t.Errorf("operational hash %q/%q: got %q want %q", vector.KindInput, vector.Value, got, vector.Output)
			}
		case "prompt_cache_key_hash":
			if got := promptCacheKeyHash(vector.Model, vector.Source, vector.Value); got != vector.Output {
				t.Errorf("prompt cache hash: got %q want %q", got, vector.Output)
			}
		case "api_key_fingerprint":
			if got := apiKeyFingerprint(vector.Input); got != vector.Output {
				t.Errorf("api key fingerprint %q: got %q want %q", vector.Input, got, vector.Output)
			}
		case "prompt_cache_bucket":
			if got := promptCacheBucketIndex(vector.Input, vector.Buckets); got != vector.Bucket {
				t.Errorf("prompt cache bucket %q/%d: got %d want %d", vector.Input, vector.Buckets, got, vector.Bucket)
			}
		case "sticky_balance_score":
			a, item := vectorAppAndAccount(vector)
			sum := a.stickyBalanceScore(vector.StickyKey, item)
			if got := hex.EncodeToString(sum[:]); got != vector.DigestHex {
				t.Errorf("sticky digest: got %q want %q", got, vector.DigestHex)
			}
		case "placement_score":
			a, item := vectorAppAndAccount(vector)
			got := a.placementScoreLocked(vector.StickyKey, item, map[string]int{item.ID: vector.Active})
			if math.Float64bits(got) != vector.ScoreBits {
				t.Errorf("placement score bits: got %x want %x", math.Float64bits(got), vector.ScoreBits)
			}
		case "upstream_identity":
			a := &app{}
			item := vectorAccount(vector)
			if got := a.upstreamIdentityKeyLocked(item); got != vector.Output {
				t.Errorf("upstream identity: got %q want %q", got, vector.Output)
			}
		default:
			t.Errorf("unknown vector kind %q", vector.Kind)
		}
	}
}

func vectorAccount(vector hashVector) account {
	slotID := vector.SlotID
	if slotID == "" {
		slotID = "fixture-slot"
	}
	return account{ID: slotID, AuthType: vector.AuthType, AccountID: vector.AccountID, Email: vector.Email, OrganizationName: vector.OrganizationName, PlanFamily: vector.PlanFamily, PlanLimit: vector.PlanLimit, SeatType: vector.SeatType}
}

func vectorAppAndAccount(vector hashVector) (*app, account) {
	item := vectorAccount(vector)
	remaining := float64(vector.Remaining)
	quota := accountQuota{Primary: quotaWindow{Present: true, RemainingPercent: &remaining}}
	a := &app{state: state{Quotas: map[string]quotaSnapshot{item.ID: {AccountID: item.ID, PlanFamily: item.PlanFamily, PlanLimit: item.PlanLimit, SeatType: item.SeatType, Quota: &quota}}}}
	return a, item
}

func writeHashVectors(path string) error {
	values := []hashVector{
		{Kind: "short_hash", Input: "", Output: shortHash(nil)},
		{Kind: "short_hash", Input: "hello", Output: shortHash([]byte("hello"))},
		{Kind: "short_hash", Input: "  leading and trailing  ", Output: shortHash([]byte("  leading and trailing  "))},
		{Kind: "short_hash", Input: "繁體中文/🦀", Output: shortHash([]byte("繁體中文/🦀"))},
		{Kind: "operational_identifier_hash", KindInput: "thread", Value: " thread-42 ", Output: operationalIdentifierHash("thread", " thread-42 ")},
		{Kind: "operational_identifier_hash", KindInput: "response", Value: "response-abc", Output: operationalIdentifierHash("response", "response-abc")},
		{Kind: "operational_identifier_hash", KindInput: "lineage", Value: "   ", Output: operationalIdentifierHash("lineage", "   ")},
		{Kind: "prompt_cache_key_hash", Model: "gpt-test", Source: "project", Value: "repo-main#3", Output: promptCacheKeyHash("gpt-test", "project", "repo-main#3")},
		{Kind: "prompt_cache_key_hash", Model: "", Source: "fallback", Value: "value", Output: promptCacheKeyHash("", "fallback", "value")},
		{Kind: "api_key_fingerprint", Input: "", Output: apiKeyFingerprint("")},
		{Kind: "api_key_fingerprint", Input: "  ", Output: apiKeyFingerprint("  ")},
		{Kind: "api_key_fingerprint", Input: "client-key-42", Output: apiKeyFingerprint("client-key-42")},
		{Kind: "prompt_cache_bucket", Input: "gpt-test:thread:one", Buckets: 16, Bucket: promptCacheBucketIndex("gpt-test:thread:one", 16)},
		{Kind: "prompt_cache_bucket", Input: "anything", Buckets: 1, Bucket: promptCacheBucketIndex("anything", 1)},
	}
	identityCases := []hashVector{
		{Kind: "upstream_identity", SlotID: "identity-a", AuthType: "codex_device_auth", AccountID: "workspace-a", Email: " Member@Example.TEST ", Output: "codex-account:workspace-a|member:member@example.test"},
		{Kind: "upstream_identity", SlotID: "identity-b", AuthType: "codex_device_auth", Email: "Member@Example.TEST", OrganizationName: " Example   Org ", Output: "codex-profile:member@example.test|Example Org"},
		{Kind: "upstream_identity", SlotID: "identity-c", AuthType: "codex_device_auth", OrganizationName: "Org", Output: "codex-profile:|Org"},
		{Kind: "upstream_identity", SlotID: "identity-d", AuthType: "codex_device_auth", Output: ""},
		{Kind: "upstream_identity", SlotID: "identity-e", AuthType: "api_key", AccountID: "workspace-a", Email: "member@example.test", Output: ""},
	}
	for _, vector := range identityCases {
		item := vectorAccount(vector)
		a := &app{}
		vector.Output = a.upstreamIdentityKeyLocked(item)
		values = append(values, vector)
	}
	for i := 0; i < 1024; i++ {
		vector := hashVector{Kind: "placement_score", SlotID: fmt.Sprintf("slot-%04d", i), StickyKey: fmt.Sprintf("gpt-test:thread:%04d", i), Active: i % 7, Remaining: 1 + (i*37)%100}
		switch i % 4 {
		case 0:
			vector.AuthType, vector.AccountID, vector.Email = "codex_device_auth", fmt.Sprintf("workspace-%d", i%11), fmt.Sprintf("Member%d@Example.TEST", i)
		case 1:
			vector.AuthType, vector.Email, vector.OrganizationName = "codex_device_auth", fmt.Sprintf("Member%d@Example.TEST", i), "Example Org"
		case 2:
			vector.AuthType, vector.OrganizationName = "codex_device_auth", fmt.Sprintf("Org %d", i)
		default:
			vector.AuthType = "api_key"
		}
		switch i % 5 {
		case 1:
			vector.PlanFamily, vector.PlanLimit = "pro", "5x"
		case 2:
			vector.PlanFamily, vector.PlanLimit = "pro_lite", ""
		case 3:
			vector.PlanFamily, vector.SeatType = "business", "premium"
		}
		a, item := vectorAppAndAccount(vector)
		vector.Identity = a.upstreamIdentityKeyLocked(item)
		sum := a.stickyBalanceScore(vector.StickyKey, item)
		vector.DigestHex = hex.EncodeToString(sum[:])
		vector.Score = a.placementScoreLocked(vector.StickyKey, item, map[string]int{item.ID: vector.Active})
		vector.Weight = a.placementWeightLocked(item, map[string]int{item.ID: vector.Active})
		vector.WeightBits = math.Float64bits(vector.Weight)
		vector.ScoreBits = math.Float64bits(vector.Score)
		values = append(values, vector)
	}
	data, err := json.MarshalIndent(values, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

func TestStateVectorsAreGeneratedFromGoTypes(t *testing.T) {
	configPath := filepath.Join("testdata", "state-vectors-config.json")
	runtimePath := filepath.Join("testdata", "state-vectors-runtime.json")
	if os.Getenv("CODEX_POOL_GENERATE_VECTORS") == "1" {
		if err := writeStateVectors(configPath, runtimePath); err != nil {
			t.Fatal(err)
		}
		if err := writeStateVectors(
			filepath.Join("..", "..", "upstream", "src", "pool", "testdata", "state-vectors-config.json"),
			filepath.Join("..", "..", "upstream", "src", "pool", "testdata", "state-vectors-runtime.json"),
		); err != nil {
			t.Fatal(err)
		}
	}
	for _, path := range []string{configPath, runtimePath} {
		if _, err := os.Stat(path); err != nil {
			t.Fatalf("state vector %s missing (set CODEX_POOL_GENERATE_VECTORS=1 once): %v", path, err)
		}
	}
}

func writeStateVectors(configPath, runtimePath string) error {
	created := time.Date(2026, 10, 21, 1, 2, 3, 123456789, time.FixedZone("+08", 8*60*60))
	updated := created.Add(2*time.Second + 7*time.Nanosecond)
	acct := account{
		ID: "slot-a", Label: "test account", OwnerNote: "fake fixture", Email: "member@example.test", AccountID: "workspace-a",
		OrganizationName: "Example Org", PlanType: "Plus", PlanLimit: "100", PlanRank: 2, RawPlanType: "plus", PlanFamily: "plus", SeatType: "individual", SeatTypeRaw: "individual",
		SeatTypeInferred: true, QuotaPolicy: []string{"weekly"}, AuthType: "device", CodexHome: "/fake/codex", Enabled: true, InPool: true, Priority: 3,
		RemainingQuota: ptrInt(72), QuotaProtectionEnabled: true, QuotaProtectionThreshold: 5, AllowedModels: []string{"gpt-test"}, ExcludedModels: []string{"gpt-old"},
		UpstreamBaseURL: "https://example.test", UpstreamAPIKey: "fake-key", WireAPI: "responses", PendingPoolActivation: false, PendingAuthVerification: true,
		PendingAuthExpectedAccountID: "workspace-a", CreatedAt: created, UpdatedAt: updated, LastLoginAt: updated,
	}
	config := config{DefaultModel: "gpt-test", ModelAliases: map[string]string{"alias": "gpt-test"}, Accounts: []account{acct}, PreserveProQuota: ptrBool(true), LineageAffinity: ptrBool(false), CreatedAt: created, UpdatedAt: updated}
	configRaw, err := json.Marshal(config)
	if err != nil {
		return err
	}
	var configObject map[string]any
	if err := json.Unmarshal(configRaw, &configObject); err != nil {
		return err
	}
	configObject["futureConfigField"] = map[string]any{"kept": true}
	configObject["futureNull"] = nil
	if err := writePrettyJSON(configPath, configObject); err != nil {
		return err
	}

	state := state{
		StickySessions:     map[string]stickySession{"gpt-test:thread:one": {Key: "gpt-test:thread:one", ModelID: "gpt-test", AccountID: "slot-a", CreatedAt: created, LastSuccessAt: updated, ExpiresAt: updated.Add(time.Hour), FailoverFrom: "slot-old"}},
		ResponseBindings:   map[string]responseBinding{"resp-1": {ResponseID: "resp-1", StickyKey: "gpt-test:thread:one", ModelID: "gpt-test", AccountID: "slot-a", CreatedAt: created, ExpiresAt: updated.Add(time.Hour)}},
		ThreadBindings:     map[string]threadBinding{"thread-1": {ThreadID: "thread-1", SessionID: "session-1", ParentThreadID: "parent-1", LineageRootID: "lineage-1", SubagentKind: "main", ModelID: "gpt-test", AccountID: "slot-a", StickyKey: "gpt-test:thread:one", PromptCacheKey: "cp_test", CreatedAt: created, LastSuccessAt: updated, ExpiresAt: updated.Add(time.Hour)}},
		LineageBindings:    map[string]lineageBinding{"lineage-1": {LineageRootID: "lineage-1", AccountID: "slot-a", FailoverFrom: "slot-old", CreatedAt: created, MovedAt: updated, LastSuccessAt: updated, ExpiresAt: updated.Add(time.Hour)}},
		Cooldowns:          map[string][]cooldown{"slot-a": {{ModelID: "gpt-test", NextRetryAt: updated, Reason: "upstream_5xx"}}},
		Health:             map[string]accountHealth{"slot-a": {ConsecutiveFailure: 2, LastFailureReason: "upstream_5xx", LastFailureAt: updated}},
		PromptCacheResetAt: updated, QuotaWindowPrimes: map[string]time.Time{"slot-a": updated}, RequestCount: 4, SuccessCount: 3, FailureCount: 1, UpstreamResponseFailedCount: 1, StreamIncompleteCount: 0, UpdatedAt: updated,
	}
	runtimeRaw, err := json.Marshal(state)
	if err != nil {
		return err
	}
	var runtimeObject map[string]any
	if err := json.Unmarshal(runtimeRaw, &runtimeObject); err != nil {
		return err
	}
	runtimeObject["futureRuntimeField"] = "preserve-me"
	if sticky, ok := runtimeObject["stickySessions"].(map[string]any); ok {
		if one, ok := sticky["gpt-test:thread:one"].(map[string]any); ok {
			one["futureStickyField"] = []any{"kept", 7}
		}
	}
	return writePrettyJSON(runtimePath, runtimeObject)
}

func ptrBool(value bool) *bool { return &value }
func ptrInt(value int) *int    { return &value }

func writePrettyJSON(path string, value any) error {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

func TestRustStateShapeCanBeReadByGo(t *testing.T) {
	// Rust's round-trip test compares its serde output with these Go-generated
	// documents. Decode the same canonical documents here as the final
	// compatibility gate: Go must still accept every known field Rust emits.
	// Go intentionally drops unknown fields on re-marshal; preservation of those
	// fields is Rust's responsibility and is asserted by the Rust fixture test.
	for _, path := range []string{
		filepath.Join("..", "..", "upstream", "src", "pool", "testdata", "state-vectors-config.json"),
		filepath.Join("..", "..", "upstream", "src", "pool", "testdata", "state-vectors-runtime.json"),
	} {
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatalf("read canonical Rust fixture %s: %v", path, err)
		}
		if path == filepath.Join("..", "..", "upstream", "src", "pool", "testdata", "state-vectors-config.json") {
			var value config
			if err := json.Unmarshal(data, &value); err != nil {
				t.Fatalf("Go config decoder rejected Rust shape: %v", err)
			}
			if value.DefaultModel != "gpt-test" || len(value.Accounts) != 1 {
				t.Fatalf("Go config decoder lost known fields: %+v", value)
			}
		} else {
			var value state
			if err := json.Unmarshal(data, &value); err != nil {
				t.Fatalf("Go runtime decoder rejected Rust shape: %v", err)
			}
			if value.RequestCount != 4 || value.SuccessCount != 3 || len(value.StickySessions) != 1 {
				t.Fatalf("Go runtime decoder lost known fields: %+v", value)
			}
		}
	}
}

func TestRustRoundTripArtifactsReadBackIdentically(t *testing.T) {
	fixtureDir := filepath.Join("..", "..", "upstream", "src", "pool", "testdata")
	for _, name := range []string{"config", "runtime"} {
		original, err := readJSONMap(filepath.Join(fixtureDir, "state-vectors-"+name+".json"))
		if err != nil {
			t.Fatalf("read Go %s fixture: %v", name, err)
		}
		artifact, err := readJSONMap(filepath.Join(fixtureDir, "rust-roundtrip-"+name+".json"))
		if err != nil {
			t.Fatalf("read Rust %s round-trip artifact (run the Rust artifact test first): %v", name, err)
		}
		if !reflect.DeepEqual(original, artifact) {
			t.Fatalf("Rust %s round-trip changed known fields or unknown future fields:\noriginal=%v\nartifact=%v", name, original, artifact)
		}
		if name == "config" && !reflect.DeepEqual(artifact["futureConfigField"], map[string]any{"kept": true}) {
			t.Fatalf("Rust config artifact dropped futureConfigField: %v", artifact["futureConfigField"])
		}
		if name == "runtime" && artifact["futureRuntimeField"] != "preserve-me" {
			t.Fatalf("Rust runtime artifact dropped futureRuntimeField: %v", artifact["futureRuntimeField"])
		}
	}
}

func readJSONMap(path string) (map[string]any, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var value map[string]any
	if err := json.Unmarshal(data, &value); err != nil {
		return nil, err
	}
	return value, nil
}
