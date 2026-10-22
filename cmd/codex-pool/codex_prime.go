package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	mathrand "math/rand/v2"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

// A quota prime for a Codex account is one turn of the bundled Codex CLI
// (pinnedCodexVersion), so what upstream receives is a native Codex request —
// its body, headers, TLS and the calls a Codex makes when it starts — rather
// than a request the pool composed, which no native Codex sends.
//
// The CLI runs against a temporary CODEX_HOME holding the account's current
// access and ID tokens and no refresh token, and the account's installation id. Refresh tokens rotate on use: a
// CLI that refreshed would revoke the pool's own copy and sign the account out,
// so it is given nothing to refresh with. last_refresh is now, so Codex sees
// no reason to try. Nothing is written back; the directory is removed after.
// The child gets a minimal environment, never the pool's API keys or admin
// secrets.
//
// Without the CLI (tests, a stripped image) the prime is skipped, not
// replaced by a composed request.
func (a *app) primeWithCodexCLI(ctx context.Context, candidate account, model string) error {
	runner := a.codexPrimeRunner
	if runner == nil {
		if _, err := exec.LookPath("codex"); err != nil {
			return errors.New("bundled Codex CLI is unavailable; quota prime skipped")
		}
		runner = runCodexExecPrime
	}
	auth, err := a.refreshedCodexAuthContext(ctx, candidate)
	if err != nil {
		return fmt.Errorf("account credential: %w", err)
	}
	file, err := a.readCodexAuthFile(candidate)
	if err != nil || file.Tokens == nil {
		return errors.New("account credential file is unreadable")
	}
	root, err := os.MkdirTemp("", "codex-prime-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(root)
	home := filepath.Join(root, "home")
	workdir := filepath.Join(root, "work")
	for _, dir := range []string{home, workdir} {
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return err
		}
	}
	accountID := auth.AccountID
	if accountID == "" {
		accountID = candidate.AccountID
	}
	now := time.Now().UTC()
	primeAuth := map[string]any{
		"auth_mode":      "chatgpt",
		"OPENAI_API_KEY": nil,
		"tokens": map[string]any{
			"id_token":      file.Tokens.IDToken,
			"access_token":  auth.AccessToken,
			"refresh_token": "",
			"account_id":    accountID,
		},
		"last_refresh": now.Format(time.RFC3339Nano),
	}
	encoded, err := json.Marshal(primeAuth)
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), encoded, 0o600); err != nil {
		return err
	}
	// The CLI sends the installation id in its home; a temporary home would
	// make a new one for every prime. It gets the account's own, the one the
	// account presents on every request (installation_id.go).
	installationID, err := a.accountInstallationID(candidate)
	if err != nil {
		return fmt.Errorf("account installation id: %w", err)
	}
	if err := os.WriteFile(filepath.Join(home, codexInstallationIDFile), []byte(installationID), 0o644); err != nil {
		return err
	}
	config := "cli_auth_credentials_store = \"file\"\nmodel = " + fmt.Sprintf("%q", model) + "\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(config), 0o600); err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(ctx, quotaPrimeTimeout)
	defer cancel()
	return runner(ctx, home, workdir, model)
}

// runCodexExecPrime runs one ephemeral, read-only `codex exec` turn.
func runCodexExecPrime(ctx context.Context, home, workdir, model string) error {
	cmd := exec.CommandContext(ctx, "codex", "exec", "--skip-git-repo-check", "--ephemeral", "--sandbox", "read-only", "--color", "never", "-m", model, quotaPrimePrompt)
	cmd.Dir = workdir
	cmd.Env = codexChildEnvironment(home)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		if cmd.Process == nil {
			return nil
		}
		if err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL); err != nil && err != syscall.ESRCH {
			return err
		}
		return nil
	}
	cmd.WaitDelay = 5 * time.Second
	// Codex's output is the model's answer and Codex's own diagnostics; neither
	// is kept, and an error reports only the exit status.
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("codex exec: %w", err)
	}
	return nil
}

// codexChildEnvironment is what a Codex child process may see: where to find
// itself and its home, locale, and the CA and proxy settings the container
// gives every outbound client. Nothing else from the pool's environment.
func codexChildEnvironment(home string) []string {
	env := []string{"CODEX_HOME=" + home, "HOME=" + filepath.Dir(home)}
	for _, name := range []string{"PATH", "LANG", "LC_ALL", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR", "CODEX_CA_CERTIFICATE", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "https_proxy", "http_proxy", "no_proxy"} {
		if value, ok := os.LookupEnv(name); ok {
			env = append(env, name+"="+value)
		}
	}
	return env
}

// A window that reset is not primed at once. Its owner may be about to use
// the account, and that first request anchors the window the way it would for
// a native Codex, with no prime at all. The pool waits a random 5 to 30
// minutes from when it first saw the window unanchored, polls the account
// again when the wait ends (quota_schedule.go pollBy), and primes only if the
// window is still untouched; a request in between anchors it and the next poll
// drops the wait. The wait is in memory: after a restart the window is seen
// again and gets a new one.
const (
	quotaPrimeGraceMin = 5 * time.Minute
	quotaPrimeGraceMax = 30 * time.Minute
)

func randomQuotaPrimeGrace() time.Duration {
	return quotaPrimeGraceMin + time.Duration(mathrand.Float64()*float64(quotaPrimeGraceMax-quotaPrimeGraceMin))
}

// quotaPrimeGraceFromEnv is the grace: random by default, or the fixed
// CODEX_POOL_QUOTA_PRIME_GRACE, a test override (wire-capture.sh sets 0s so
// its prime phase need not wait).
func quotaPrimeGraceFromEnv() (func() time.Duration, error) {
	value := strings.TrimSpace(os.Getenv("CODEX_POOL_QUOTA_PRIME_GRACE"))
	if value == "" {
		return randomQuotaPrimeGrace, nil
	}
	fixed, err := time.ParseDuration(value)
	if err != nil || fixed < 0 {
		return nil, fmt.Errorf("CODEX_POOL_QUOTA_PRIME_GRACE must be a duration such as 0s or 10m, got %q", value)
	}
	return func() time.Duration { return fixed }, nil
}

// primeGraceElapsed starts the account's grace the first time its window is
// seen unanchored and reports whether it has run out.
func (a *app) primeGraceElapsed(accountID string, now time.Time) bool {
	a.primeGraceMu.Lock()
	defer a.primeGraceMu.Unlock()
	if a.primeGraceUntil == nil {
		a.primeGraceUntil = map[string]time.Time{}
	}
	until, ok := a.primeGraceUntil[accountID]
	if !ok {
		grace := randomQuotaPrimeGrace
		if a.quotaPrimeGrace != nil {
			grace = a.quotaPrimeGrace
		}
		until = now.Add(grace())
		a.primeGraceUntil[accountID] = until
	}
	if now.Before(until) {
		return false
	}
	delete(a.primeGraceUntil, accountID)
	return true
}

func (a *app) clearPrimeGrace(accountID string) {
	a.primeGraceMu.Lock()
	defer a.primeGraceMu.Unlock()
	delete(a.primeGraceUntil, accountID)
}

// primeGraceDue is when the account must be polled again to decide its prime.
func (a *app) primeGraceDue(accountID string) (time.Time, bool) {
	a.primeGraceMu.Lock()
	defer a.primeGraceMu.Unlock()
	until, ok := a.primeGraceUntil[accountID]
	return until, ok
}
