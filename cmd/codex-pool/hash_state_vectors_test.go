package main

// This file is the compatibility contract for the step-4 Rust port
// (RUST_PORT.md step 4, upstream/src/pool). The vectors are generated from the
// production helpers rather than copied from a second implementation, and the
// tests regenerate them and compare with the committed files: changing a hash,
// a JSON tag or a state field makes these tests fail until the files are
// regenerated (CODEX_POOL_GENERATE_VECTORS=1), and the regenerated files then
// fail the Rust tests until the Rust side matches. Fixtures hold fake data
// only; nothing here may come from a real account or /data.

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"
)

const (
	hashVectorsPath  = "testdata/hash-vectors.json"
	stateVectorsPath = "testdata/state-vectors.json"
)

type hashVector struct {
	Kind             string `json:"kind"`
	Input            string `json:"input,omitempty"`
	KindInput        string `json:"kindInput,omitempty"`
	Value            string `json:"value,omitempty"`
	Model            string `json:"model,omitempty"`
	Source           string `json:"source,omitempty"`
	StickyKey        string `json:"stickyKey,omitempty"`
	Identity         string `json:"identity,omitempty"`
	WeightBits       uint64 `json:"weightBits,omitempty"`
	Buckets          int    `json:"buckets,omitempty"`
	DigestHex        string `json:"digestHex,omitempty"`
	Output           string `json:"output,omitempty"`
	Bucket           int    `json:"bucket,omitempty"`
	ScoreBits        uint64 `json:"scoreBits,omitempty"`
	SlotID           string `json:"slotId,omitempty"`
	AccountID        string `json:"accountId,omitempty"`
	Email            string `json:"email,omitempty"`
	OrganizationName string `json:"organizationName,omitempty"`
	AuthType         string `json:"authType,omitempty"`
	PlanFamily       string `json:"planFamily,omitempty"`
	PlanLimit        string `json:"planLimit,omitempty"`
	SeatType         string `json:"seatType,omitempty"`
	Remaining        int    `json:"remaining,omitempty"`
	Active           int    `json:"active,omitempty"`
}

// checkGeneratedFile compares a regenerated document with the committed one,
// or rewrites it when CODEX_POOL_GENERATE_VECTORS=1.
func checkGeneratedFile(t *testing.T, path string, generated []byte) {
	t.Helper()
	if os.Getenv("CODEX_POOL_GENERATE_VECTORS") == "1" {
		if err := os.WriteFile(path, generated, 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	committed, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s (set CODEX_POOL_GENERATE_VECTORS=1 to create it): %v", path, err)
	}
	if !bytes.Equal(committed, generated) {
		t.Fatalf("%s no longer matches the Go code; regenerate it with CODEX_POOL_GENERATE_VECTORS=1, then make the Rust side (upstream/src/pool) pass", path)
	}
}

func TestHashVectorsMatchTheGoCode(t *testing.T) {
	checkGeneratedFile(t, hashVectorsPath, generateHashVectors())
}

func TestStateVectorsMatchTheGoTypes(t *testing.T) {
	checkGeneratedFile(t, stateVectorsPath, generateStateVectors(t))
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

// Strings Go and Rust might trim differently: Unicode spaces (NEL, NBSP,
// ideographic space), control characters, and a zero-width space that is not
// a space to either.
var unicodeEdgeInputs = []string{
	"\u0085value ",
	"　value　",
	"\tvalue\n",
	"value​",
}

func generateHashVectors() []byte {
	values := []hashVector{
		{Kind: "short_hash", Input: ""},
		{Kind: "short_hash", Input: "hello"},
		{Kind: "short_hash", Input: "  leading and trailing  "},
		{Kind: "short_hash", Input: "繁體中文/🦀"},
		{Kind: "operational_identifier_hash", KindInput: "thread", Value: " thread-42 "},
		{Kind: "operational_identifier_hash", KindInput: "response", Value: "response-abc"},
		{Kind: "operational_identifier_hash", KindInput: "lineage", Value: "   "},
		{Kind: "prompt_cache_key_hash", Model: "gpt-test", Source: "project", Value: "repo-main#3"},
		{Kind: "prompt_cache_key_hash", Model: "", Source: "fallback", Value: "value"},
		{Kind: "api_key_fingerprint", Input: ""},
		{Kind: "api_key_fingerprint", Input: "  "},
		{Kind: "api_key_fingerprint", Input: "\u0085　"},
		{Kind: "api_key_fingerprint", Input: "client-key-42"},
		{Kind: "prompt_cache_bucket", Input: "gpt-test:thread:one", Buckets: 16},
		{Kind: "prompt_cache_bucket", Input: "anything", Buckets: 1},
	}
	for _, input := range unicodeEdgeInputs {
		values = append(values, hashVector{Kind: "operational_identifier_hash", KindInput: "thread", Value: input})
	}
	identityCases := []hashVector{
		{SlotID: "identity-a", AuthType: "codex_device_auth", AccountID: "workspace-a", Email: " Member@Example.TEST "},
		{SlotID: "identity-b", AuthType: "codex_device_auth", Email: "Member@Example.TEST", OrganizationName: " Example   Org "},
		{SlotID: "identity-c", AuthType: "codex_device_auth", OrganizationName: "Org"},
		{SlotID: "identity-d", AuthType: "codex_device_auth"},
		{SlotID: "identity-e", AuthType: "api_key", AccountID: "workspace-a", Email: "member@example.test"},
		// The dotted capital I lower-cases to two characters under the full
		// Unicode mapping and to one under Go's per-rune mapping; a final
		// sigma changes only under a context-aware mapping.
		{SlotID: "identity-f", AuthType: "codex_device_auth", AccountID: "　workspace-f\u0085", Email: " İSTANBUL@Example.TEST"},
		{SlotID: "identity-g", AuthType: "codex_device_auth", Email: "ΟΔΟΣ@example.test", OrganizationName: "Org\twith\ncontrols\u0085and　spaces"},
		// 119 letters, a space, then more: Go joins the words and then cuts at
		// 120 runes, so the name ends in that space.
		{SlotID: "identity-h", AuthType: "codex_device_auth", OrganizationName: strings.Repeat("a", 119) + " bcd"},
		{SlotID: "identity-i", AuthType: "codex_device_auth", OrganizationName: strings.Repeat("組", 130)},
		{SlotID: "identity-j", AuthType: "codex_device_auth", OrganizationName: "\x00\x7f\u0085​"},
	}
	for _, vector := range identityCases {
		vector.Kind = "upstream_identity"
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
		values = append(values, vector)
	}
	for i := range values {
		fillHashVectorOutput(&values[i])
	}
	// One vector per line keeps the file reviewable and its diffs small.
	var out bytes.Buffer
	out.WriteString("[\n")
	for i, vector := range values {
		line, err := json.Marshal(vector)
		if err != nil {
			panic(err)
		}
		out.Write(line)
		if i < len(values)-1 {
			out.WriteByte(',')
		}
		out.WriteByte('\n')
	}
	out.WriteString("]\n")
	return out.Bytes()
}

// fillHashVectorOutput computes a vector's expected output with the production
// function it names.
func fillHashVectorOutput(vector *hashVector) {
	switch vector.Kind {
	case "short_hash":
		vector.Output = shortHash([]byte(vector.Input))
	case "operational_identifier_hash":
		vector.Output = operationalIdentifierHash(vector.KindInput, vector.Value)
	case "prompt_cache_key_hash":
		vector.Output = promptCacheKeyHash(vector.Model, vector.Source, vector.Value)
	case "api_key_fingerprint":
		vector.Output = apiKeyFingerprint(vector.Input)
	case "prompt_cache_bucket":
		vector.Bucket = promptCacheBucketIndex(vector.Input, vector.Buckets)
	case "upstream_identity":
		a := &app{}
		vector.Output = a.upstreamIdentityKeyLocked(vectorAccount(*vector))
	case "placement_score":
		a, item := vectorAppAndAccount(*vector)
		active := map[string]int{item.ID: vector.Active}
		vector.Identity = a.upstreamIdentityKeyLocked(item)
		sum := a.stickyBalanceScore(vector.StickyKey, item)
		vector.DigestHex = hex.EncodeToString(sum[:])
		vector.WeightBits = math.Float64bits(a.placementWeightLocked(item, active))
		vector.ScoreBits = math.Float64bits(a.placementScoreLocked(vector.StickyKey, item, active))
	default:
		panic("unknown hash vector kind " + vector.Kind)
	}
}

// generateStateVectors writes config.json and runtime.json documents the way
// the pool saves them, in two variants. "full" sets every field of every
// nested type, found by reflection, so a field the Rust types lack shows up
// as an unknown key there; "zero" leaves every value at its zero value, so
// Go's omission rules (omitempty, nil versus empty) are on record. The full
// documents also carry future fields at several depths, which Rust must keep.
func generateStateVectors(t *testing.T) []byte {
	t.Helper()
	documents := map[string]map[string]any{"config": {}, "runtime": {}}
	for _, full := range []bool{true, false} {
		variant := "zero"
		if full {
			variant = "full"
		}
		filler := &stateFiller{full: full}
		var cfg config
		filler.fill(reflect.ValueOf(&cfg).Elem())
		var runtime state
		filler.fill(reflect.ValueOf(&runtime).Elem())
		configDoc, runtimeDoc := genericJSON(t, cfg), genericJSON(t, runtime)
		if full {
			configDoc["futureConfigField"] = map[string]any{"kept": true}
			configDoc["futureNull"] = nil
			configDoc["accounts"].([]any)[0].(map[string]any)["futureAccountField"] = 7.5
			runtimeDoc["futureRuntimeField"] = "preserve-me"
			for _, sticky := range runtimeDoc["stickySessions"].(map[string]any) {
				sticky.(map[string]any)["futureStickyField"] = []any{"kept", 7.0}
			}
			for _, snapshot := range runtimeDoc["quotas"].(map[string]any) {
				quota := snapshot.(map[string]any)["quota"].(map[string]any)
				quota["primary"].(map[string]any)["futureWindowField"] = map[string]any{"nested": []any{}}
			}
		}
		documents["config"][variant] = configDoc
		documents["runtime"][variant] = runtimeDoc
	}
	data, err := json.MarshalIndent(documents, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	return append(data, '\n')
}

func genericJSON(t *testing.T, value any) map[string]any {
	t.Helper()
	raw, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err := json.Unmarshal(raw, &doc); err != nil {
		t.Fatal(err)
	}
	return doc
}

// stateFiller sets fields by reflection, so a field added to a persisted type
// lands in the fixtures without anyone remembering to add it. Values are
// distinct, integers exceed 32 bits (a Go int is 64 bits, and so must the
// Rust field be), floats have a fraction, and times carry nanoseconds and a
// non-UTC offset, so each would show a narrowing or rounding on the Rust side.
type stateFiller struct {
	full bool
	n    int
}

var stateFillerBase = time.Date(2026, 10, 21, 1, 2, 3, 123456789, time.FixedZone("", 8*60*60))

func (f *stateFiller) next() int {
	f.n++
	return f.n
}

func (f *stateFiller) fill(v reflect.Value) {
	if v.Type() == reflect.TypeOf(time.Time{}) {
		if f.full {
			n := f.next()
			v.Set(reflect.ValueOf(stateFillerBase.Add(time.Duration(n) * (time.Second + time.Nanosecond))))
		}
		return
	}
	switch v.Kind() {
	case reflect.Struct:
		for i := 0; i < v.NumField(); i++ {
			field := v.Type().Field(i)
			if !field.IsExported() || field.Tag.Get("json") == "-" {
				continue
			}
			f.fill(v.Field(i))
		}
	case reflect.Pointer:
		// A zero document leaves a pointer to a scalar nil, the common case,
		// but still walks a pointer to a struct so its fields' zero values
		// and omission rules are recorded.
		if !f.full && v.Type().Elem().Kind() != reflect.Struct {
			return
		}
		v.Set(reflect.New(v.Type().Elem()))
		f.fill(v.Elem())
	case reflect.Slice:
		element := reflect.New(v.Type().Elem()).Elem()
		f.fill(element)
		v.Set(reflect.Append(reflect.MakeSlice(v.Type(), 0, 1), element))
	case reflect.Map:
		if v.Type().Key().Kind() != reflect.String {
			panic("state vectors: map key " + v.Type().Key().String())
		}
		element := reflect.New(v.Type().Elem()).Elem()
		f.fill(element)
		v.Set(reflect.MakeMap(v.Type()))
		v.SetMapIndex(reflect.ValueOf(fmt.Sprintf("key-%d", f.next())).Convert(v.Type().Key()), element)
	case reflect.String:
		if f.full {
			v.SetString(fmt.Sprintf("value-%d", f.next()))
		}
	case reflect.Bool:
		v.SetBool(f.full)
	case reflect.Int, reflect.Int64:
		if f.full {
			v.SetInt(int64(1)<<33 + int64(f.next()))
		}
	case reflect.Uint64:
		if f.full {
			v.SetUint(uint64(1)<<33 + uint64(f.next()))
		}
	case reflect.Float64:
		if f.full {
			v.SetFloat(float64(f.next()) + 0.25)
		}
	default:
		// A new kind (an int32, an interface) needs a deliberate value here
		// and a matching Rust type; failing is how it gets noticed.
		panic("state vectors: unsupported kind " + v.Kind().String() + " in " + v.Type().String())
	}
}
