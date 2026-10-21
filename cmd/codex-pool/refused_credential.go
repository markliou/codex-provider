package main

import (
	"crypto/sha256"
	"encoding/json"
	"errors"
	"strings"
	"sync"
	"time"
)

// A credential upstream has refused for good is not asked again.
//
// A native Codex caches a permanent refresh failure for the auth snapshot it
// happened on, so later refreshes of the same credentials fail fast without
// network (login auth/manager.rs CachedAuth.permanent_refresh_failure); it
// asks again only once the user logs in and the credentials change. The pool
// once polled such an account about every five minutes regardless: an account that
// OpenAI deactivated answered 401 token_revoked about 280 times a day, each
// one a request a native Codex would never send. The pool now remembers the
// refusal against the account's current tokens and its background work
// (quota polling, refresh-all, priming) skips the account until auth.json
// holds different tokens, i.e. until someone logs in again. A single-account
// refresh from the management page still asks, once, because an operator
// asked for it.
//
// The record is in memory only and holds a digest of the tokens, never the
// tokens; after a restart the first poll asks once and records it again.

// errCodexCredentialRefused marks an error as upstream's permanent refusal of
// the credential, as opposed to a failure a later attempt could get past.
var errCodexCredentialRefused = errors.New("credential refused by upstream")

// refusedReasonRefresh is the reason recorded for a refused token refresh.
// Only that refusal makes later refreshes fail without network; a revoked
// access token alone still leaves the refresh Codex would try after a 401.
const refusedReasonRefresh = "token refresh refused"

type refusedCredential struct {
	Tokens [sha256.Size]byte
	Reason string
	At     time.Time
}

type refusedCredentials struct {
	mu        sync.Mutex
	byAccount map[string]refusedCredential
}

func (r *refusedCredentials) record(accountID string, tokens [sha256.Size]byte, reason string, now time.Time) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.byAccount == nil {
		r.byAccount = map[string]refusedCredential{}
	}
	r.byAccount[accountID] = refusedCredential{Tokens: tokens, Reason: reason, At: now}
}

// refused reports whether the account's current tokens are the ones upstream
// refused. Different tokens (a new login) clear the record.
func (r *refusedCredentials) refused(accountID string, tokens [sha256.Size]byte) (refusedCredential, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	record, ok := r.byAccount[accountID]
	if !ok {
		return refusedCredential{}, false
	}
	if record.Tokens != tokens {
		delete(r.byAccount, accountID)
		return refusedCredential{}, false
	}
	return record, true
}

func (r *refusedCredentials) clear(accountID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.byAccount, accountID)
}

// codexTokensDigest identifies the credentials in the account's auth.json.
func (a *app) codexTokensDigest(item account) ([sha256.Size]byte, bool) {
	auth, err := a.readCodexAuthFile(item)
	if err != nil || auth.Tokens == nil {
		return [sha256.Size]byte{}, false
	}
	return codexTokensDigestOf(auth), true
}

func codexTokensDigestOf(auth codexAuthFile) [sha256.Size]byte {
	if auth.Tokens == nil {
		return [sha256.Size]byte{}
	}
	return sha256.Sum256([]byte(auth.Tokens.AccessToken + "\x00" + auth.Tokens.RefreshToken))
}

// recordRefusedTokens records a refusal of tokens the caller already holds.
// It takes no app lock, so the token refresh can call it under the account's
// auth lock.
func (a *app) recordRefusedTokens(accountID string, tokens [sha256.Size]byte, reason string) {
	a.refusedCredentials.record(accountID, tokens, reason, time.Now().UTC())
	if a.logger != nil {
		a.logger.Printf("credential refused for %s: %s; automatic quota refresh stops until the account logs in again", accountID, reason)
	}
}

// recordRefusedCredential remembers that upstream refused the account's
// current tokens for good.
func (a *app) recordRefusedCredential(accountID, reason string) {
	a.mu.RLock()
	item := a.accountLocked(accountID)
	var accountCopy account
	if item != nil {
		accountCopy = *item
	}
	a.mu.RUnlock()
	if item == nil {
		return
	}
	tokens, ok := a.codexTokensDigest(accountCopy)
	if !ok {
		return
	}
	a.recordRefusedTokens(accountID, tokens, reason)
}

// credentialRefused reports whether background work must leave the
// account alone because upstream refused its current tokens.
func (a *app) credentialRefused(item account) bool {
	tokens, ok := a.codexTokensDigest(item)
	if !ok {
		return false
	}
	_, refused := a.refusedCredentials.refused(item.ID, tokens)
	return refused
}

// usageRefusesCredential is a usage or backend answer that revokes the access
// token itself, which no retry with the same tokens can get past.
func usageRefusesCredential(status int, code string) bool {
	if !upstreamAuthFailureStatus(status) {
		return false
	}
	switch sanitizedErrorCode(code) {
	case "token_revoked", "token_invalidated":
		return true
	}
	return false
}

// codexRefreshFailureIsPermanent is Codex's verdict on a failed token refresh
// (login auth/manager.rs request_chatgpt_token_refresh and
// classify_refresh_token_failure): permanent on a 401, on one of the three
// refresh_token_* codes, or on a 400 invalid_grant; anything else is
// transient. The code is read as login oauth/error.rs TokenErrorDetail::parse
// reads it: "error" as a string, else "error.code", else a top-level "code".
func codexRefreshFailureIsPermanent(status int, body []byte) bool {
	code := codexTokenErrorCode(body)
	switch strings.ToLower(code) {
	case "refresh_token_expired", "refresh_token_reused", "refresh_token_invalidated":
		return true
	}
	if status == 401 {
		return true
	}
	return status == 400 && strings.EqualFold(code, "invalid_grant")
}

func codexTokenErrorCode(body []byte) string {
	var parsed map[string]any
	if json.Unmarshal([]byte(strings.TrimSpace(string(body))), &parsed) != nil {
		return ""
	}
	nonEmpty := func(value any) string {
		text, _ := value.(string)
		if strings.TrimSpace(text) == "" {
			return ""
		}
		return text
	}
	if code := nonEmpty(parsed["error"]); code != "" {
		return code
	}
	if nested, ok := parsed["error"].(map[string]any); ok {
		if code := nonEmpty(nested["code"]); code != "" {
			return code
		}
	}
	return nonEmpty(parsed["code"])
}
