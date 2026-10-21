package main

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
)

// The credit a native Codex's /usage picker selects first: available only,
// soonest expiry first, one without an expiry last, ties in upstream's order
// (tui reset_credits.rs reset_credit_options).
func TestNativeResetCreditChoiceIsThePickersFirst(t *testing.T) {
	listing := func(raw string) codexResetCreditListing {
		var parsed codexResetCreditListing
		if err := json.Unmarshal([]byte(raw), &parsed); err != nil {
			t.Fatal(err)
		}
		return parsed
	}
	for _, tc := range []struct {
		name string
		raw  string
		want string
	}{
		{"soonest expiry first", `{"available_count":3,"credits":[
			{"id":"late","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-30T00:00:00Z"},
			{"id":"never","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":null},
			{"id":"soon","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-22T00:00:00Z"}]}`, "soon"},
		{"only available credits", `{"available_count":1,"credits":[
			{"id":"spent","status":"redeemed","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-21T00:00:00Z"},
			{"id":"busy","status":"redeeming","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-21T00:00:00Z"},
			{"id":"left","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-12-01T00:00:00Z"}]}`, "left"},
		{"ties keep upstream order", `{"available_count":2,"credits":[
			{"id":"first","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-01T00:00:00Z"},
			{"id":"second","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-01T00:00:00Z"}]}`, "first"},
		// app-server rejects a listing with a timestamp it cannot parse, and the
		// picker falls back to an option without an id.
		{"unparsable expiry names none", `{"available_count":1,"credits":[
			{"id":"odd","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"tomorrow"}]}`, ""},
		{"unparsable grant names none", `{"available_count":1,"credits":[
			{"id":"odd","status":"available","granted_at":"","expires_at":"2026-11-01T00:00:00Z"}]}`, ""},
		{"no available count names none", `{"available_count":0,"credits":[
			{"id":"x","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":null}]}`, ""},
	} {
		if got := nativeResetCreditChoice(listing(tc.raw)); got != tc.want {
			t.Fatalf("%s: chose %q, want %q", tc.name, got, tc.want)
		}
	}
}

// The consume body is Codex's ConsumeRateLimitResetCreditRequest: its field
// order, and no credit_id when none is named.
func TestConsumeResetCreditBodyIsCodexs(t *testing.T) {
	named, _ := codexJSON(codexConsumeResetCreditRequest{RedeemRequestID: "key", CreditID: "credit"})
	if string(named) != `{"redeem_request_id":"key","credit_id":"credit"}` {
		t.Fatalf("named body = %s", named)
	}
	unnamed, _ := codexJSON(codexConsumeResetCreditRequest{RedeemRequestID: "key"})
	if string(unnamed) != `{"redeem_request_id":"key"}` {
		t.Fatalf("unnamed body = %s", unnamed)
	}
}

type resetCreditUpstream struct {
	mu       sync.Mutex
	listings int
	bodies   []string
	// listingsAt is how many listings came before each consume: a resend must
	// not list again, while the quota refresh after a verdict does.
	listingsAt []int
	answers    []func(http.ResponseWriter)
}

func (u *resetCreditUpstream) serve(t *testing.T) *httptest.Server {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		u.mu.Lock()
		defer u.mu.Unlock()
		switch r.URL.Path {
		case "/backend-api/wham/rate-limit-reset-credits":
			u.listings++
			_, _ = io.WriteString(w, `{"available_count":2,"credits":[
				{"id":"credit-late","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-12-01T00:00:00Z"},
				{"id":"credit-soon","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-22T00:00:00Z"}]}`)
		case "/backend-api/wham/rate-limit-reset-credits/consume":
			body, _ := io.ReadAll(r.Body)
			u.bodies = append(u.bodies, string(body))
			u.listingsAt = append(u.listingsAt, u.listings)
			answer := u.answers[0]
			if len(u.answers) > 1 {
				u.answers = u.answers[1:]
			}
			answer(w)
		default:
			_, _ = io.WriteString(w, `{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000}},"rate_limit_reset_credits":{"available_count":1}}`)
		}
	}))
	t.Cleanup(server.Close)
	return server
}

func resetCreditTestApp(t *testing.T, upstream *httptest.Server) *app {
	a := testApp(t, []account{{ID: "acct", AuthType: "codex_device_auth", Enabled: true, InPool: true, Priority: 100}})
	a.config.Accounts[0].CodexHome = a.accountCodexHome("acct")
	a.codexBaseURL = upstream.URL + "/backend-api"
	writeTestCodexAuth(t, a, "acct")
	credits := int64(2)
	a.state.Quotas["acct"] = quotaSnapshot{AccountID: "acct", Quota: &accountQuota{ResetCredits: &quotaResetCredits{AvailableCount: &credits}}}
	return a
}

func pressResetCredit(t *testing.T, a *app) (int, map[string]any) {
	cookies, csrf := adminSession(t, a)
	request := httptest.NewRequest(http.MethodPost, "/admin/api/accounts/acct/quota/reset-credit", nil)
	for _, cookie := range cookies {
		request.AddCookie(cookie)
	}
	request.Header.Set("X-CSRF-Token", csrf)
	recorder := httptest.NewRecorder()
	a.adminMux().ServeHTTP(recorder, request)
	var body map[string]any
	_ = json.Unmarshal(recorder.Body.Bytes(), &body)
	return recorder.Code, body
}

// A spend lists the credits first and names the one expiring soonest, so a
// red-outlined credit is the one the reset uses.
func TestResetCreditSpendsTheSoonestExpiringCredit(t *testing.T) {
	upstream := &resetCreditUpstream{answers: []func(http.ResponseWriter){func(w http.ResponseWriter) { _, _ = io.WriteString(w, `{"code":"reset"}`) }}}
	a := resetCreditTestApp(t, upstream.serve(t))
	code, body := pressResetCredit(t, a)
	if code != http.StatusOK || body["consumed"] != true {
		t.Fatalf("reset returned %d %v", code, body)
	}
	var sent codexConsumeResetCreditRequest
	_ = json.Unmarshal([]byte(upstream.bodies[0]), &sent)
	if upstream.listings == 0 || sent.CreditID != "credit-soon" || sent.RedeemRequestID == "" {
		t.Fatalf("listings=%d consume body %s", upstream.listings, upstream.bodies[0])
	}
	// The credit id is upstream's identifier: it goes to the consume request
	// and nowhere else.
	encoded, _ := json.Marshal(body)
	if strings.Contains(string(encoded), "credit-soon") {
		t.Fatalf("the credit id reached the admin response: %s", encoded)
	}
}

// A failed spend is resent unchanged by the next press, as Codex's "Try again"
// resends it: same key, same credit, no new listing. The resent attempt that
// upstream already honoured answers already_redeemed, which is a reset.
func TestResetCreditRetryResendsTheFailedAttempt(t *testing.T) {
	upstream := &resetCreditUpstream{answers: []func(http.ResponseWriter){
		func(w http.ResponseWriter) { w.WriteHeader(http.StatusBadGateway) },
		func(w http.ResponseWriter) { _, _ = io.WriteString(w, `{"code":"already_redeemed"}`) },
		func(w http.ResponseWriter) { _, _ = io.WriteString(w, `{"code":"reset"}`) },
	}}
	a := resetCreditTestApp(t, upstream.serve(t))
	if code, _ := pressResetCredit(t, a); code != http.StatusBadGateway {
		t.Fatalf("failed spend returned %d", code)
	}
	code, body := pressResetCredit(t, a)
	if code != http.StatusOK || body["consumed"] != true {
		t.Fatalf("resent spend returned %d %v", code, body)
	}
	if len(upstream.bodies) != 2 || upstream.bodies[0] != upstream.bodies[1] || upstream.listingsAt[1] != upstream.listingsAt[0] {
		t.Fatalf("retry was not a resend: listings before each consume=%v bodies=%q", upstream.listingsAt, upstream.bodies)
	}
	// The verdict ended that attempt: the next press is a new spend.
	if code, _ := pressResetCredit(t, a); code != http.StatusOK {
		t.Fatalf("next spend returned %d", code)
	}
	if upstream.listingsAt[2] <= upstream.listingsAt[1] || upstream.bodies[2] == upstream.bodies[1] {
		t.Fatalf("a spend after a verdict reused the old attempt: listings before each consume=%v bodies=%q", upstream.listingsAt, upstream.bodies)
	}
}

// no_credit for a named credit means that credit is gone, as Codex's picker
// tells its user, not that the account has none.
func TestResetCreditNamedCreditGone(t *testing.T) {
	upstream := &resetCreditUpstream{answers: []func(http.ResponseWriter){func(w http.ResponseWriter) { _, _ = io.WriteString(w, `{"code":"no_credit"}`) }}}
	a := resetCreditTestApp(t, upstream.serve(t))
	code, body := pressResetCredit(t, a)
	if code != http.StatusOK || body["consumed"] != false || !strings.Contains(body["message"].(string), "no longer available") {
		t.Fatalf("named no_credit returned %d %v", code, body)
	}
}
