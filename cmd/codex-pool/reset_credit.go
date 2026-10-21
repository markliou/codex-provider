package main

import (
	"context"
	"encoding/json"
	"io"
	"math"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"
)

// Spending a reset credit the way a native Codex spends one.
//
// A native Codex resets from its /usage picker (tui chatwidget/usage.rs and
// reset_credits.rs, through app-server account_processor/rate_limit_resets.rs
// and backend-client rate_limit_resets.rs): it lists the account's credits,
// offers the available ones soonest-expiring first with the first selected,
// and spends the chosen one by id under a fresh idempotency key. A failed
// attempt's "Try again" resends the same key and the same credit, so a spend
// that went through but lost its answer comes back already_redeemed instead of
// spending a second credit. The pool once let upstream pick the credit and
// minted a new key on every click: a credit about to expire could outlive the
// reset meant to use it, and a retried click could spend twice.

const (
	// app-server rate_limit_resets.rs RATE_LIMIT_RESET_DETAILS_REQUEST_TIMEOUT
	// and RATE_LIMIT_RESET_REQUEST_TIMEOUT.
	resetCreditDetailsTimeout = 5 * time.Second
	resetCreditConsumeTimeout = 10 * time.Second
	// How long a failed attempt stays the one a retry resends. Codex keeps it
	// while its "Try again" prompt is open; the pool has no prompt, so it keeps
	// it for a while an operator would plausibly still be retrying.
	resetCreditRetryWindow = 10 * time.Minute
)

// resetCreditAttempt is a spend that failed without an upstream verdict. The
// credit id is upstream's identifier: it lives in memory for the retry only,
// never in runtime state, a log line or a response.
type resetCreditAttempt struct {
	RedeemRequestID string
	CreditID        string
	At              time.Time
}

type resetCreditAttempts struct {
	mu      sync.Mutex
	pending map[string]resetCreditAttempt
}

// resume returns the failed attempt a retry on this account must resend.
func (r *resetCreditAttempts) resume(accountID string, now time.Time) (resetCreditAttempt, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	attempt, ok := r.pending[accountID]
	if !ok || now.Sub(attempt.At) > resetCreditRetryWindow {
		delete(r.pending, accountID)
		return resetCreditAttempt{}, false
	}
	return attempt, true
}

func (r *resetCreditAttempts) keep(accountID string, attempt resetCreditAttempt) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.pending == nil {
		r.pending = map[string]resetCreditAttempt{}
	}
	r.pending[accountID] = attempt
}

func (r *resetCreditAttempts) clear(accountID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.pending, accountID)
}

// codexResetCreditListing is backend-client types.rs RateLimitResetCreditDetails
// as far as the picker reads it. The id is decoded only here, to spend the
// credit, and goes no further than the consume request.
type codexResetCreditListing struct {
	AvailableCount int64 `json:"available_count"`
	Credits        []struct {
		ID        string  `json:"id"`
		Status    string  `json:"status"`
		GrantedAt string  `json:"granted_at"`
		ExpiresAt *string `json:"expires_at"`
	} `json:"credits"`
}

// nativeResetCreditChoice is the credit a native Codex's picker selects first:
// available credits sorted by expiry, a credit without one last, ties in
// upstream's order (tui reset_credits.rs reset_credit_options, a stable sort).
// "" means Codex would spend without naming a credit: the listing failed, or a
// timestamp in it did not parse (app-server rejects the whole listing then),
// or it offers nothing, and the picker falls back to an option without an id.
func nativeResetCreditChoice(listing codexResetCreditListing) string {
	type candidate struct {
		id      string
		expires int64
	}
	var available []candidate
	for _, credit := range listing.Credits {
		if _, err := time.Parse(time.RFC3339, credit.GrantedAt); err != nil {
			return ""
		}
		expires := int64(math.MaxInt64)
		if credit.ExpiresAt != nil {
			at, err := time.Parse(time.RFC3339, *credit.ExpiresAt)
			if err != nil {
				return ""
			}
			expires = at.Unix()
		}
		if credit.Status == "available" {
			available = append(available, candidate{id: credit.ID, expires: expires})
		}
	}
	// The picker shows at most available_count credits.
	if listing.AvailableCount <= 0 || len(available) == 0 {
		return ""
	}
	sort.SliceStable(available, func(i, j int) bool { return available[i].expires < available[j].expires })
	return available[0].id
}

// chooseResetCredit lists the account's credits as the picker does before a
// spend, with the picker's timeout, and returns the credit it would select.
func (a *app) chooseResetCredit(ctx context.Context, accountID string, auth codexAuthInfo) string {
	ctx, cancel := context.WithTimeout(ctx, resetCreditDetailsTimeout)
	defer cancel()
	response, err := a.sendCodexOwnRequest(ctx, accountID, http.MethodGet, a.codexResetCreditsURL(), codexBackendHeaderOrder,
		codexBackendHeaders(a.clientAgentFor(accountID), auth, auth.AccountID), nil)
	if err != nil {
		return ""
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return ""
	}
	var listing codexResetCreditListing
	if err := json.NewDecoder(io.LimitReader(response.Body, maxRequestBody)).Decode(&listing); err != nil {
		return ""
	}
	return nativeResetCreditChoice(listing)
}

// codexConsumeResetCreditRequest is backend-client rate_limit_resets.rs
// ConsumeRateLimitResetCreditRequest: the fields in its declaration order
// (serde writes a struct in that order; a Go map would sort credit_id first),
// and credit_id left out when none is named.
type codexConsumeResetCreditRequest struct {
	RedeemRequestID string `json:"redeem_request_id"`
	CreditID        string `json:"credit_id,omitempty"`
}

// resetCreditNoLongerAvailable is the picker's answer to no_credit for a named
// credit (tui chatwidget/usage.rs finish_rate_limit_reset_consume).
func resetCreditNoLongerAvailable(outcome, creditID string) bool {
	switch strings.ToLower(strings.TrimSpace(outcome)) {
	case "no_credit", "nocredit", "nocreditsavailable", "nocredits":
		return creditID != ""
	}
	return false
}
