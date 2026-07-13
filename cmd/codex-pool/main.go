package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode"
)

const (
	publicAddressDefault      = ":8317"
	adminAddressDefault       = "127.0.0.1:8318"
	codexBaseURLDefault       = "https://chatgpt.com/backend-api"
	cliproxyBaseURLDefault    = "http://127.0.0.1:8319/v1"
	codexRefreshURLDefault    = "https://auth.openai.com/oauth/token"
	codexOAuthClientIDDefault = "app_EMoamEEZ73f0CkXaXp7hrann"
	chatGPTWebReferer         = "https://chatgpt.com/"
	chatGPTWebUserAgent       = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"
	codexTokenRefreshWindow   = 5 * time.Minute
	adminLoginMaxFailures     = 5
	adminLoginLockout         = 15 * time.Minute
	maxRequestBody            = 16 << 20
	sessionLifetime           = 12 * time.Hour
	sessionAffinityTTLDefault = 24 * time.Hour
	accountActiveWindow       = 60 * time.Second
	routingStrategyBalanced   = "sticky_balanced"
	routingStrategyFailover   = "sticky_failover"
	// promptCacheMinTokens is OpenAI's minimum prompt size for caching to engage;
	// requests below it can never cache, so they are excluded from cold-start
	// accounting.
	promptCacheMinTokens    = 1024
	maxRequestIdentityValue = 512
	// promptCacheBucketsDefault spreads a coarse (project/user) prompt cache key
	// across a few buckets so a hot scope stays under OpenAI's ~15 RPM per
	// (prefix + prompt_cache_key) limit while still sharing the static prefix
	// across conversations. 4 covers a heavy single user (~60 RPM) before any
	// overflow; raise it if the dashboard shows a hot account with a low hit rate.
	promptCacheBucketsDefault = 4
	quotaRefreshInterval      = 5 * time.Minute
	quotaRefreshTimeout       = 30 * time.Second
	codexAuthReadAttempts     = 20
	codexAuthReadRetryDelay   = 50 * time.Millisecond
	upstreamFirstByteTimeout  = 45 * time.Second
	upstream5xxCooldown       = 10 * time.Second
	upstream5xxFailoverAfter  = 3
	upstream5xxFailureWindow  = 2 * time.Minute
	// Codex treats remote model metadata as authoritative for ChatGPT-backed
	// providers. This fallback must remain non-empty or a schema-only fix would
	// silently remove the coding-agent instructions after a successful refresh.
	codexModelBaseInstructions = "You are Codex, a coding agent running in a terminal-based coding assistant. Inspect the workspace before acting, follow repository instructions such as AGENTS.md, use the available tools to implement and verify changes, keep the user informed, and continue until the task is genuinely handled."
)

var (
	errAccountAuthFailed = errors.New("account authentication failed")
	errCodexAuthMissing  = errors.New("codex auth missing")
)

type config struct {
	DefaultModel     string            `json:"defaultModel"`
	ModelAliases     map[string]string `json:"modelAliases"`
	Accounts         []account         `json:"accounts"`
	PreserveProQuota *bool             `json:"preserveProQuota,omitempty"`
	CreatedAt        time.Time         `json:"createdAt"`
	UpdatedAt        time.Time         `json:"updatedAt"`
}

type account struct {
	ID               string `json:"id"`
	Label            string `json:"label"`
	Email            string `json:"email,omitempty"`
	AccountID        string `json:"accountId,omitempty"`
	OrganizationName string `json:"organizationName,omitempty"`
	// Deprecated: migrated into OrganizationName at load time. Not exposed in admin APIs.
	OrganizationNameOverride string   `json:"organizationNameOverride,omitempty"`
	PlanType                 string   `json:"planType,omitempty"`
	PlanLimit                string   `json:"planLimit,omitempty"`
	PlanRank                 int      `json:"planRank,omitempty"`
	AuthType                 string   `json:"authType"`
	CodexHome                string   `json:"codexHome,omitempty"`
	Enabled                  bool     `json:"enabled"`
	InPool                   bool     `json:"inPool"`
	Priority                 int      `json:"priority"`
	RemainingQuota           *int     `json:"remainingQuota,omitempty"`
	AllowedModels            []string `json:"allowedModels,omitempty"`
	ExcludedModels           []string `json:"excludedModels,omitempty"`
	UpstreamBaseURL          string   `json:"upstreamBaseUrl,omitempty"`
	UpstreamAPIKey           string   `json:"upstreamApiKey,omitempty"`
	WireAPI                  string   `json:"wireApi,omitempty"`
	// PendingPoolActivation keeps a newly-created device-auth slot disabled and
	// out of the pool until login has produced usable auth and gateway state.
	// Without this staging flag, empty slots can stall status/routing paths while
	// they repeatedly classify missing auth under the global state lock.
	PendingPoolActivation bool      `json:"pendingPoolActivation,omitempty"`
	CreatedAt             time.Time `json:"createdAt"`
	UpdatedAt             time.Time `json:"updatedAt"`
	LastLoginAt           time.Time `json:"lastLoginAt,omitempty"`
}

type cooldown struct {
	ModelID     string    `json:"modelId"`
	NextRetryAt time.Time `json:"nextRetryAt"`
	Reason      string    `json:"reason"`
}

type quotaWindow struct {
	Percentage    int    `json:"percentage"`
	ResetAt       *int64 `json:"resetAt,omitempty"`
	WindowMinutes *int64 `json:"windowMinutes,omitempty"`
	Present       bool   `json:"present"`
}

type accountQuota struct {
	Hourly quotaWindow `json:"hourly"`
	Weekly quotaWindow `json:"weekly"`
}

type quotaErrorInfo struct {
	Code      string    `json:"code,omitempty"`
	Message   string    `json:"message"`
	Timestamp time.Time `json:"timestamp"`
}

type quotaSnapshot struct {
	AccountID        string          `json:"accountId"`
	OrganizationName string          `json:"organizationName,omitempty"`
	PlanType         string          `json:"planType,omitempty"`
	PlanLimit        string          `json:"planLimit,omitempty"`
	Quota            *accountQuota   `json:"quota,omitempty"`
	UsageUpdatedAt   time.Time       `json:"usageUpdatedAt,omitempty"`
	QuotaError       *quotaErrorInfo `json:"quotaError,omitempty"`
}

type stickySession struct {
	Key           string    `json:"key"`
	ModelID       string    `json:"modelId"`
	AccountID     string    `json:"accountId"`
	CreatedAt     time.Time `json:"createdAt"`
	LastSuccessAt time.Time `json:"lastSuccessAt"`
	ExpiresAt     time.Time `json:"expiresAt,omitempty"`
	FailoverFrom  string    `json:"failoverFrom,omitempty"`
}

type responseBinding struct {
	ResponseID string    `json:"responseId"`
	StickyKey  string    `json:"stickyKey"`
	ModelID    string    `json:"modelId"`
	AccountID  string    `json:"accountId"`
	CreatedAt  time.Time `json:"createdAt"`
	ExpiresAt  time.Time `json:"expiresAt"`
}

type requestIdentity struct {
	SessionID      string `json:"sessionId,omitempty"`
	ThreadID       string `json:"threadId,omitempty"`
	ParentThreadID string `json:"parentThreadId,omitempty"`
	ForkedFromID   string `json:"forkedFromThreadId,omitempty"`
	LineageRootID  string `json:"lineageRootId,omitempty"`
	SubagentKind   string `json:"subagentKind,omitempty"`
	ThreadSource   string `json:"threadSource,omitempty"`
	IsSubagent     bool   `json:"isSubagent"`
}

type threadBinding struct {
	ThreadID       string    `json:"threadId"`
	SessionID      string    `json:"sessionId,omitempty"`
	ParentThreadID string    `json:"parentThreadId,omitempty"`
	LineageRootID  string    `json:"lineageRootId"`
	SubagentKind   string    `json:"subagentKind,omitempty"`
	ModelID        string    `json:"modelId"`
	AccountID      string    `json:"accountId"`
	StickyKey      string    `json:"stickyKey"`
	PromptCacheKey string    `json:"promptCacheKey,omitempty"`
	CreatedAt      time.Time `json:"createdAt"`
	LastSuccessAt  time.Time `json:"lastSuccessAt"`
	ExpiresAt      time.Time `json:"expiresAt"`
}

type promptCacheStat struct {
	AccountID    string `json:"accountId"`
	ModelID      string `json:"modelId"`
	AgentKind    string `json:"agentKind,omitempty"`
	RequestCount uint64 `json:"requestCount"`
	InputTokens  uint64 `json:"inputTokens"`
	CachedTokens uint64 `json:"cachedTokens"`
	// ColdRequestCount counts cache-eligible requests (input >= 1024 tokens)
	// that returned zero cached tokens, i.e. a cold start. It quantifies why a
	// hit rate is low: new conversations, failover hand-offs, or 15 RPM overflow.
	ColdRequestCount            uint64    `json:"coldRequestCount"`
	ParentAffinityHitCount      uint64    `json:"parentAffinityHitCount,omitempty"`
	ParentAffinityFallbackCount uint64    `json:"parentAffinityFallbackCount,omitempty"`
	LineageFailoverCount        uint64    `json:"lineageFailoverCount,omitempty"`
	UpdatedAt                   time.Time `json:"updatedAt"`
}

type accountHealth struct {
	LastSuccessAt      time.Time `json:"lastSuccessAt,omitempty"`
	LastFailureAt      time.Time `json:"lastFailureAt,omitempty"`
	LastFailureReason  string    `json:"lastFailureReason,omitempty"`
	ConsecutiveFailure int       `json:"consecutiveFailure"`
}

type loginJob struct {
	ID              string    `json:"jobId"`
	Type            string    `json:"type"`
	Status          string    `json:"status"`
	AccountID       string    `json:"accountId"`
	VerificationURL string    `json:"verificationUrl,omitempty"`
	UserCode        string    `json:"userCode,omitempty"`
	CodeExpiresAt   time.Time `json:"codeExpiresAt,omitempty"`
	Message         string    `json:"message,omitempty"`
	Error           string    `json:"error,omitempty"`
	StartedAt       time.Time `json:"startedAt"`
	UpdatedAt       time.Time `json:"updatedAt"`
	CompletedAt     time.Time `json:"completedAt,omitempty"`
}

type loginFailure struct {
	Count       int
	LockedOutAt time.Time
	LastFailure time.Time
}

type state struct {
	StickySessions   map[string]stickySession   `json:"stickySessions"`
	ResponseBindings map[string]responseBinding `json:"responseBindings,omitempty"`
	ThreadBindings   map[string]threadBinding   `json:"threadBindings,omitempty"`
	Cooldowns        map[string][]cooldown      `json:"cooldowns"`
	Health           map[string]accountHealth   `json:"health"`
	Quotas           map[string]quotaSnapshot   `json:"quotas,omitempty"`
	PromptCache      map[string]promptCacheStat `json:"promptCache,omitempty"`
	// PromptCacheBaseline snapshots PromptCache at the last reset so the
	// dashboard can show a "since reset" hit rate over fresh traffic only, which
	// the slow-moving lifetime total cannot reveal. PromptCacheResetAt is the
	// reset timestamp (zero == never reset, so the window equals lifetime).
	PromptCacheBaseline map[string]promptCacheStat `json:"promptCacheBaseline,omitempty"`
	PromptCacheResetAt  time.Time                  `json:"promptCacheResetAt,omitempty"`
	// PromptCacheResetAtByAccount records per-account window resets so a single
	// account's hit rate can be recalculated independently of the pool-wide reset.
	PromptCacheResetAtByAccount map[string]time.Time `json:"promptCacheResetAtByAccount,omitempty"`
	RequestCount                uint64               `json:"requestCount"`
	SuccessCount                uint64               `json:"successCount"`
	FailureCount                uint64               `json:"failureCount"`
	UpdatedAt                   time.Time            `json:"updatedAt"`
}

type app struct {
	mu                   sync.RWMutex
	authLockMu           sync.Mutex
	config               config
	state                state
	dataDir              string
	apiKeys              [][]byte
	adminUser            string
	adminHash            []byte
	sessionKey           []byte
	sessionAffinityTTL   time.Duration
	maxRetryAccounts     int
	routingStrategy      string
	promptCacheKeyMode   string
	promptCacheKeyScope  string
	promptCacheKeyPolicy string
	promptCacheBuckets   int
	promptCacheRetention string
	preserveProQuota     bool
	publicAddress        string
	adminAddress         string
	allowRemoteAdmin     bool
	publicDashboard      bool
	codexBaseURL         string
	codexGatewayMode     string
	cliproxyBaseURL      string
	cliproxyAPIKey       string
	jobs                 map[string]*loginJob
	loginCancels         map[string]context.CancelFunc
	loginFailures        map[string]loginFailure
	authLocks            map[string]*sync.Mutex
	client               *http.Client
	streamClient         *http.Client
	logger               *log.Logger
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "hash-password" {
		password := os.Getenv("CODEX_POOL_ADMIN_PASSWORD")
		if password == "" {
			log.Fatal("CODEX_POOL_ADMIN_PASSWORD is required for hash-password")
		}
		hash, err := newPasswordHash(password)
		if err != nil {
			log.Fatalf("generate password hash: %v", err)
		}
		fmt.Println(hash)
		return
	}

	a, err := newAppFromEnv()
	if err != nil {
		log.Fatal(err)
	}
	if err := a.serve(); err != nil {
		log.Fatal(err)
	}
}

func newAppFromEnv() (*app, error) {
	keys, err := loadAPIKeys()
	if err != nil {
		return nil, err
	}
	adminHash := os.Getenv("CODEX_POOL_ADMIN_PASSWORD_HASH")
	if adminHash == "" {
		return nil, errors.New("CODEX_POOL_ADMIN_PASSWORD_HASH is required")
	}
	if !validPasswordHash(adminHash) {
		return nil, errors.New("CODEX_POOL_ADMIN_PASSWORD_HASH must be a valid pbkdf2-sha256 hash generated by hash-password")
	}
	publicAddress := envOr("CODEX_POOL_PUBLIC_ADDR", publicAddressDefault)
	adminAddress := envOr("CODEX_POOL_ADMIN_ADDR", adminAddressDefault)
	allowRemote := os.Getenv("CODEX_POOL_ALLOW_REMOTE_ADMIN") == "true"
	if !allowRemote && !isLoopbackAddress(adminAddress) {
		return nil, errors.New("admin address must be loopback unless CODEX_POOL_ALLOW_REMOTE_ADMIN=true")
	}
	sessionAffinityTTL, err := sessionAffinityTTLFromEnv()
	if err != nil {
		return nil, err
	}
	maxRetryAccounts, err := maxRetryAccountsFromEnv()
	if err != nil {
		return nil, err
	}
	routingStrategy, err := routingStrategyFromEnv()
	if err != nil {
		return nil, err
	}
	codexGatewayMode, err := codexGatewayModeFromEnv()
	if err != nil {
		return nil, err
	}
	promptCacheKeyMode, err := promptCacheKeyModeFromEnv()
	if err != nil {
		return nil, err
	}
	promptCacheKeyScope, err := promptCacheKeyScopeFromEnv()
	if err != nil {
		return nil, err
	}
	promptCacheKeyPolicy, err := promptCacheKeyPolicyFromEnv()
	if err != nil {
		return nil, err
	}
	promptCacheBuckets, err := promptCacheBucketsFromEnv()
	if err != nil {
		return nil, err
	}
	promptCacheRetention, err := promptCacheRetentionFromEnv()
	if err != nil {
		return nil, err
	}
	preserveProQuota, err := boolFromEnv("CODEX_POOL_PRESERVE_PRO_QUOTA")
	if err != nil {
		return nil, err
	}
	// Product contract: the admin-port root is a public control page, while
	// management actions require password auth. A previous hardening pass flipped
	// this default and broke the expected landing page, so keep the default true
	// unless the operator explicitly hides the public control view.
	publicDashboard, err := boolFromEnvDefault("CODEX_POOL_PUBLIC_DASHBOARD", true)
	if err != nil {
		return nil, err
	}

	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		return nil, fmt.Errorf("generate session key: %w", err)
	}
	a := &app{
		dataDir:              envOr("CODEX_POOL_DATA_DIR", "/data"),
		apiKeys:              keys,
		adminUser:            envOr("CODEX_POOL_ADMIN_USERNAME", "admin"),
		adminHash:            []byte(adminHash),
		sessionKey:           key,
		sessionAffinityTTL:   sessionAffinityTTL,
		maxRetryAccounts:     maxRetryAccounts,
		routingStrategy:      routingStrategy,
		promptCacheKeyMode:   promptCacheKeyMode,
		promptCacheKeyScope:  promptCacheKeyScope,
		promptCacheKeyPolicy: promptCacheKeyPolicy,
		promptCacheBuckets:   promptCacheBuckets,
		promptCacheRetention: promptCacheRetention,
		preserveProQuota:     preserveProQuota,
		publicAddress:        publicAddress,
		adminAddress:         adminAddress,
		allowRemoteAdmin:     allowRemote,
		publicDashboard:      publicDashboard,
		codexBaseURL:         strings.TrimRight(envOr("CODEX_POOL_CODEX_BASE_URL", codexBaseURLDefault), "/"),
		codexGatewayMode:     codexGatewayMode,
		cliproxyBaseURL:      strings.TrimRight(envOr("CODEX_POOL_CLIPROXY_BASE_URL", cliproxyBaseURLDefault), "/"),
		cliproxyAPIKey:       strings.TrimSpace(os.Getenv("CODEX_POOL_CLIPROXY_API_KEY")),
		jobs:                 map[string]*loginJob{},
		loginCancels:         map[string]context.CancelFunc{},
		loginFailures:        map[string]loginFailure{},
		authLocks:            map[string]*sync.Mutex{},
		client:               defaultHTTPClient(),
		streamClient:         streamingHTTPClient(),
		logger:               log.New(os.Stdout, "codex-pool ", log.LstdFlags|log.LUTC),
	}
	if err := a.load(); err != nil {
		return nil, err
	}
	return a, nil
}

func defaultHTTPClient() *http.Client {
	base, ok := http.DefaultTransport.(*http.Transport)
	if !ok {
		return &http.Client{Timeout: 5 * time.Minute}
	}
	transport := base.Clone()
	// Bound first-byte wait per selected account so one stalled sidecar/upstream
	// cannot hold routing for minutes before Pool can fail over or return.
	transport.ResponseHeaderTimeout = upstreamFirstByteTimeout
	return &http.Client{Timeout: 5 * time.Minute, Transport: transport}
}

// streamingHTTPClient must not carry an overall timeout: http.Client.Timeout
// covers reading the whole body, so it cuts live SSE generations mid-stream
// (heavy reasoning models regularly run past five minutes, and the client then
// loops on "reconnecting" retries). First-byte wait stays bounded through the
// transport, and a disconnected client still cancels via the request context.
func streamingHTTPClient() *http.Client {
	base, ok := http.DefaultTransport.(*http.Transport)
	if !ok {
		return &http.Client{}
	}
	transport := base.Clone()
	transport.ResponseHeaderTimeout = upstreamFirstByteTimeout
	return &http.Client{Transport: transport}
}

func (a *app) load() error {
	if err := os.MkdirAll(filepath.Join(a.dataDir, "state"), 0o700); err != nil {
		return fmt.Errorf("create data directory: %w", err)
	}
	a.config = config{DefaultModel: envOr("CODEX_POOL_DEFAULT_MODEL", "gpt-5.5(xhigh)"), ModelAliases: map[string]string{}}
	a.state = state{StickySessions: map[string]stickySession{}, ResponseBindings: map[string]responseBinding{}, ThreadBindings: map[string]threadBinding{}, Cooldowns: map[string][]cooldown{}, Health: map[string]accountHealth{}, Quotas: map[string]quotaSnapshot{}, PromptCache: map[string]promptCacheStat{}}
	if err := readJSON(filepath.Join(a.dataDir, "config.json"), &a.config); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("read config: %w", err)
	}
	if a.config.PreserveProQuota != nil {
		a.preserveProQuota = *a.config.PreserveProQuota
	}
	if configuredDefault := strings.TrimSpace(os.Getenv("CODEX_POOL_DEFAULT_MODEL")); configuredDefault != "" {
		a.config.DefaultModel = configuredDefault
	}
	if strings.TrimSpace(a.config.DefaultModel) == "" {
		a.config.DefaultModel = "gpt-5.5(xhigh)"
	}
	if err := readJSON(filepath.Join(a.dataDir, "state", "runtime.json"), &a.state); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("read runtime state: %w", err)
	}
	if a.config.ModelAliases == nil {
		a.config.ModelAliases = map[string]string{}
	}
	if a.state.StickySessions == nil {
		a.state.StickySessions = map[string]stickySession{}
	}
	if a.state.ResponseBindings == nil {
		a.state.ResponseBindings = map[string]responseBinding{}
	}
	if a.state.ThreadBindings == nil {
		a.state.ThreadBindings = map[string]threadBinding{}
	}
	if a.state.Cooldowns == nil {
		a.state.Cooldowns = map[string][]cooldown{}
	}
	if a.state.Health == nil {
		a.state.Health = map[string]accountHealth{}
	}
	if a.state.Quotas == nil {
		a.state.Quotas = map[string]quotaSnapshot{}
	}
	if a.state.PromptCache == nil {
		a.state.PromptCache = map[string]promptCacheStat{}
	}
	if a.pruneExpiredRuntimeStateLocked(time.Now().UTC()) {
		_ = a.saveLocked()
	}
	for i := range a.config.Accounts {
		a.config.Accounts[i].Email = normalizeEmail(a.config.Accounts[i].Email)
		a.config.Accounts[i].OrganizationName = cleanOrganizationName(a.config.Accounts[i].OrganizationName)
		if a.config.Accounts[i].OrganizationName == "" {
			a.config.Accounts[i].OrganizationName = cleanOrganizationName(a.config.Accounts[i].OrganizationNameOverride)
		}
		a.config.Accounts[i].OrganizationNameOverride = ""
		a.config.Accounts[i].PlanType = normalizePlanType(a.config.Accounts[i].PlanType)
		a.config.Accounts[i].PlanLimit = cleanPlanLimit(a.config.Accounts[i].PlanLimit)
		a.config.Accounts[i].PlanRank = planRank(a.config.Accounts[i].PlanType)
		if strings.TrimSpace(a.config.Accounts[i].Label) == "" {
			a.config.Accounts[i].Label = accountDisplayName(a.config.Accounts[i])
		}
		if isCodexDeviceAuth(a.config.Accounts[i]) {
			a.config.Accounts[i].CodexHome = a.accountCodexHome(a.config.Accounts[i].ID)
			a.config.Accounts[i].UpstreamBaseURL = ""
			a.config.Accounts[i].UpstreamAPIKey = ""
		}
	}
	if a.codexGatewayMode != "direct" {
		for _, account := range a.config.Accounts {
			if isCodexDeviceAuth(account) {
				_ = a.syncCliproxyAuth(account, false)
			}
		}
	}
	if len(a.config.Accounts) == 0 && os.Getenv("CODEX_POOL_UPSTREAM_BASE_URL") != "" {
		now := time.Now().UTC()
		a.config.Accounts = []account{{
			ID:              "provider-default",
			Label:           "Default provider",
			AuthType:        "provider_api_key",
			Enabled:         true,
			InPool:          true,
			Priority:        100,
			UpstreamBaseURL: strings.TrimRight(os.Getenv("CODEX_POOL_UPSTREAM_BASE_URL"), "/"),
			UpstreamAPIKey:  os.Getenv("CODEX_POOL_UPSTREAM_API_KEY"),
			WireAPI:         normalWireAPI(os.Getenv("CODEX_POOL_UPSTREAM_WIRE_API")),
			CreatedAt:       now,
			UpdatedAt:       now,
		}}
		a.config.CreatedAt = now
		if err := a.saveLocked(); err != nil {
			return err
		}
	}
	return nil
}

func (a *app) serve() error {
	public := &http.Server{Addr: a.publicAddress, Handler: a.publicMux(), ReadHeaderTimeout: 10 * time.Second}
	admin := &http.Server{Addr: a.adminAddress, Handler: a.adminMux(), ReadHeaderTimeout: 10 * time.Second}
	publicListener, err := net.Listen("tcp", a.publicAddress)
	if err != nil {
		return fmt.Errorf("listen public API: %w", err)
	}
	adminListener, err := net.Listen("tcp", a.adminAddress)
	if err != nil {
		_ = publicListener.Close()
		return fmt.Errorf("listen admin API: %w", err)
	}
	a.logger.Printf("public API listening on %s; admin listening on %s", a.publicAddress, a.adminAddress)
	a.startQuotaRefresher(context.Background())
	errCh := make(chan error, 2)
	go func() { errCh <- public.Serve(publicListener) }()
	go func() { errCh <- admin.Serve(adminListener) }()
	err = <-errCh
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

func (a *app) startQuotaRefresher(ctx context.Context) {
	go func() {
		a.refreshAllCodexQuotas(ctx)
		ticker := time.NewTicker(quotaRefreshInterval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				a.refreshAllCodexQuotas(ctx)
			}
		}
	}()
}

func (a *app) refreshAllCodexQuotas(ctx context.Context) {
	a.mu.RLock()
	accountIDs := make([]string, 0, len(a.config.Accounts))
	for _, account := range a.config.Accounts {
		if isCodexDeviceAuth(account) {
			accountIDs = append(accountIDs, account.ID)
		}
	}
	a.mu.RUnlock()
	for _, accountID := range accountIDs {
		if _, err := a.refreshAccountQuota(ctx, accountID); err != nil {
			a.logger.Printf("quota refresh skipped for %s: %s", accountID, err)
		}
	}
}

func (a *app) publicMux() http.Handler {
	mux := http.NewServeMux()
	// Surface contract: the public API port is for authenticated API calls only.
	// Its root must stay dark so a browser scan of the API entrypoint does not
	// advertise the service or the control page.
	mux.HandleFunc("GET /{$}", a.handleNotFound)
	mux.HandleFunc("GET /healthz", a.requireAPIKey(a.handleHealthz))
	mux.HandleFunc("GET /v1/codex-pool/status", a.requireAPIKey(a.handleCurrentStatus))
	mux.HandleFunc("GET /v1/models", a.requireAPIKey(a.handleModels))
	mux.HandleFunc("POST /v1/responses", a.requireAPIKey(a.handleResponses))
	mux.HandleFunc("POST /v1/responses/compact", a.requireAPIKey(a.handleResponses))
	mux.HandleFunc("POST /v1/chat/completions", a.requireAPIKey(a.handleChatCompletions))
	return recovery(mux)
}

func (a *app) adminMux() http.Handler {
	mux := http.NewServeMux()
	// Surface contract: the admin-port root is the public control page. The page
	// itself is intentionally visible without a password; owner-only actions are
	// protected by requireAdmin on the management API routes below.
	//
	// The unauthenticated/login chrome intentionally uses low-key wording. Do not
	// "clarify" the visible title or login copy into obvious Codex/pool/provider
	// management terms without owner approval: casual browsing and keyword probes
	// should not learn more than the public control surface must reveal. This is
	// only passive exposure reduction; requireAdmin remains the security boundary.
	mux.HandleFunc("GET /{$}", a.handleAdminPage)
	mux.HandleFunc("GET /admin", a.handleAdminPage)
	mux.HandleFunc("GET /admin/assets/app.css", handleAdminCSS)
	mux.HandleFunc("GET /admin/assets/app.js", handleAdminJS)
	mux.HandleFunc("GET /admin/assets/logo.svg", handleAdminLogo)
	mux.HandleFunc("GET /admin/manifest.webmanifest", handleAdminManifest)
	mux.HandleFunc("GET /admin/api/public-dashboard", a.handlePublicDashboard)
	mux.HandleFunc("POST /admin/api/public-dashboard/accounts/", a.handlePublicAccountAction)
	mux.HandleFunc("POST /admin/api/login", a.handleAdminLogin)
	mux.HandleFunc("POST /admin/api/logout", a.requireAdmin(a.handleAdminLogout))
	mux.HandleFunc("GET /admin/api/state", a.requireAdmin(a.handleAdminState))
	mux.HandleFunc("POST /admin/api/settings", a.requireAdmin(a.handleAdminSettings))
	mux.HandleFunc("GET /admin/api/accounts", a.requireAdmin(a.handleAccounts))
	mux.HandleFunc("POST /admin/api/accounts", a.requireAdmin(a.handleAccounts))
	mux.HandleFunc("GET /admin/api/accounts/health", a.requireAdmin(a.handleAccountHealth))
	mux.HandleFunc("POST /admin/api/accounts/quota/refresh-all", a.requireAdmin(a.handleRefreshAllQuota))
	mux.HandleFunc("POST /admin/api/cache/reset", a.requireAdmin(a.handleResetPromptCacheWindow))
	mux.HandleFunc("GET /admin/api/jobs/", a.requireAdmin(a.handleJob))
	mux.HandleFunc("POST /admin/api/jobs/", a.requireAdmin(a.handleJobCancel))
	mux.HandleFunc("GET /admin/api/sticky-sessions", a.requireAdmin(a.handleStickySessions))
	mux.HandleFunc("DELETE /admin/api/sticky-sessions/", a.requireAdmin(a.handleStickySessionDelete))
	mux.HandleFunc("POST /admin/api/accounts/", a.requireAdmin(a.handleAccountAction))
	mux.HandleFunc("DELETE /admin/api/accounts/", a.requireAdmin(a.handleAccountDelete))
	return recovery(mux)
}

func (a *app) handleNotFound(w http.ResponseWriter, r *http.Request) {
	http.NotFound(w, r)
}

func (a *app) handleHealthz(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{"ok": true})
}

func (a *app) handleCurrentStatus(w http.ResponseWriter, r *http.Request) {
	model := a.resolveModel(r.URL.Query().Get("model"))
	stickyKey, scope := currentStatusStickyKey(r, model)
	now := time.Now().UTC()

	a.mu.RLock()
	var session stickySession
	var found bool
	if stickyKey != "" {
		session, found = a.state.StickySessions[stickyKey]
		if found && a.stickySessionExpiredLocked(session, now) {
			found = false
		}
	} else {
		session, found = a.latestStickySessionLocked(model, now)
		scope = "latest"
	}
	if !found {
		a.mu.RUnlock()
		writeOpenAIError(w, http.StatusNotFound, "current_account_not_found", "no current account is bound to the requested model/session")
		return
	}
	item, index := a.accountWithIndexLocked(session.AccountID)
	if item == nil {
		a.mu.RUnlock()
		writeOpenAIError(w, http.StatusNotFound, "current_account_not_found", "current account is no longer configured")
		return
	}
	expiresAt := a.stickyExpiresAt(session)
	accountStatus := a.currentAccountStatusLocked(*item, index, now)
	a.mu.RUnlock()

	response := map[string]any{
		"ok":    true,
		"model": model,
		"scope": map[string]any{
			"type":          scope,
			"lastSuccessAt": session.LastSuccessAt,
			"expiresAt":     expiresAt,
		},
		"account": accountStatus,
	}
	if scope == "latest" {
		response["warning"] = "No session or project was provided; returning the most recent active sticky session for this model."
	}
	writeJSON(w, http.StatusOK, response)
}

type codexReasoningLevel struct {
	Effort      string `json:"effort"`
	Description string `json:"description"`
}

type codexTruncationPolicy struct {
	Mode  string `json:"mode"`
	Limit int    `json:"limit"`
}

type codexModelInfo struct {
	ID                                string                `json:"id"`
	Slug                              string                `json:"slug"`
	DisplayName                       string                `json:"display_name"`
	Description                       string                `json:"description"`
	DefaultReasoningLevel             string                `json:"default_reasoning_level"`
	SupportedReasoningLevels          []codexReasoningLevel `json:"supported_reasoning_levels"`
	ShellType                         string                `json:"shell_type"`
	Visibility                        string                `json:"visibility"`
	SupportedInAPI                    bool                  `json:"supported_in_api"`
	Priority                          int                   `json:"priority"`
	AdditionalSpeedTiers              []string              `json:"additional_speed_tiers"`
	ServiceTiers                      []any                 `json:"service_tiers"`
	DefaultServiceTier                any                   `json:"default_service_tier"`
	AvailabilityNUX                   any                   `json:"availability_nux"`
	Upgrade                           any                   `json:"upgrade"`
	BaseInstructions                  string                `json:"base_instructions"`
	ModelMessages                     any                   `json:"model_messages"`
	SupportsReasoningSummaries        bool                  `json:"supports_reasoning_summaries"`
	SupportsReasoningSummaryParameter bool                  `json:"supports_reasoning_summary_parameter"`
	DefaultReasoningSummary           string                `json:"default_reasoning_summary"`
	SupportVerbosity                  bool                  `json:"support_verbosity"`
	DefaultVerbosity                  string                `json:"default_verbosity"`
	ApplyPatchToolType                string                `json:"apply_patch_tool_type"`
	WebSearchToolType                 string                `json:"web_search_tool_type"`
	TruncationPolicy                  codexTruncationPolicy `json:"truncation_policy"`
	SupportsParallelToolCalls         bool                  `json:"supports_parallel_tool_calls"`
	SupportsImageDetailOriginal       bool                  `json:"supports_image_detail_original"`
	ContextWindow                     int                   `json:"context_window"`
	ContextLength                     int                   `json:"context_length"`
	MaxContextWindow                  int                   `json:"max_context_window"`
	AutoCompactTokenLimit             any                   `json:"auto_compact_token_limit"`
	CompHash                          any                   `json:"comp_hash"`
	EffectiveContextWindowPercent     int                   `json:"effective_context_window_percent"`
	ExperimentalSupportedTools        []string              `json:"experimental_supported_tools"`
	InputModalities                   []string              `json:"input_modalities"`
	IncludeSkillsUsageInstructions    bool                  `json:"include_skills_usage_instructions"`
	SupportsSearchTool                bool                  `json:"supports_search_tool"`
	UseResponsesLite                  bool                  `json:"use_responses_lite"`
	AutoReviewModelOverride           any                   `json:"auto_review_model_override"`
	ToolMode                          any                   `json:"tool_mode"`
	MultiAgentVersion                 any                   `json:"multi_agent_version"`
}

// defaultCodexModelSlugs is the current Codex model lineup (July 2026),
// ordered the way the picker should rank them. These are merged into the
// advertised catalog so a stock Codex client can select any current model
// without its requested model falling off this pool's catalog. A model that
// is missing here still works as a request input, but the client then runs
// on bundled fallback metadata, which prints a startup warning and can attach
// conflicting tools (see dropHostedToolConflicts). Advertising a model is not
// an access grant: per-account allowedModels/excludedModels still gate
// routing, and upstream still enforces plan access (for example
// gpt-5.3-codex-spark is Pro-only).
var defaultCodexModelSlugs = []string{
	"gpt-5.6-sol",
	"gpt-5.6-terra",
	"gpt-5.6-luna",
	"gpt-5.5",
	"gpt-5.4",
	"gpt-5.4-mini",
	"gpt-5.3-codex-spark",
	"gpt-5.2-codex",
}

func codexReasoningLevels() []codexReasoningLevel {
	return []codexReasoningLevel{
		{Effort: "low", Description: "Fast responses with lighter reasoning"},
		{Effort: "medium", Description: "Balances speed and reasoning depth for everyday tasks"},
		{Effort: "high", Description: "Greater reasoning depth for complex problems"},
		{Effort: "xhigh", Description: "Extra high reasoning depth for complex problems"},
	}
}

// codexReasoningLevelsForModel returns the reasoning levels a model may
// advertise. Only the gpt-5.6 family documents `max` and `ultra`; advertising
// them on older models would let the client submit an effort upstream rejects,
// so the extended tiers stay gated to that family.
func codexReasoningLevelsForModel(model string) []codexReasoningLevel {
	levels := codexReasoningLevels()
	if strings.HasPrefix(model, "gpt-5.6") {
		levels = append(levels,
			codexReasoningLevel{Effort: "max", Description: "Maximum reasoning depth for the hardest problems"},
			codexReasoningLevel{Effort: "ultra", Description: "Deepest reasoning for ambiguous, high-value work"},
		)
	}
	return levels
}

func codexCatalogReasoningDefault(tier string, levels []codexReasoningLevel) string {
	for _, level := range levels {
		if level.Effort == tier {
			return tier
		}
	}
	return "medium"
}

// codexCatalogPriority ranks catalog entries: the configured default model
// first, then the built-in lineup in defaultCodexModelSlugs order, then any
// operator-configured extras.
func codexCatalogPriority(model, defaultModel string) int {
	if model == defaultModel {
		return 0
	}
	for index, slug := range defaultCodexModelSlugs {
		if slug == model {
			return index + 1
		}
	}
	return 1000
}

func (a *app) codexModelCatalogLocked(models []string) []codexModelInfo {
	defaultModel, defaultTier := parseModel(a.config.DefaultModel)
	seen := map[string]bool{}
	items := make([]codexModelInfo, 0, len(models))
	for _, model := range models {
		canonical, tier := parseModel(model)
		if tier != "" {
			model = canonical
		}
		if model == "" || seen[model] {
			continue
		}
		seen[model] = true
		levels := codexReasoningLevelsForModel(model)
		reasoningDefault := "medium"
		priority := codexCatalogPriority(model, defaultModel)
		if model == defaultModel {
			reasoningDefault = codexCatalogReasoningDefault(defaultTier, levels)
		}
		items = append(items, codexModelInfo{
			ID:                                model,
			Slug:                              model,
			DisplayName:                       model,
			Description:                       model,
			DefaultReasoningLevel:             reasoningDefault,
			SupportedReasoningLevels:          levels,
			ShellType:                         "shell_command",
			Visibility:                        "list",
			SupportedInAPI:                    true,
			Priority:                          priority,
			AdditionalSpeedTiers:              []string{},
			ServiceTiers:                      []any{},
			DefaultServiceTier:                nil,
			AvailabilityNUX:                   nil,
			Upgrade:                           nil,
			BaseInstructions:                  codexModelBaseInstructions,
			ModelMessages:                     nil,
			SupportsReasoningSummaries:        true,
			SupportsReasoningSummaryParameter: true,
			DefaultReasoningSummary:           "none",
			SupportVerbosity:                  true,
			DefaultVerbosity:                  "low",
			ApplyPatchToolType:                "freeform",
			WebSearchToolType:                 "text_and_image",
			TruncationPolicy:                  codexTruncationPolicy{Mode: "tokens", Limit: 10000},
			SupportsParallelToolCalls:         true,
			SupportsImageDetailOriginal:       true,
			ContextWindow:                     272000,
			ContextLength:                     272000,
			MaxContextWindow:                  272000,
			AutoCompactTokenLimit:             nil,
			CompHash:                          nil,
			EffectiveContextWindowPercent:     95,
			ExperimentalSupportedTools:        []string{},
			InputModalities:                   []string{"text", "image"},
			IncludeSkillsUsageInstructions:    false,
			SupportsSearchTool:                false,
			UseResponsesLite:                  false,
			AutoReviewModelOverride:           nil,
			ToolMode:                          nil,
			MultiAgentVersion:                 nil,
		})
	}
	sort.SliceStable(items, func(left, right int) bool {
		return items[left].Priority < items[right].Priority
	})
	return items
}

func (a *app) handleModels(w http.ResponseWriter, r *http.Request) {
	a.mu.RLock()
	defer a.mu.RUnlock()
	models := a.modelsLocked()
	if r.URL.Query().Get("client_version") != "" {
		// Codex decodes this endpoint with its remote-model schema, not the loose
		// OpenAI model-list shape below. Keep reasoning effort as structured model
		// capability metadata and collapse legacy `(high)`-style aliases to one
		// canonical model. Omitting required capability fields makes the model
		// manager retry during app-server startup and can starve unrelated MCP app
		// initialization until it times out.
		writeJSON(w, http.StatusOK, map[string]any{"models": a.codexModelCatalogLocked(models)})
		return
	}
	items := make([]map[string]any, 0, len(models))
	for _, model := range models {
		items = append(items, map[string]any{"id": model, "object": "model", "created": 0, "owned_by": "codex-pool"})
	}
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": items})
}

func (a *app) handleResponses(w http.ResponseWriter, r *http.Request) {
	a.handleProxy(w, r, false)
}

func (a *app) handleChatCompletions(w http.ResponseWriter, r *http.Request) {
	a.handleProxy(w, r, true)
}

func (a *app) handleProxy(w http.ResponseWriter, r *http.Request, chat bool) {
	body, err := io.ReadAll(io.LimitReader(r.Body, maxRequestBody+1))
	if err != nil || len(body) > maxRequestBody {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "request body is invalid or too large")
		return
	}
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "request body must be JSON")
		return
	}
	requestedModel, _ := payload["model"].(string)
	a.mu.RLock()
	defaultModel := a.config.DefaultModel
	a.mu.RUnlock()
	if requestedModel == "" {
		requestedModel = defaultModel
	}
	model, tier := parseModel(requestedModel)
	a.mu.RLock()
	if alias, ok := a.config.ModelAliases[model]; ok {
		model = alias
	}
	a.mu.RUnlock()
	payload["model"] = model
	if !chat && tier != "" && tier != "none" {
		payload["reasoning"] = map[string]any{"effort": tier}
	}
	dropHostedToolConflicts(payload)
	route := a.routingDecision(r, payload, model, requestAPIKey(r))
	a.applyPromptCacheControls(payload, route)
	updatedBody, err := json.Marshal(payload)
	if err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "unable to encode request")
		return
	}
	excluded := map[string]bool{}
	for attempt := 0; attempt < a.proxyAttemptLimit(); attempt++ {
		candidate, err := a.selectAccountForRoute(route, model, excluded)
		if err != nil {
			if len(excluded) > 0 {
				// At least one upstream was already selected and failed. Reporting
				// this as "no eligible account" makes a transient upstream failure
				// look like pool exhaustion, especially when Pro is the only real
				// fallback after Plus/Team quota is drained.
				writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", "all eligible upstream accounts failed")
				return
			}
			writeOpenAIError(w, http.StatusServiceUnavailable, "all_accounts_cooling_down", err.Error())
			return
		}
		endpoint, outBody, convertResponse, err := a.prepareUpstreamRequest(candidate, updatedBody, chat)
		if err != nil {
			writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", err.Error())
			return
		}
		response, err := a.forward(r.Context(), candidate, endpoint, outBody, r.Header)
		if err != nil {
			if requestContextFinished(r.Context(), err) {
				// The caller timed out or disconnected. Stop immediately and avoid
				// marking the selected account unhealthy; no upstream failure was
				// confirmed, and persisting one makes later unrelated requests slower.
				return
			}
			excluded[candidate.ID] = true
			// A revoked or invalid device-auth credential must not end the user
			// request while other upstream accounts are eligible. Mark it
			// unavailable and continue; selectAccount also suppresses any local
			// duplicate slot for the same upstream identity during this attempt.
			if errors.Is(err, errAccountAuthFailed) {
				a.markAccountAuthFailure(candidate.ID, model, "account_auth_failed")
				continue
			}
			a.markFailure(candidate.ID, model, "upstream_transport_error", 30*time.Second)
			continue
		}
		if upstreamAuthFailureStatus(response.StatusCode) {
			body, _ := io.ReadAll(io.LimitReader(response.Body, maxRequestBody))
			_ = response.Body.Close()
			excluded[candidate.ID] = true
			// 401/403 from an upstream account is credential state, not quota or
			// transient capacity. Keep the error body out of persisted state and
			// route to a different upstream account if one exists.
			reason := codeOr(extractUpstreamErrorCode(body), "account_auth_failed")
			a.markAccountAuthFailure(candidate.ID, model, reason)
			continue
		}
		if response.StatusCode == http.StatusTooManyRequests || response.StatusCode >= http.StatusInternalServerError {
			_ = response.Body.Close()
			retryAfterHeader := response.Header.Get("Retry-After")
			wait := retryAfter(retryAfterHeader)
			reason := "upstream_5xx"
			if response.StatusCode == http.StatusTooManyRequests {
				reason = "rate_limited"
				excluded[candidate.ID] = true
				a.markFailure(candidate.ID, model, reason, wait)
				continue
			} else {
				wait = retryAfterOrDefault(retryAfterHeader, upstream5xxCooldown)
				if strings.TrimSpace(retryAfterHeader) == "" {
					// A 5xx without Retry-After is a transient upstream/server
					// failure, not proof that the account is out of quota. Preserve
					// sticky account locality for KV cache hit rate: do not fail over
					// or cool down the selected account until repeated failures show
					// it is genuinely unhealthy. This keeps a single blip from moving
					// a hot route to a cold account.
					consecutive := a.markFailure(candidate.ID, model, reason, 0)
					if consecutive < upstream5xxFailoverAfter {
						writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", "selected upstream account failed transiently")
						return
					}
					excluded[candidate.ID] = true
					if !a.hasPreferredAccount(model, excluded) {
						writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", "selected upstream account failed repeatedly")
						return
					}
					a.markCooldown(candidate.ID, model, reason, wait)
					continue
				}
			}
			excluded[candidate.ID] = true
			a.markFailure(candidate.ID, model, reason, wait)
			continue
		}
		defer response.Body.Close()
		a.addCurrentAccountResponseHeaders(w, candidate.ID)
		var info proxyResponseInfo
		ok := true
		if convertResponse {
			info, ok = a.writeChatFromResponse(w, response, model)
		} else {
			info, ok = copyProxyResponse(w, response)
		}
		if !ok {
			a.markFailure(candidate.ID, model, "upstream_response_error", 30*time.Second)
			return
		}
		a.markSuccess(route, model, candidate.ID, info)
		return
	}
	writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", "all eligible upstream accounts failed")
}

func requestContextFinished(ctx context.Context, err error) bool {
	return ctx.Err() != nil || errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded)
}

func (a *app) prepareUpstreamRequest(candidate account, body []byte, chat bool) (string, []byte, bool, error) {
	base := strings.TrimRight(candidate.UpstreamBaseURL, "/")
	if a.usesCliproxySidecar(candidate) {
		base = a.cliproxyBaseURL
	} else if base == "" && isCodexDeviceAuth(candidate) {
		base = a.codexBaseURL
	}
	if base == "" {
		return "", nil, false, errors.New("selected account has no upstreamBaseUrl")
	}
	var endpoint string
	var outbound []byte
	convertResponse := false
	if !chat || normalWireAPI(candidate.WireAPI) == "chat_completions" {
		path := "/responses"
		if chat {
			path = "/chat/completions"
		}
		endpoint = base + path
		outbound = body
	} else {
		var chatRequest map[string]any
		if err := json.Unmarshal(body, &chatRequest); err != nil {
			return "", nil, false, err
		}
		input := make([]map[string]any, 0)
		if messages, ok := chatRequest["messages"].([]any); ok {
			for _, raw := range messages {
				message, _ := raw.(map[string]any)
				input = append(input, map[string]any{"role": message["role"], "content": message["content"]})
			}
		}
		responsesRequest := map[string]any{"model": chatRequest["model"], "input": input}
		if stream, _ := chatRequest["stream"].(bool); stream {
			responsesRequest["stream"] = true
		}
		for _, name := range []string{"prompt_cache_key", "prompt_cache_retention"} {
			if value, ok := chatRequest[name]; ok {
				responsesRequest[name] = value
			}
		}
		converted, err := json.Marshal(responsesRequest)
		if err != nil {
			return "", nil, false, err
		}
		endpoint = base + "/responses"
		outbound = converted
		convertResponse = true
	}
	if a.usesCliproxySidecar(candidate) {
		var err error
		outbound, err = withCliproxyAccountModel(outbound, candidate.ID)
		if err != nil {
			return "", nil, false, err
		}
	}
	return endpoint, outbound, convertResponse, nil
}

var codexMetadataHeaderAllowlist = []string{
	"X-Codex-Parent-Thread-ID",
	"X-OpenAI-Subagent",
	"X-Codex-Turn-Metadata",
	"X-Codex-Window-ID",
	"X-Codex-Installation-ID",
}

func (a *app) forward(ctx context.Context, candidate account, endpoint string, body []byte, inbound http.Header) (*http.Response, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, strings.NewReader(string(body)))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if accept := inbound.Get("Accept"); accept != "" {
		req.Header.Set("Accept", accept)
	}
	forwardCodexMetadataHeaders(req.Header, inbound)
	if a.usesCliproxySidecar(candidate) {
		if err := a.syncCliproxyAuth(candidate, false); err != nil {
			return nil, err
		}
		if a.cliproxyAPIKey == "" {
			return nil, errors.New("cliproxy sidecar API key is unavailable")
		}
		req.Header.Set("Authorization", "Bearer "+a.cliproxyAPIKey)
	} else if candidate.UpstreamAPIKey != "" {
		req.Header.Set("Authorization", "Bearer "+candidate.UpstreamAPIKey)
	} else if isCodexDeviceAuth(candidate) {
		auth, err := a.refreshedCodexAuthContext(ctx, candidate)
		if err != nil {
			return nil, err
		}
		if auth.AccountID == "" {
			auth.AccountID = candidate.AccountID
		}
		req.Header.Set("Authorization", "Bearer "+auth.AccessToken)
		if auth.AccountID != "" {
			req.Header.Set("ChatGPT-Account-ID", auth.AccountID)
		}
		if auth.FedRAMP {
			req.Header.Set("X-OpenAI-Fedramp", "true")
		}
	}
	return a.streamClient.Do(req)
}

func forwardCodexMetadataHeaders(outbound, inbound http.Header) {
	for _, name := range codexMetadataHeaderAllowlist {
		if value := strings.TrimSpace(inbound.Get(name)); value != "" {
			outbound.Set(name, value)
		}
	}
}

func upstreamAuthFailureStatus(status int) bool {
	return status == http.StatusUnauthorized || status == http.StatusForbidden
}

func oauthRefreshAuthFailureStatus(status int) bool {
	return status == http.StatusBadRequest || upstreamAuthFailureStatus(status)
}

func markAccountAuthError(err error) error {
	// This sentinel separates credential failures from transport failures. The
	// proxy loop uses it to quarantine the account and try another eligible
	// account instead of turning one revoked token into a request outage.
	if err == nil {
		return errAccountAuthFailed
	}
	if errors.Is(err, errAccountAuthFailed) {
		return err
	}
	return fmt.Errorf("%w: %w", errAccountAuthFailed, err)
}

func (a *app) usesCliproxySidecar(item account) bool {
	return isCodexDeviceAuth(item) && a.codexGatewayMode != "direct"
}

func cliproxyAccountPrefix(accountID string) string {
	return "codex-pool-" + accountID
}

func withCliproxyAccountModel(body []byte, accountID string) ([]byte, error) {
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		return nil, err
	}
	model, _ := payload["model"].(string)
	if strings.TrimSpace(model) == "" {
		return nil, errors.New("request model is required for cliproxy sidecar")
	}
	payload["model"] = cliproxyAccountPrefix(accountID) + "/" + model
	return json.Marshal(payload)
}

type promptCacheUsage struct {
	InputTokens  uint64
	CachedTokens uint64
	Present      bool
}

type proxyResponseInfo struct {
	ResponseID string
	Usage      promptCacheUsage
}

func (a *app) writeChatFromResponse(w http.ResponseWriter, response *http.Response, model string) (proxyResponseInfo, bool) {
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return copyProxyResponse(w, response)
	}
	var data map[string]any
	if err := json.NewDecoder(response.Body).Decode(&data); err != nil {
		writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", "invalid Responses API payload from upstream")
		return proxyResponseInfo{}, false
	}
	text := outputText(data)
	created := time.Now().Unix()
	writeJSON(w, http.StatusOK, map[string]any{
		"id": "chatcmpl_" + randomID(), "object": "chat.completion", "created": created, "model": model,
		"choices": []map[string]any{{"index": 0, "message": map[string]any{"role": "assistant", "content": text}, "finish_reason": "stop"}},
	})
	return responseInfoFromPayload(data), true
}

func outputText(data map[string]any) string {
	output, _ := data["output"].([]any)
	for _, raw := range output {
		item, _ := raw.(map[string]any)
		content, _ := item["content"].([]any)
		for _, c := range content {
			part, _ := c.(map[string]any)
			if text, ok := part["text"].(string); ok {
				return text
			}
		}
	}
	return ""
}

func (a *app) addCurrentAccountResponseHeaders(w http.ResponseWriter, accountID string) {
	a.mu.RLock()
	item, index := a.accountWithIndexLocked(accountID)
	if item == nil {
		a.mu.RUnlock()
		return
	}
	quota := a.state.Quotas[item.ID]
	displayItem := *item
	if quota.OrganizationName != "" {
		displayItem.OrganizationName = quota.OrganizationName
	}
	if quota.PlanType != "" {
		displayItem.PlanType = quota.PlanType
	}
	if quota.PlanLimit != "" {
		displayItem.PlanLimit = quota.PlanLimit
	}
	status, _ := a.accountStatusLocked(*item, time.Now().UTC())
	displayName := currentAccountDisplayName(displayItem, index)
	organizationName := publicOrganizationName(effectiveOrganizationName(displayItem))
	planType := normalizePlanType(displayItem.PlanType)
	planDisplay := accountPlanDisplayName(displayItem, false)
	quotaValue := quota.Quota
	updatedAt := quota.UsageUpdatedAt
	a.mu.RUnlock()

	if displayName != "" {
		w.Header().Set("X-Codex-Pool-Account", safeHeaderValue(displayName))
	}
	if planType != "" && planType != "unknown" {
		w.Header().Set("X-Codex-Pool-Plan", safeHeaderValue(planDisplay))
	}
	if organizationName != "" {
		w.Header().Set("X-Codex-Pool-Organization", safeHeaderValue(organizationName))
	}
	if status != "" {
		w.Header().Set("X-Codex-Pool-Account-Status", safeHeaderValue(status))
	}
	if quotaValue != nil {
		w.Header().Set("X-Codex-Pool-Quota-Remaining", strconv.Itoa(remainingQuotaHint(*quotaValue)))
		if quotaValue.Hourly.Present {
			w.Header().Set("X-Codex-Pool-Quota-Hourly-Remaining", strconv.Itoa(quotaValue.Hourly.Percentage))
		}
		if quotaValue.Weekly.Present {
			w.Header().Set("X-Codex-Pool-Quota-Weekly-Remaining", strconv.Itoa(quotaValue.Weekly.Percentage))
		}
	}
	if !updatedAt.IsZero() {
		w.Header().Set("X-Codex-Pool-Quota-Updated-At", updatedAt.Format(time.RFC3339))
	}
}

func safeHeaderValue(value string) string {
	return strings.Map(func(r rune) rune {
		if r == '\r' || r == '\n' || r == 0 {
			return -1
		}
		if r < 32 && r != '\t' {
			return -1
		}
		return r
	}, strings.TrimSpace(value))
}

func copyProxyResponse(w http.ResponseWriter, response *http.Response) (proxyResponseInfo, bool) {
	for _, header := range []string{"Content-Type", "Cache-Control", "X-Request-Id"} {
		if value := response.Header.Get(header); value != "" {
			w.Header().Set(header, value)
		}
	}
	w.WriteHeader(response.StatusCode)
	if strings.Contains(strings.ToLower(response.Header.Get("Content-Type")), "text/event-stream") {
		return copyStreamingProxyResponse(w, response.Body), true
	}
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return proxyResponseInfo{}, false
	}
	var info proxyResponseInfo
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err == nil {
		info = responseInfoFromPayload(payload)
	}
	_, _ = w.Write(body)
	return info, true
}

func copyStreamingProxyResponse(w http.ResponseWriter, body io.Reader) proxyResponseInfo {
	var info proxyResponseInfo
	reader := bufio.NewReader(body)
	flusher, _ := w.(http.Flusher)
	for {
		line, err := reader.ReadString('\n')
		if line != "" {
			_, _ = io.WriteString(w, line)
			info.merge(responseInfoFromSSELine(line))
			if flusher != nil {
				flusher.Flush()
			}
		}
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			break
		}
	}
	return info
}

func (info *proxyResponseInfo) merge(next proxyResponseInfo) {
	if next.ResponseID != "" {
		info.ResponseID = next.ResponseID
	}
	if next.Usage.Present {
		info.Usage = next.Usage
	}
}

func responseInfoFromSSELine(line string) proxyResponseInfo {
	line = strings.TrimSpace(line)
	if !strings.HasPrefix(line, "data:") {
		return proxyResponseInfo{}
	}
	data := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
	if data == "" || data == "[DONE]" {
		return proxyResponseInfo{}
	}
	var payload map[string]any
	if err := json.Unmarshal([]byte(data), &payload); err != nil {
		return proxyResponseInfo{}
	}
	if response, ok := payload["response"].(map[string]any); ok {
		return responseInfoFromPayload(response)
	}
	return responseInfoFromPayload(payload)
}

func responseInfoFromPayload(payload map[string]any) proxyResponseInfo {
	if payload == nil {
		return proxyResponseInfo{}
	}
	id, _ := payload["id"].(string)
	return proxyResponseInfo{ResponseID: id, Usage: promptCacheUsageFromPayload(payload)}
}

func promptCacheUsageFromPayload(payload map[string]any) promptCacheUsage {
	usage, _ := payload["usage"].(map[string]any)
	if usage == nil {
		return promptCacheUsage{}
	}
	inputTokens, inputOK := uint64Field(usage, "input_tokens")
	if !inputOK {
		inputTokens, inputOK = uint64Field(usage, "prompt_tokens")
	}
	var cachedTokens uint64
	var cachedOK bool
	for _, name := range []string{"input_tokens_details", "prompt_tokens_details"} {
		details, _ := usage[name].(map[string]any)
		if details == nil {
			continue
		}
		cachedTokens, cachedOK = uint64Field(details, "cached_tokens")
		if cachedOK {
			break
		}
	}
	return promptCacheUsage{InputTokens: inputTokens, CachedTokens: cachedTokens, Present: inputOK || cachedOK}
}

func uint64Field(values map[string]any, name string) (uint64, bool) {
	value, ok := values[name]
	if !ok {
		return 0, false
	}
	switch typed := value.(type) {
	case json.Number:
		number, err := typed.Int64()
		if err != nil || number < 0 {
			return 0, false
		}
		return uint64(number), true
	case float64:
		if typed < 0 {
			return 0, false
		}
		return uint64(typed), true
	case int:
		if typed < 0 {
			return 0, false
		}
		return uint64(typed), true
	case int64:
		if typed < 0 {
			return 0, false
		}
		return uint64(typed), true
	case uint64:
		return typed, true
	case uint:
		return uint64(typed), true
	default:
		return 0, false
	}
}

func (a *app) handleAdminPage(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Content-Security-Policy", "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'")
	w.Header().Set("X-Codex-Pool-Version", adminDisplayVersion())
	_, _ = io.WriteString(w, adminPage())
}

func (a *app) handleAdminLogin(w http.ResponseWriter, r *http.Request) {
	key := adminLoginKey(r)
	if retryAt, locked := a.adminLoginLockedOut(key); locked {
		w.Header().Set("Retry-After", strconv.Itoa(int(time.Until(retryAt).Seconds())))
		writeOpenAIError(w, http.StatusTooManyRequests, "admin_rate_limited", "too many failed login attempts")
		return
	}
	var request struct {
		Username string `json:"username"`
		Password string `json:"password"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&request); err != nil {
		a.recordAdminLoginFailure(key)
		writeOpenAIError(w, http.StatusUnauthorized, "invalid_admin_credentials", "invalid username or password")
		return
	}
	username := strings.TrimSpace(request.Username)
	if username == "" {
		username = a.adminUser
	}
	if username != a.adminUser || !verifyPasswordHash(string(a.adminHash), request.Password) {
		a.recordAdminLoginFailure(key)
		writeOpenAIError(w, http.StatusUnauthorized, "invalid_admin_credentials", "invalid username or password")
		return
	}
	a.clearAdminLoginFailures(key)
	expires := time.Now().UTC().Add(sessionLifetime)
	token := a.signSession(a.adminUser, expires)
	csrf := randomID()
	secureCookie := a.cookieSecure(r)
	http.SetCookie(w, &http.Cookie{Name: "codex_pool_session", Value: token, Path: "/admin", HttpOnly: true, SameSite: http.SameSiteStrictMode, Secure: secureCookie, Expires: expires})
	http.SetCookie(w, &http.Cookie{Name: "codex_pool_csrf", Value: csrf, Path: "/admin", HttpOnly: false, SameSite: http.SameSiteStrictMode, Secure: secureCookie, Expires: expires})
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "csrfToken": csrf})
}

func (a *app) handleAdminLogout(w http.ResponseWriter, r *http.Request) {
	secureCookie := a.cookieSecure(r)
	http.SetCookie(w, &http.Cookie{Name: "codex_pool_session", Value: "", Path: "/admin", MaxAge: -1, HttpOnly: true, Secure: secureCookie})
	http.SetCookie(w, &http.Cookie{Name: "codex_pool_csrf", Value: "", Path: "/admin", MaxAge: -1, Secure: secureCookie})
	writeJSON(w, http.StatusOK, map[string]any{"ok": true})
}

func (a *app) handleAdminState(w http.ResponseWriter, _ *http.Request) {
	a.mu.RLock()
	defer a.mu.RUnlock()
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "state": a.adminStateLocked(time.Now().UTC())})
}

func (a *app) handleAdminSettings(w http.ResponseWriter, r *http.Request) {
	var request struct {
		PreserveProQuota *bool `json:"preserveProQuota"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&request); err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid settings JSON")
		return
	}
	if request.PreserveProQuota == nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "preserveProQuota is required")
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	a.preserveProQuota = *request.PreserveProQuota
	value := a.preserveProQuota
	a.config.PreserveProQuota = &value
	if err := a.saveLocked(); err != nil {
		writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist settings")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "state": a.adminStateLocked(time.Now().UTC())})
}

func (a *app) adminStateLocked(now time.Time) map[string]any {
	return map[string]any{"running": true, "routingStrategy": a.effectiveRoutingStrategy(), "defaultModel": a.config.DefaultModel, "preserveProQuota": a.preserveProQuota, "promptCacheKeyMode": envOrValue(a.promptCacheKeyMode, "auto"), "promptCacheKeyScope": envOrValue(a.promptCacheKeyScope, "auto"), "promptCacheKeyPolicy": envOrValue(a.promptCacheKeyPolicy, "preserve"), "promptCacheBuckets": a.promptCacheBuckets, "promptCacheRetention": a.promptCacheRetention, "promptCache": a.state.PromptCache, "promptCacheWindow": a.promptCacheWindowLocked(), "threadBindings": a.state.ThreadBindings, "accounts": publicAccounts(a.config.Accounts), "requestCount": a.state.RequestCount, "successCount": a.state.SuccessCount, "failureCount": a.state.FailureCount, "summary": a.dashboardSummaryLocked(now)}
}

func (a *app) handlePublicDashboard(w http.ResponseWriter, _ *http.Request) {
	if !a.publicDashboard {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "not found")
		return
	}
	// This endpoint is intentionally unauthenticated: it powers the public
	// control page on the admin port. Keep it redacted and limited to pool state;
	// owner-only state stays behind requireAdmin routes.
	a.mu.RLock()
	defer a.mu.RUnlock()
	now := time.Now().UTC()
	accounts := make([]map[string]any, 0, len(a.config.Accounts))
	for index, item := range a.config.Accounts {
		accounts = append(accounts, a.publicDashboardAccountLocked(item, index, now))
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "dashboard": map[string]any{"updatedAt": a.state.UpdatedAt, "summary": a.publicDashboardSummaryLocked(now), "accounts": accounts, "promptCacheWindow": a.promptCacheWindowLocked()}})
}

func (a *app) handlePublicAccountAction(w http.ResponseWriter, r *http.Request) {
	if !a.publicDashboard {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "not found")
		return
	}
	// Public users may only join/leave the pool through an opaque per-process
	// reference. Do not add account IDs, delete/login actions, or unmasked
	// metadata to this surface; those belong to authenticated management APIs.
	parts := strings.Split(strings.TrimPrefix(r.URL.Path, "/admin/api/public-dashboard/accounts/"), "/")
	if len(parts) != 2 || parts[0] == "" {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "public account action not found")
		return
	}
	ref, action := parts[0], parts[1]
	if action != "pool-add" && action != "pool-remove" {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "public account action not found")
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	for index := range a.config.Accounts {
		item := &a.config.Accounts[index]
		if !a.publicAccountRefMatchesLocked(item.ID, ref) {
			continue
		}
		switch action {
		case "pool-add":
			item.Enabled = true
			item.InPool = true
		case "pool-remove":
			item.InPool = false
			a.clearStickyForAccountLocked(item.ID)
		}
		item.UpdatedAt = now
		if err := a.saveLocked(); err != nil {
			writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist account")
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "account": a.publicDashboardAccountLocked(*item, index, now)})
		return
	}
	writeOpenAIError(w, http.StatusNotFound, "not_found", "account not found")
}

func (a *app) handleAccounts(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		a.mu.RLock()
		defer a.mu.RUnlock()
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "accounts": publicAccounts(a.config.Accounts)})
		return
	}
	var input account
	if err := json.NewDecoder(io.LimitReader(r.Body, maxRequestBody)).Decode(&input); err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid account JSON")
		return
	}
	input.ID = strings.TrimSpace(input.ID)
	input.Email = normalizeEmail(input.Email)
	input.OrganizationName = cleanOrganizationName(input.OrganizationName)
	input.Label = strings.TrimSpace(input.Label)
	input.UpstreamBaseURL = strings.TrimRight(input.UpstreamBaseURL, "/")
	input.WireAPI = normalWireAPI(input.WireAPI)
	if input.AuthType == "" {
		if input.UpstreamBaseURL != "" || input.UpstreamAPIKey != "" {
			input.AuthType = "provider_api_key"
		} else {
			input.AuthType = "codex_device_auth"
		}
	}
	if isCodexDeviceAuth(input) {
		input.Email = ""
		input.AccountID = ""
		input.OrganizationName = ""
		input.PlanType = ""
		input.PlanLimit = ""
		input.PlanRank = 0
	} else {
		input.PlanType = normalizePlanType(input.PlanType)
		input.PlanLimit = cleanPlanLimit(input.PlanLimit)
		input.PlanRank = planRank(input.PlanType)
	}
	generateID := input.ID == ""
	if !generateID && !validAccountID(input.ID) {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "id must contain only letters, numbers, underscores, or dashes")
		return
	}
	now := time.Now().UTC()
	input.CreatedAt = now
	input.UpdatedAt = now
	a.mu.Lock()
	defer a.mu.Unlock()
	if generateID {
		input.ID = a.uniqueAccountIDLocked(generatedAccountIDBase(input))
	}
	if input.Label == "" {
		input.Label = accountDisplayName(input)
	}
	if isCodexDeviceAuth(input) {
		input.UpstreamBaseURL = ""
		input.UpstreamAPIKey = ""
		input.AllowedModels = nil
		input.ExcludedModels = nil
		input.CodexHome = a.accountCodexHome(input.ID)
		if input.Enabled || input.InPool {
			// A device-auth slot is not routable until Codex CLI has written
			// auth.json and the sidecar/quota state has been prepared. Stage new
			// slots even when callers request immediate pool membership; otherwise
			// empty auth directories can stall dashboard and routing lock paths.
			input.PendingPoolActivation = input.Enabled && input.InPool
			input.Enabled = false
			input.InPool = false
		}
	} else if strings.TrimSpace(input.UpstreamBaseURL) == "" {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "upstreamBaseUrl is required for provider API key accounts")
		return
	}
	if a.accountLocked(input.ID) != nil {
		writeOpenAIError(w, http.StatusConflict, "account_exists", "account id already exists")
		return
	}
	a.config.Accounts = append(a.config.Accounts, input)
	if err := a.saveLocked(); err != nil {
		writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist account")
		return
	}
	writeJSON(w, http.StatusCreated, map[string]any{"ok": true, "account": publicAccount(input, len(a.config.Accounts)-1)})
}

func (a *app) handleAccountHealth(w http.ResponseWriter, _ *http.Request) {
	a.mu.RLock()
	defer a.mu.RUnlock()
	items := make([]map[string]any, 0, len(a.config.Accounts))
	now := time.Now().UTC()
	for _, account := range a.config.Accounts {
		items = append(items, a.accountHealthItemLocked(account, now))
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "accounts": items})
}

func (a *app) handleRefreshAllQuota(w http.ResponseWriter, r *http.Request) {
	a.mu.RLock()
	accountIDs := make([]string, 0, len(a.config.Accounts))
	for _, account := range a.config.Accounts {
		if isCodexDeviceAuth(account) {
			accountIDs = append(accountIDs, account.ID)
		}
	}
	a.mu.RUnlock()
	results := make([]map[string]any, 0, len(accountIDs))
	for _, accountID := range accountIDs {
		snapshot, err := a.refreshAccountQuota(r.Context(), accountID)
		if err != nil {
			results = append(results, map[string]any{"accountId": accountID, "ok": false, "error": map[string]any{"code": "quota_refresh_failed", "message": err.Error()}})
			continue
		}
		results = append(results, map[string]any{"accountId": accountID, "ok": true, "quota": snapshot.Quota, "organizationName": publicOrganizationName(snapshot.OrganizationName), "planType": snapshot.PlanType, "planLimit": snapshot.PlanLimit, "usageUpdatedAt": snapshot.UsageUpdatedAt})
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "results": results})
}

func (a *app) handleJob(w http.ResponseWriter, r *http.Request) {
	id := strings.TrimPrefix(r.URL.Path, "/admin/api/jobs/")
	a.mu.RLock()
	defer a.mu.RUnlock()
	job := a.jobs[id]
	if job == nil {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "job not found")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "job": *job})
}

func (a *app) handleJobCancel(w http.ResponseWriter, r *http.Request) {
	path := strings.TrimPrefix(r.URL.Path, "/admin/api/jobs/")
	id := strings.TrimSuffix(path, "/cancel")
	id = strings.TrimSuffix(id, "/")
	if id == "" || id == path {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "job action not found")
		return
	}
	cancel, job, err := a.cancelLoginJob(id)
	if err != nil {
		writeOpenAIError(w, http.StatusNotFound, "not_found", err.Error())
		return
	}
	if cancel != nil {
		cancel()
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "job": job})
}

func (a *app) handleResetPromptCacheWindow(w http.ResponseWriter, _ *http.Request) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.resetPromptCacheWindowLocked(time.Now().UTC())
	if err := a.saveLocked(); err != nil {
		writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist cache window reset")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "promptCacheWindow": a.promptCacheWindowLocked()})
}

func (a *app) handleStickySessions(w http.ResponseWriter, _ *http.Request) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	pruned := a.pruneExpiredRuntimeStateLocked(now)
	items := make([]stickySession, 0, len(a.state.StickySessions))
	for _, item := range a.state.StickySessions {
		item.ExpiresAt = a.stickyExpiresAt(item)
		items = append(items, item)
	}
	sort.Slice(items, func(i, j int) bool { return items[i].Key < items[j].Key })
	if pruned {
		_ = a.saveLocked()
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "sessions": items})
}

func (a *app) handleStickySessionDelete(w http.ResponseWriter, r *http.Request) {
	key := strings.TrimPrefix(r.URL.Path, "/admin/api/sticky-sessions/")
	if key == "" {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "session key is required")
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	delete(a.state.StickySessions, key)
	for bindingKey, binding := range a.state.ThreadBindings {
		if binding.StickyKey == key {
			delete(a.state.ThreadBindings, bindingKey)
		}
	}
	if err := a.saveLocked(); err != nil {
		writeOpenAIError(w, 500, "storage_error", "unable to persist state")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true})
}

func (a *app) handleAccountAction(w http.ResponseWriter, r *http.Request) {
	parts := strings.Split(strings.TrimPrefix(r.URL.Path, "/admin/api/accounts/"), "/")
	if len(parts) < 2 {
		writeOpenAIError(w, 404, "not_found", "account action not found")
		return
	}
	id, action := parts[0], strings.Join(parts[1:], "/")
	a.mu.Lock()
	item, index := a.accountWithIndexLocked(id)
	if item == nil {
		a.mu.Unlock()
		writeOpenAIError(w, 404, "not_found", "account not found")
		return
	}
	if action == "quota/refresh" {
		accountID := item.ID
		a.mu.Unlock()
		snapshot, err := a.refreshAccountQuota(r.Context(), accountID)
		if err != nil {
			writeOpenAIError(w, http.StatusBadGateway, "quota_refresh_failed", err.Error())
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "accountId": id, "quota": snapshot.Quota, "organizationName": publicOrganizationName(snapshot.OrganizationName), "planType": snapshot.PlanType, "planLimit": snapshot.PlanLimit, "usageUpdatedAt": snapshot.UsageUpdatedAt})
		return
	}
	defer a.mu.Unlock()
	switch action {
	case "login":
		if !isCodexDeviceAuth(*item) {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "device auth login is only available for Codex accounts")
			return
		}
		job := a.startLoginJobLocked(*item)
		writeJSON(w, http.StatusAccepted, map[string]any{"ok": true, "job": job})
		return
	case "enable":
		item.Enabled = true
	case "disable":
		item.Enabled = false
		item.InPool = false
		a.clearStickyForAccountLocked(id)
	case "pool-add":
		item.InPool = true
	case "pool-remove":
		item.InPool = false
		a.clearStickyForAccountLocked(id)
	case "cooldowns/clear":
		delete(a.state.Cooldowns, id)
	case "cache/reset":
		a.resetPromptCacheWindowForAccountLocked(id, time.Now().UTC())
		if err := a.saveLocked(); err != nil {
			writeOpenAIError(w, 500, "storage_error", "unable to persist cache window reset")
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "accountId": id, "promptCacheWindow": a.promptCacheWindowForAccountLocked(id)})
		return
	case "quota/set":
		var request struct {
			RemainingQuota *int `json:"remainingQuota"`
		}
		if err := json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&request); err != nil || request.RemainingQuota == nil || *request.RemainingQuota < 0 || *request.RemainingQuota > 100 {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "remainingQuota must be an integer from 0 to 100")
			return
		}
		item.RemainingQuota = request.RemainingQuota
	default:
		writeOpenAIError(w, 404, "not_found", "account action not found")
		return
	}
	item.UpdatedAt = time.Now().UTC()
	if err := a.saveLocked(); err != nil {
		writeOpenAIError(w, 500, "storage_error", "unable to persist account")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "account": publicAccount(*item, index)})
}

func (a *app) handleAccountDelete(w http.ResponseWriter, r *http.Request) {
	id := strings.TrimPrefix(r.URL.Path, "/admin/api/accounts/")
	a.mu.Lock()
	defer a.mu.Unlock()
	for i, account := range a.config.Accounts {
		if account.ID == id {
			a.cancelLoginJobsForAccountLocked(id)
			if isCodexDeviceAuth(account) {
				if err := os.RemoveAll(a.accountRoot(id)); err != nil {
					writeOpenAIError(w, 500, "storage_error", "unable to purge account credentials")
					return
				}
				if err := os.Remove(a.cliproxyAuthPath(id)); err != nil && !errors.Is(err, os.ErrNotExist) {
					writeOpenAIError(w, 500, "storage_error", "unable to purge account gateway credentials")
					return
				}
			}
			a.config.Accounts = append(a.config.Accounts[:i], a.config.Accounts[i+1:]...)
			delete(a.state.Cooldowns, id)
			delete(a.state.Health, id)
			delete(a.state.Quotas, id)
			deletePromptCacheForAccount(a.state.PromptCache, id)
			deletePromptCacheForAccount(a.state.PromptCacheBaseline, id)
			delete(a.state.PromptCacheResetAtByAccount, id)
			a.clearStickyForAccountLocked(id)
			if err := a.saveLocked(); err != nil {
				writeOpenAIError(w, 500, "storage_error", "unable to persist account")
				return
			}
			writeJSON(w, http.StatusOK, map[string]any{"ok": true})
			return
		}
	}
	writeOpenAIError(w, 404, "not_found", "account not found")
}

func (a *app) requireAPIKey(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if !a.validAPIKey(requestAPIKey(r)) {
			writeOpenAIError(w, http.StatusUnauthorized, "invalid_api_key", "invalid or missing API key")
			return
		}
		next(w, r)
	}
}

func (a *app) requireAdmin(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		cookie, err := r.Cookie("codex_pool_session")
		if err != nil || !a.validSession(cookie.Value) {
			writeOpenAIError(w, http.StatusUnauthorized, "admin_unauthorized", "admin login required")
			return
		}
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			csrfCookie, err := r.Cookie("codex_pool_csrf")
			if err != nil || csrfCookie.Value == "" || subtle.ConstantTimeCompare([]byte(csrfCookie.Value), []byte(r.Header.Get("X-CSRF-Token"))) != 1 {
				writeOpenAIError(w, http.StatusForbidden, "csrf_invalid", "valid CSRF token required")
				return
			}
		}
		next(w, r)
	}
}

func (a *app) cookieSecure(r *http.Request) bool {
	if os.Getenv("CODEX_POOL_COOKIE_SECURE") == "true" {
		return true
	}
	if r.TLS != nil {
		return true
	}
	return strings.EqualFold(r.Header.Get("X-Forwarded-Proto"), "https")
}

func (a *app) adminLoginLockedOut(key string) (time.Time, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.loginFailures == nil {
		a.loginFailures = map[string]loginFailure{}
	}
	failure := a.loginFailures[key]
	if failure.LockedOutAt.IsZero() {
		return time.Time{}, false
	}
	until := failure.LockedOutAt.Add(adminLoginLockout)
	if time.Now().UTC().Before(until) {
		return until, true
	}
	delete(a.loginFailures, key)
	return time.Time{}, false
}

func (a *app) recordAdminLoginFailure(key string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.loginFailures == nil {
		a.loginFailures = map[string]loginFailure{}
	}
	now := time.Now().UTC()
	failure := a.loginFailures[key]
	if !failure.LastFailure.IsZero() && now.Sub(failure.LastFailure) > adminLoginLockout {
		failure = loginFailure{}
	}
	failure.Count++
	failure.LastFailure = now
	if failure.Count >= adminLoginMaxFailures && failure.LockedOutAt.IsZero() {
		failure.LockedOutAt = now
	}
	a.loginFailures[key] = failure
}

func (a *app) clearAdminLoginFailures(key string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	delete(a.loginFailures, key)
}

func adminLoginKey(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err == nil && host != "" {
		return host
	}
	if r.RemoteAddr != "" {
		return r.RemoteAddr
	}
	return "unknown"
}

func (a *app) validAPIKey(value string) bool {
	if value == "" {
		return false
	}
	for _, key := range a.apiKeys {
		if subtle.ConstantTimeCompare([]byte(value), key) == 1 {
			return true
		}
	}
	return false
}

func requestAPIKey(r *http.Request) string {
	for _, value := range []string{r.Header.Get("Authorization"), r.Header.Get("X-Goog-Api-Key"), r.Header.Get("X-Api-Key"), r.URL.Query().Get("key"), r.URL.Query().Get("auth_token")} {
		if strings.HasPrefix(value, "Bearer ") {
			value = strings.TrimSpace(strings.TrimPrefix(value, "Bearer "))
		}
		if value != "" {
			return value
		}
	}
	return ""
}

func (a *app) signSession(user string, expires time.Time) string {
	payload := user + "|" + strconv.FormatInt(expires.Unix(), 10)
	mac := hmac.New(sha256.New, a.sessionKey)
	_, _ = mac.Write([]byte(payload))
	return base64.RawURLEncoding.EncodeToString([]byte(payload + "|" + base64.RawURLEncoding.EncodeToString(mac.Sum(nil))))
}

func (a *app) validSession(token string) bool {
	decoded, err := base64.RawURLEncoding.DecodeString(token)
	if err != nil {
		return false
	}
	parts := strings.Split(string(decoded), "|")
	if len(parts) != 3 || parts[0] != a.adminUser {
		return false
	}
	expiry, err := strconv.ParseInt(parts[1], 10, 64)
	if err != nil || time.Now().After(time.Unix(expiry, 0)) {
		return false
	}
	expected := a.signSession(parts[0], time.Unix(expiry, 0))
	return subtle.ConstantTimeCompare([]byte(token), []byte(expected)) == 1
}

func (a *app) modelsLocked() []string {
	set := map[string]bool{a.config.DefaultModel: true}
	if base, _ := parseModel(a.config.DefaultModel); base != "" {
		set[base] = true
		for _, level := range codexReasoningLevelsForModel(base) {
			set[fmt.Sprintf("%s(%s)", base, level.Effort)] = true
		}
	}
	for _, slug := range defaultCodexModelSlugs {
		set[slug] = true
	}
	for _, account := range a.config.Accounts {
		for _, model := range account.AllowedModels {
			set[model] = true
		}
	}
	for alias := range a.config.ModelAliases {
		set[alias] = true
	}
	models := make([]string, 0, len(set))
	for model := range set {
		if model != "" {
			models = append(models, model)
		}
	}
	sort.Strings(models)
	return models
}

func (a *app) selectAccount(stickyKey, model string, excluded map[string]bool) (account, error) {
	return a.selectAccountWithPreference(stickyKey, model, "", excluded)
}

func (a *app) selectAccountForRoute(route routingDecision, model string, excluded map[string]bool) (account, error) {
	return a.selectAccountWithPreference(route.StickyKey, model, route.PreferredParentAccountID, excluded)
}

func (a *app) selectAccountWithPreference(stickyKey, model, preferredParentAccountID string, excluded map[string]bool) (account, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	stickyChanged := false
	if existing, ok := a.state.StickySessions[stickyKey]; ok && !excluded[existing.AccountID] {
		if a.stickySessionExpiredLocked(existing, now) {
			delete(a.state.StickySessions, stickyKey)
			stickyChanged = true
		} else if item := a.accountLocked(existing.AccountID); item != nil && a.usableLocked(*item, model, now) {
			if a.preserveProQuota && a.proAccountLocked(*item) {
				if replacement, ok := a.preferredAccountForStickyLocked(stickyKey, model, excluded, now); ok && replacement.ID != item.ID && !a.proAccountLocked(replacement) {
					return replacement, nil
				}
			}
			return *item, nil
		} else if item == nil {
			delete(a.state.StickySessions, stickyKey)
			stickyChanged = true
		}
	}
	if stickyChanged {
		_ = a.saveLocked()
	}
	// Parent affinity applies only while assigning an unbound child thread. It
	// must stay a soft preference behind every normal eligibility, duplicate-
	// identity, cooldown, and Pro-preservation gate; turning this into a hard pin
	// would let an unhealthy parent account block or weaken normal failover.
	if preferredParentAccountID != "" {
		if parent := a.accountLocked(preferredParentAccountID); parent != nil && a.selectableAccountLocked(*parent, model, excluded, now) {
			if !a.preserveProQuota || !a.proAccountLocked(*parent) {
				return *parent, nil
			}
			if replacement, ok := a.preferredAccountForStickyLocked(stickyKey, model, excluded, now); !ok || a.proAccountLocked(replacement) {
				return *parent, nil
			}
		}
	}
	selected, ok := a.preferredAccountForStickyLocked(stickyKey, model, excluded, now)
	if !ok {
		return account{}, errors.New("no eligible account is available for the requested model")
	}
	return selected, nil
}

func (a *app) selectableAccountLocked(item account, model string, excluded map[string]bool, now time.Time) bool {
	if excluded[item.ID] || !a.usableLocked(item, model, now) {
		return false
	}
	identity := a.upstreamIdentityKeyLocked(item)
	if identity != "" && a.excludedUpstreamIdentitiesLocked(excluded)[identity] {
		return false
	}
	if primaryID := a.primaryUpstreamAccountIDLocked(item, model, now); primaryID != "" && primaryID != item.ID {
		return false
	}
	return true
}

func (a *app) preferredAccountLocked(model string, excluded map[string]bool, now time.Time) (account, bool) {
	eligible := a.eligibleAccountsLocked(model, excluded, now)
	if len(eligible) == 0 {
		return account{}, false
	}
	sort.SliceStable(eligible, func(i, j int) bool { return a.preferredBeforeLocked(eligible[i], eligible[j]) })
	return eligible[0], true
}

func (a *app) eligibleAccountsLocked(model string, excluded map[string]bool, now time.Time) []account {
	eligible := make([]account, 0)
	// `excluded` is per user request. If one local slot failed, any other slot
	// with the same upstream identity must be excluded too; otherwise a single
	// upstream ChatGPT account can masquerade as multiple failover accounts.
	excludedIdentities := a.excludedUpstreamIdentitiesLocked(excluded)
	for _, item := range a.config.Accounts {
		if excluded[item.ID] || !a.usableLocked(item, model, now) {
			continue
		}
		identity := a.upstreamIdentityKeyLocked(item)
		if identity != "" && excludedIdentities[identity] {
			continue
		}
		if primaryID := a.primaryUpstreamAccountIDLocked(item, model, now); primaryID != "" && primaryID != item.ID {
			continue
		}
		eligible = append(eligible, item)
	}
	return eligible
}

// preferredAccountForStickyLocked assigns only an unbound route. Existing
// sticky routes and parent affinity are resolved before this function. In the
// balanced strategy the route key, not arrival order or completed-route counts,
// chooses the account. That deterministic choice prevents simultaneous cold
// starts from stampeding one "least loaded" account and guarantees concurrent
// first turns for the same session select the same credential before a success
// has had time to persist the sticky binding.
func (a *app) preferredAccountForStickyLocked(stickyKey, model string, excluded map[string]bool, now time.Time) (account, bool) {
	eligible := a.eligibleAccountsLocked(model, excluded, now)
	if len(eligible) == 0 {
		return account{}, false
	}
	if a.effectiveRoutingStrategy() == routingStrategyFailover {
		sort.SliceStable(eligible, func(i, j int) bool { return a.preferredBeforeLocked(eligible[i], eligible[j]) })
		return eligible[0], true
	}

	// Preserve the existing priority contract as capacity tiers, then balance
	// sessions only across the best eligible tier. Newly-created accounts all use
	// the same priority, so normal pools distribute evenly; operators who
	// intentionally configured a lower priority still retain a standby tier.
	if a.preserveProQuota {
		hasNonPro := false
		for _, item := range eligible {
			if !a.proAccountLocked(item) {
				hasNonPro = true
				break
			}
		}
		if hasNonPro {
			filtered := eligible[:0]
			for _, item := range eligible {
				if !a.proAccountLocked(item) {
					filtered = append(filtered, item)
				}
			}
			eligible = filtered
		}
	}
	highestPriority := eligible[0].Priority
	for _, item := range eligible[1:] {
		if item.Priority > highestPriority {
			highestPriority = item.Priority
		}
	}
	tier := eligible[:0]
	for _, item := range eligible {
		if item.Priority == highestPriority {
			tier = append(tier, item)
		}
	}

	selected := tier[0]
	selectedScore := a.stickyBalanceScore(stickyKey, selected)
	for _, item := range tier[1:] {
		score := a.stickyBalanceScore(stickyKey, item)
		comparison := bytes.Compare(score[:], selectedScore[:])
		if comparison > 0 || (comparison == 0 && item.ID < selected.ID) {
			selected = item
			selectedScore = score
		}
	}
	return selected, true
}

func (a *app) stickyBalanceScore(stickyKey string, item account) [sha256.Size]byte {
	identity := a.upstreamIdentityKeyLocked(item)
	if identity == "" {
		identity = "slot:" + item.ID
	}
	return sha256.Sum256([]byte(stickyKey + "\x00" + identity))
}

func (a *app) effectiveRoutingStrategy() string {
	if a.routingStrategy == routingStrategyFailover {
		return routingStrategyFailover
	}
	return routingStrategyBalanced
}

func (a *app) hasPreferredAccount(model string, excluded map[string]bool) bool {
	a.mu.RLock()
	defer a.mu.RUnlock()
	_, ok := a.preferredAccountLocked(model, excluded, time.Now().UTC())
	return ok
}

func (a *app) preferredBeforeLocked(left, right account) bool {
	if a.preserveProQuota {
		leftPro := a.proAccountLocked(left)
		rightPro := a.proAccountLocked(right)
		if leftPro != rightPro {
			return !leftPro
		}
	}
	if left.Priority == right.Priority {
		return left.ID < right.ID
	}
	return left.Priority > right.Priority
}

func (a *app) excludedUpstreamIdentitiesLocked(excluded map[string]bool) map[string]bool {
	identities := map[string]bool{}
	for _, item := range a.config.Accounts {
		if !excluded[item.ID] {
			continue
		}
		if identity := a.upstreamIdentityKeyLocked(item); identity != "" {
			identities[identity] = true
		}
	}
	return identities
}

func (a *app) primaryUpstreamAccountIDLocked(item account, model string, now time.Time) string {
	identity := a.upstreamIdentityKeyLocked(item)
	if identity == "" {
		return ""
	}
	// Choose the representative from the local credential copies that are usable
	// right now, not merely from the first slot with this upstream account id.
	// ChatGPT/Codex device-auth slots from the same visible account can carry
	// different quota snapshots or session-scoped rate limits; if a stale,
	// zero-quota, or cooling-down slot keeps owning the identity, the router falls
	// through to Pro while a healthy Team/Plus credential copy sits idle. The
	// duplicate guard still applies inside one failed request through
	// excludedUpstreamIdentitiesLocked, so a sibling is not used as an immediate
	// retry target after the representative gets a 429/5xx.
	candidates := make([]account, 0)
	for _, candidate := range a.config.Accounts {
		if !candidate.Enabled || !candidate.InPool {
			continue
		}
		if model != "" && !allowedModel(candidate, model) {
			continue
		}
		if !a.hasUsableAuthLocked(candidate) {
			continue
		}
		if a.accountMetadataErrorLocked(candidate.ID) {
			continue
		}
		if a.upstreamIdentityKeyLocked(candidate) == identity {
			candidates = append(candidates, candidate)
		}
	}
	if len(candidates) == 0 {
		return ""
	}
	sort.SliceStable(candidates, func(i, j int) bool {
		return a.identityRepresentativeBeforeLocked(candidates[i], candidates[j], model, now)
	})
	return candidates[0].ID
}

func (a *app) identityRepresentativeBeforeLocked(left, right account, model string, now time.Time) bool {
	leftCooldown := a.accountCoolingDownLocked(left.ID, model, now)
	rightCooldown := a.accountCoolingDownLocked(right.ID, model, now)
	if leftCooldown != rightCooldown {
		return !leftCooldown
	}
	leftQuotaClass, leftRemaining := a.identityRepresentativeQuotaClassLocked(left)
	rightQuotaClass, rightRemaining := a.identityRepresentativeQuotaClassLocked(right)
	if leftQuotaClass != rightQuotaClass {
		return leftQuotaClass > rightQuotaClass
	}
	if leftQuotaClass == 2 && leftRemaining != rightRemaining {
		return leftRemaining > rightRemaining
	}
	return a.preferredBeforeLocked(left, right)
}

func (a *app) identityRepresentativeQuotaClassLocked(item account) (int, int) {
	snapshot := a.state.Quotas[item.ID]
	if snapshot.QuotaError != nil {
		return 0, 0
	}
	if snapshot.Quota != nil {
		remaining := remainingQuotaHint(*snapshot.Quota)
		if remaining > 0 {
			return 2, remaining
		}
		return 0, 0
	}
	if available, decided := manualQuotaAvailable(item); decided {
		if available {
			return 2, *item.RemainingQuota
		}
		return 0, 0
	}
	return 1, 0
}

func (a *app) accountCoolingDownLocked(accountID, model string, now time.Time) bool {
	for _, cd := range a.state.Cooldowns[accountID] {
		if model != "" && cd.ModelID != model {
			continue
		}
		if cd.NextRetryAt.After(now) {
			return true
		}
	}
	return false
}

func (a *app) duplicateUpstreamAccountPrimaryLocked(item account, now time.Time) string {
	// Dashboard/status view mirrors the routing rule. A duplicate slot is only
	// called duplicate when it is otherwise active and authenticated; disabled,
	// out-of-pool, or missing-auth slots keep their more direct status.
	if !item.Enabled || !item.InPool || !a.hasUsableAuthLocked(item) {
		return ""
	}
	primaryID := a.primaryUpstreamAccountIDLocked(item, "", now)
	if primaryID == "" || primaryID == item.ID {
		return ""
	}
	return primaryID
}

func (a *app) accountMetadataErrorLocked(accountID string) bool {
	return quotaErrorBlocksRouting(a.state.Quotas[accountID].QuotaError)
}

func quotaErrorBlocksRouting(info *quotaErrorInfo) bool {
	if info == nil {
		return false
	}
	// Quota polling is advisory and uses a different upstream path from inference.
	// A transient usage API 5xx, timeout, or decode failure must not remove a
	// healthy Pro fallback and turn a non-Pro-to-Pro transition into a false 503.
	// Only errors that explicitly prove the credential is unusable may gate
	// routing; the proxy path will still quarantine a credential if inference
	// itself later returns 401/403.
	switch sanitizedErrorCode(info.Code) {
	case "account_auth_failed", "invalid_token", "token_invalidated", "token_revoked", "unauthorized", "forbidden":
		return true
	default:
		return false
	}
}

func (a *app) hasUsableAuthLocked(item account) bool {
	if isCodexDeviceAuth(item) {
		_, err := a.codexAuth(item)
		return err == nil
	}
	return strings.TrimSpace(item.UpstreamBaseURL) != ""
}

// upstreamIdentityKeyLocked is deliberately based on the upstream ChatGPT/Codex
// account identity, not the local slot ID. A single browser login can create
// several local device-auth slots from the same upstream account, especially
// when the host's own Codex session and the pool are authenticated from the same
// IP. Treating those slots as separate capacity causes noisy failover and can
// amplify refresh-token or team-workspace policy failures, so routing only lets
// one enabled slot per upstream identity become eligible.
func (a *app) upstreamIdentityKeyLocked(item account) string {
	if !isCodexDeviceAuth(item) {
		return ""
	}
	if accountID := strings.TrimSpace(item.AccountID); accountID != "" {
		return "codex-account:" + accountID
	}
	auth, err := a.codexAuth(item)
	if err == nil {
		if accountID := strings.TrimSpace(auth.AccountID); accountID != "" {
			return "codex-account:" + accountID
		}
		email := normalizeEmail(auth.Email)
		organization := cleanOrganizationName(auth.OrganizationName)
		if email != "" || organization != "" {
			return "codex-profile:" + email + "|" + organization
		}
	}
	email := normalizeEmail(item.Email)
	organization := cleanOrganizationName(item.OrganizationName)
	if email != "" || organization != "" {
		return "codex-profile:" + email + "|" + organization
	}
	return ""
}

func (a *app) proxyAttemptLimit() int {
	a.mu.RLock()
	defer a.mu.RUnlock()
	accountCount := len(a.config.Accounts)
	if accountCount <= 0 {
		return 1
	}
	if a.maxRetryAccounts > 0 && a.maxRetryAccounts < accountCount {
		return a.maxRetryAccounts
	}
	return accountCount
}

func (a *app) usableLocked(item account, model string, now time.Time) bool {
	if !item.Enabled || !item.InPool || !allowedModel(item, model) {
		return false
	}
	if primaryID := a.primaryUpstreamAccountIDLocked(item, model, now); primaryID != "" && primaryID != item.ID {
		return false
	}
	if !a.quotaAvailableLocked(item, model, now) {
		return false
	}
	if isCodexDeviceAuth(item) {
		if _, err := a.codexAuth(item); err != nil {
			return false
		}
	} else if item.UpstreamBaseURL == "" {
		return false
	}
	if a.accountCoolingDownLocked(item.ID, model, now) {
		return false
	}
	return true
}

func (a *app) quotaAvailableLocked(item account, model string, now time.Time) bool {
	snapshot := a.state.Quotas[item.ID]
	if quotaErrorBlocksRouting(snapshot.QuotaError) {
		return false
	}
	if snapshot.Quota != nil {
		if remainingQuotaHint(*snapshot.Quota) > 0 {
			return true
		}
		return a.sameIdentityQuotaHintAvailableLocked(item, model, now)
	}
	if available, decided := manualQuotaAvailable(item); decided {
		if available {
			return true
		}
		return a.sameIdentityQuotaHintAvailableLocked(item, model, now)
	}
	return true
}

func (a *app) quotaSnapshotAvailableLocked(accountID string) (bool, bool) {
	snapshot := a.state.Quotas[accountID]
	if quotaErrorBlocksRouting(snapshot.QuotaError) {
		return false, true
	}
	if snapshot.Quota != nil {
		return remainingQuotaHint(*snapshot.Quota) > 0, true
	}
	return false, false
}

func manualQuotaAvailable(item account) (bool, bool) {
	if item.RemainingQuota != nil {
		return *item.RemainingQuota > 0, true
	}
	return false, false
}

func (a *app) sameIdentityQuotaHintAvailableLocked(item account, model string, now time.Time) bool {
	// This is only a last-resort eligibility hint for the current representative.
	// The representative chooser should normally move traffic to the local
	// credential copy that has the positive quota snapshot. Keeping this fallback
	// prevents a transiently incomplete snapshot from forcing Pro usage, while the
	// primary-id check below stops a zero-quota duplicate from becoming routable.
	identity := a.upstreamIdentityKeyLocked(item)
	if identity == "" {
		return false
	}
	if primaryID := a.primaryUpstreamAccountIDLocked(item, model, now); primaryID != "" && primaryID != item.ID {
		return false
	}
	for _, candidate := range a.config.Accounts {
		if candidate.ID == item.ID || !candidate.Enabled || !candidate.InPool || !a.hasUsableAuthLocked(candidate) {
			continue
		}
		if a.upstreamIdentityKeyLocked(candidate) != identity {
			continue
		}
		// A duplicate slot with a persisted metadata/auth error is not evidence
		// that the shared upstream account has usable capacity. This path is only
		// a quota hint for the current representative; letting an errored sibling's
		// stale manual quota keep a zero-quota representative eligible caused the
		// router to hammer the same Team identity with 429s and occasionally return
		// false 503s instead of moving on to Pro/other identities.
		if a.accountMetadataErrorLocked(candidate.ID) {
			continue
		}
		if available, decided := a.quotaSnapshotAvailableLocked(candidate.ID); decided && available {
			return true
		}
		if available, decided := manualQuotaAvailable(candidate); decided && available {
			return true
		}
	}
	return false
}

func (a *app) proAccountLocked(item account) bool {
	plan := item.PlanType
	if snapshot := a.state.Quotas[item.ID]; snapshot.PlanType != "" {
		plan = snapshot.PlanType
	}
	return normalizePlanType(plan) == "pro"
}

func (a *app) markFailure(accountID, model, reason string, duration time.Duration) int {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.state.Health == nil {
		a.state.Health = map[string]accountHealth{}
	}
	now := time.Now().UTC()
	health := a.state.Health[accountID]
	// Repeated 5xx failover is a recent-health signal, not a permanent strike
	// counter. If an account had an old blip and then serves a hot sticky route
	// later, one fresh 5xx must not immediately move the route and lose KV cache
	// locality.
	if reason == "upstream_5xx" && (health.LastFailureReason != reason || now.Sub(health.LastFailureAt) > upstream5xxFailureWindow) {
		health.ConsecutiveFailure = 0
	}
	health.LastFailureAt = now
	health.LastFailureReason = reason
	health.ConsecutiveFailure++
	a.state.Health[accountID] = health
	a.state.FailureCount++
	if duration > 0 {
		a.state.Cooldowns[accountID] = append(a.state.Cooldowns[accountID], cooldown{ModelID: model, NextRetryAt: now.Add(duration), Reason: reason})
	}
	_ = a.saveLocked()
	return health.ConsecutiveFailure
}

func (a *app) markCooldown(accountID, model, reason string, duration time.Duration) {
	if duration <= 0 {
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	a.state.Cooldowns[accountID] = append(a.state.Cooldowns[accountID], cooldown{ModelID: model, NextRetryAt: now.Add(duration), Reason: reason})
	_ = a.saveLocked()
}

func (a *app) markAccountAuthFailure(accountID, model, reason string) {
	reason = codeOr(sanitizedErrorCode(reason), "account_auth_failed")
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.state.Health == nil {
		a.state.Health = map[string]accountHealth{}
	}
	now := time.Now().UTC()
	health := a.state.Health[accountID]
	health.LastFailureAt = now
	health.LastFailureReason = reason
	health.ConsecutiveFailure++
	a.state.Health[accountID] = health
	a.state.FailureCount++
	if item := a.accountLocked(accountID); item != nil && isCodexDeviceAuth(*item) {
		prior := a.state.Quotas[accountID]
		prior.AccountID = accountID
		prior.QuotaError = &quotaErrorInfo{Code: reason, Message: "account credential is unavailable; sign in again", Timestamp: now}
		a.state.Quotas[accountID] = prior
	} else {
		a.state.Cooldowns[accountID] = append(a.state.Cooldowns[accountID], cooldown{ModelID: model, NextRetryAt: now.Add(15 * time.Minute), Reason: reason})
	}
	_ = a.saveLocked()
}

func (a *app) markSuccess(route routingDecision, model, accountID string, info proxyResponseInfo) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	key := route.StickyKey
	if a.state.Health == nil {
		a.state.Health = map[string]accountHealth{}
	}
	health := a.state.Health[accountID]
	health.LastSuccessAt = now
	health.ConsecutiveFailure = 0
	a.state.Health[accountID] = health
	prior := a.state.StickySessions[key]
	failoverFrom := prior.FailoverFrom
	if prior.AccountID != "" && prior.AccountID != accountID {
		failoverFrom = prior.AccountID
	}
	a.state.StickySessions[key] = stickySession{Key: key, ModelID: model, AccountID: accountID, CreatedAt: chooseTime(prior.CreatedAt, now), LastSuccessAt: now, ExpiresAt: now.Add(a.stickyTTL()), FailoverFrom: failoverFrom}
	lineageFailover := route.Identity.IsSubagent && prior.AccountID != "" && prior.AccountID != accountID
	if route.Identity.ThreadID != "" {
		if a.state.ThreadBindings == nil {
			a.state.ThreadBindings = map[string]threadBinding{}
		}
		bindingKey := threadBindingStateKey(model, route.Identity.ThreadID)
		priorBinding := a.state.ThreadBindings[bindingKey]
		if route.Identity.IsSubagent && priorBinding.AccountID != "" && priorBinding.AccountID != accountID {
			lineageFailover = true
		}
		lineageRootID := route.Identity.LineageRootID
		if lineageRootID == "" {
			lineageRootID = route.Identity.ThreadID
		}
		a.state.ThreadBindings[bindingKey] = threadBinding{
			ThreadID:       route.Identity.ThreadID,
			SessionID:      route.Identity.SessionID,
			ParentThreadID: identityParentID(route.Identity),
			LineageRootID:  lineageRootID,
			SubagentKind:   route.Identity.SubagentKind,
			ModelID:        model,
			AccountID:      accountID,
			StickyKey:      key,
			PromptCacheKey: route.UpstreamPromptCacheKey,
			CreatedAt:      chooseTime(priorBinding.CreatedAt, now),
			LastSuccessAt:  now,
			ExpiresAt:      now.Add(a.stickyTTL()),
		}
	}
	if info.ResponseID != "" {
		if a.state.ResponseBindings == nil {
			a.state.ResponseBindings = map[string]responseBinding{}
		}
		a.state.ResponseBindings[info.ResponseID] = responseBinding{ResponseID: info.ResponseID, StickyKey: key, ModelID: model, AccountID: accountID, CreatedAt: now, ExpiresAt: now.Add(a.stickyTTL())}
	}
	parentAffinityHit := route.ParentAffinityAttempted && route.PreferredParentAccountID != "" && route.PreferredParentAccountID == accountID
	parentAffinityFallback := route.ParentAffinityAttempted && !parentAffinityHit
	if route.ParentAffinityAttempted && route.PreferredParentAccountID != "" && route.PreferredParentAccountID != accountID {
		lineageFailover = true
	}
	a.recordPromptCacheResultLocked(accountID, model, route.Identity, info.Usage, parentAffinityHit, parentAffinityFallback, lineageFailover, now)
	a.state.RequestCount++
	a.state.SuccessCount++
	_ = a.saveLocked()
}

func (a *app) recordPromptCacheUsageLocked(accountID, model string, usage promptCacheUsage, now time.Time) {
	a.recordPromptCacheResultLocked(accountID, model, requestIdentity{}, usage, false, false, false, now)
}

func (a *app) recordPromptCacheResultLocked(accountID, model string, identity requestIdentity, usage promptCacheUsage, parentAffinityHit, parentAffinityFallback, lineageFailover bool, now time.Time) {
	if a.state.PromptCache == nil {
		a.state.PromptCache = map[string]promptCacheStat{}
	}
	agentKind := "main"
	if identity.IsSubagent {
		agentKind = "subagent"
	}
	key := accountID + ":" + model + ":" + agentKind
	stat := a.state.PromptCache[key]
	stat.AccountID = accountID
	stat.ModelID = model
	stat.AgentKind = agentKind
	stat.RequestCount++
	if usage.Present {
		stat.InputTokens += usage.InputTokens
		stat.CachedTokens += usage.CachedTokens
	}
	if usage.Present && usage.InputTokens >= promptCacheMinTokens && usage.CachedTokens == 0 {
		stat.ColdRequestCount++
	}
	if parentAffinityHit {
		stat.ParentAffinityHitCount++
	}
	if parentAffinityFallback {
		stat.ParentAffinityFallbackCount++
	}
	if lineageFailover {
		stat.LineageFailoverCount++
	}
	stat.UpdatedAt = now
	a.state.PromptCache[key] = stat
}

func deletePromptCacheForAccount(values map[string]promptCacheStat, accountID string) {
	for key, value := range values {
		if value.AccountID == accountID {
			delete(values, key)
		}
	}
}

func (a *app) clearAccountRuntimeStateLocked(accountID string) {
	if a.state.Health != nil {
		health := a.state.Health[accountID]
		health.ConsecutiveFailure = 0
		health.LastFailureReason = ""
		a.state.Health[accountID] = health
	}
	if a.state.Cooldowns != nil {
		delete(a.state.Cooldowns, accountID)
	}
	if a.state.Quotas != nil {
		snapshot := a.state.Quotas[accountID]
		if snapshot.AccountID != "" || snapshot.Quota != nil || snapshot.QuotaError != nil || !snapshot.UsageUpdatedAt.IsZero() {
			snapshot.AccountID = accountID
			snapshot.QuotaError = nil
			a.state.Quotas[accountID] = snapshot
		}
	}
}

func (a *app) stickyTTL() time.Duration {
	if a.sessionAffinityTTL > 0 {
		return a.sessionAffinityTTL
	}
	return sessionAffinityTTLDefault
}

func (a *app) stickyExpiresAt(item stickySession) time.Time {
	if !item.ExpiresAt.IsZero() {
		return item.ExpiresAt
	}
	base := item.LastSuccessAt
	if base.IsZero() {
		base = item.CreatedAt
	}
	if base.IsZero() {
		return time.Time{}
	}
	return base.Add(a.stickyTTL())
}

func (a *app) stickySessionExpiredLocked(item stickySession, now time.Time) bool {
	expiresAt := a.stickyExpiresAt(item)
	return expiresAt.IsZero() || !expiresAt.After(now)
}

func (a *app) pruneExpiredStickySessionsLocked(now time.Time) bool {
	changed := false
	for key, item := range a.state.StickySessions {
		if a.stickySessionExpiredLocked(item, now) {
			delete(a.state.StickySessions, key)
			changed = true
		}
	}
	return changed
}

func (a *app) pruneExpiredRuntimeStateLocked(now time.Time) bool {
	changed := a.pruneExpiredStickySessionsLocked(now)
	for id, binding := range a.state.ResponseBindings {
		if !binding.ExpiresAt.After(now) {
			delete(a.state.ResponseBindings, id)
			changed = true
		}
	}
	for id, binding := range a.state.ThreadBindings {
		if !binding.ExpiresAt.After(now) {
			delete(a.state.ThreadBindings, id)
			changed = true
		}
	}
	return changed
}

func (a *app) latestStickySessionLocked(model string, now time.Time) (stickySession, bool) {
	var best stickySession
	found := false
	for _, item := range a.state.StickySessions {
		if item.ModelID != model || a.stickySessionExpiredLocked(item, now) {
			continue
		}
		if !found || stickyActivityTime(item).After(stickyActivityTime(best)) {
			best = item
			found = true
		}
	}
	return best, found
}

func stickyActivityTime(item stickySession) time.Time {
	if !item.LastSuccessAt.IsZero() {
		return item.LastSuccessAt
	}
	return item.CreatedAt
}

func (a *app) accountLocked(id string) *account {
	for i := range a.config.Accounts {
		if a.config.Accounts[i].ID == id {
			return &a.config.Accounts[i]
		}
	}
	return nil
}

func (a *app) accountWithIndexLocked(id string) (*account, int) {
	for i := range a.config.Accounts {
		if a.config.Accounts[i].ID == id {
			return &a.config.Accounts[i], i
		}
	}
	return nil, -1
}
func (a *app) clearStickyForAccountLocked(id string) {
	for key, item := range a.state.StickySessions {
		if item.AccountID == id {
			delete(a.state.StickySessions, key)
		}
	}
	for key, item := range a.state.ResponseBindings {
		if item.AccountID == id {
			delete(a.state.ResponseBindings, key)
		}
	}
	for key, item := range a.state.ThreadBindings {
		if item.AccountID == id {
			delete(a.state.ThreadBindings, key)
		}
	}
}

func (a *app) accountRoot(id string) string {
	return filepath.Join(a.dataDir, "accounts", id)
}

func (a *app) accountCodexHome(id string) string {
	return filepath.Join(a.accountRoot(id), ".codex")
}

func (a *app) saveLocked() error {
	a.config.UpdatedAt = time.Now().UTC()
	a.state.UpdatedAt = time.Now().UTC()
	if err := writeJSONAtomic(filepath.Join(a.dataDir, "config.json"), a.config); err != nil {
		return err
	}
	return writeJSONAtomic(filepath.Join(a.dataDir, "state", "runtime.json"), a.state)
}

func loadAPIKeys() ([][]byte, error) {
	raw := append(strings.Split(os.Getenv("CODEX_POOL_API_KEYS"), ","), os.Getenv("CODEX_POOL_API_KEY"))
	keys := make([][]byte, 0, len(raw))
	for _, value := range raw {
		if value = strings.TrimSpace(value); value != "" {
			if strings.Contains(value, "replace_with") || strings.Contains(value, "replace-with") {
				return nil, errors.New("public API key cannot use an example value")
			}
			keys = append(keys, []byte(value))
		}
	}
	if len(keys) == 0 {
		return nil, errors.New("CODEX_POOL_API_KEY or CODEX_POOL_API_KEYS is required")
	}
	return keys, nil
}

func parseModel(value string) (string, string) {
	end := strings.LastIndex(value, ")")
	start := strings.LastIndex(value, "(")
	if start > 0 && end == len(value)-1 {
		tier := value[start+1 : end]
		switch tier {
		case "none", "auto", "minimal", "low", "medium", "high", "xhigh", "max", "ultra":
			return value[:start], tier
		}
	}
	return value, ""
}

func (a *app) resolveModel(requestedModel string) string {
	a.mu.RLock()
	defaultModel := a.config.DefaultModel
	a.mu.RUnlock()
	if strings.TrimSpace(requestedModel) == "" {
		requestedModel = defaultModel
	}
	model, _ := parseModel(requestedModel)
	a.mu.RLock()
	if alias, ok := a.config.ModelAliases[model]; ok {
		model = alias
	}
	a.mu.RUnlock()
	return model
}

func currentStatusStickyKey(r *http.Request, model string) (string, string) {
	values := []struct {
		value  string
		source string
	}{
		{r.Header.Get("X-Codex-Pool-Session"), "session"},
		{r.URL.Query().Get("session"), "session"},
		{r.Header.Get("X-Codex-Pool-Project"), "project"},
		{r.URL.Query().Get("project"), "project"},
	}
	for _, item := range values {
		value := strings.TrimSpace(item.value)
		if value != "" {
			return model + ":" + value, item.source
		}
	}
	return "", ""
}

type routingDecision struct {
	StickyKey                string
	UpstreamPromptCacheKey   string
	Source                   string
	SourceValue              string
	Identity                 requestIdentity
	ClientPromptCacheKey     string
	PreferredParentAccountID string
	ParentAffinityAttempted  bool
}

// routingDecision deliberately keeps account affinity and the upstream prompt
// cache key separate. Codex main/child threads need independent sticky routes,
// while an operator may explicitly group their backend cache keys; a bound
// previous_response_id remains authoritative across client metadata skew.
func (a *app) routingDecision(r *http.Request, payload map[string]any, model, apiKey string) routingDecision {
	identity := requestIdentityFrom(r, payload)
	identity, parentAccountID, parentFound := a.resolveRequestIdentity(identity, model)
	clientPromptCacheKey := stringValue(payload["prompt_cache_key"])
	finish := func(route routingDecision) routingDecision {
		route.Identity = identity
		route.ClientPromptCacheKey = clientPromptCacheKey
		route.PreferredParentAccountID = parentAccountID
		route.ParentAffinityAttempted = identity.IsSubagent && identityParentID(identity) != "" && !a.liveStickySession(route.StickyKey)
		if !parentFound {
			route.PreferredParentAccountID = ""
		}
		route.UpstreamPromptCacheKey = a.upstreamPromptCacheKey(r, model, apiKey, route)
		return route
	}
	if previousResponseID := stringValue(payload["previous_response_id"]); previousResponseID != "" {
		if binding, found := a.responseBinding(previousResponseID, model); found {
			// The response chain is stronger evidence than version-skewed turn
			// metadata. Recover the bound thread identity so success does not
			// create a second, incorrect thread binding for the same continuation.
			if boundIdentity, ok := a.requestIdentityForSticky(binding.StickyKey, model); ok {
				identity, parentAccountID, parentFound = a.resolveRequestIdentity(boundIdentity, model)
			}
			return finish(routingDecision{StickyKey: binding.StickyKey, Source: "previous_response_id", SourceValue: binding.StickyKey})
		}
	}
	if identity.ThreadID != "" {
		stickyKey := model + ":thread:" + identity.ThreadID
		return finish(routingDecision{StickyKey: stickyKey, Source: "thread_id", SourceValue: identity.ThreadID})
	}
	for _, item := range []struct {
		value  string
		source string
	}{
		{r.Header.Get("X-Codex-Pool-Session"), "session"},
		{r.Header.Get("X-Codex-Pool-Project"), "project"},
	} {
		if value := strings.TrimSpace(item.value); value != "" {
			stickyKey := model + ":" + value
			return finish(routingDecision{StickyKey: stickyKey, Source: item.source, SourceValue: value})
		}
	}
	if clientPromptCacheKey != "" {
		return finish(routingDecision{StickyKey: model + ":" + clientPromptCacheKey, Source: "prompt_cache_key", SourceValue: clientPromptCacheKey})
	}
	if value := conversationID(payload); value != "" {
		stickyKey := model + ":" + value
		return finish(routingDecision{StickyKey: stickyKey, Source: "conversation", SourceValue: value})
	}
	for _, name := range []string{"session_id", "conversation_id"} {
		if value := stringValue(payload[name]); value != "" {
			stickyKey := model + ":" + value
			return finish(routingDecision{StickyKey: stickyKey, Source: name, SourceValue: value})
		}
	}
	if previousResponseID := stringValue(payload["previous_response_id"]); previousResponseID != "" {
		value := "previous:" + shortHash([]byte(previousResponseID))
		stickyKey := model + ":" + value
		return finish(routingDecision{StickyKey: stickyKey, Source: "previous_response_id", SourceValue: value})
	}
	return finish(a.fallbackRoutingDecision(payload, model, apiKey))
}

// requestIdentityFrom accepts multiple Codex metadata generations. Canonical
// turn metadata wins over flat client metadata, compatibility headers, and
// top-level fallbacks; malformed JSON is ignored so version skew cannot reject
// an otherwise valid Responses request.
func requestIdentityFrom(r *http.Request, payload map[string]any) requestIdentity {
	var identity requestIdentity
	clientMetadata := metadataObject(payload["client_metadata"])
	mergeRequestIdentity(&identity, metadataObject(firstMetadataValue(clientMetadata, "x-codex-turn-metadata", "x_codex_turn_metadata")))
	mergeRequestIdentity(&identity, clientMetadata)
	mergeRequestIdentity(&identity, metadataObject(r.Header.Get("X-Codex-Turn-Metadata")))
	mergeRequestIdentity(&identity, map[string]any{
		"session_id":            r.Header.Get("X-Codex-Session-ID"),
		"thread_id":             r.Header.Get("X-Codex-Thread-ID"),
		"parent_thread_id":      r.Header.Get("X-Codex-Parent-Thread-ID"),
		"forked_from_thread_id": r.Header.Get("X-Codex-Forked-From-Thread-ID"),
		"lineage_root_id":       r.Header.Get("X-Codex-Lineage-Root-ID"),
		"subagent_kind":         r.Header.Get("X-OpenAI-Subagent"),
		"thread_source":         r.Header.Get("X-Codex-Thread-Source"),
	})
	mergeRequestIdentity(&identity, payload)
	if identity.ParentThreadID == "" {
		identity.ParentThreadID = identity.ForkedFromID
	}
	identity.IsSubagent = identity.ParentThreadID != "" || identity.ForkedFromID != "" || identity.SubagentKind != "" || strings.Contains(strings.ToLower(identity.ThreadSource), "subagent")
	return identity
}

func mergeRequestIdentity(identity *requestIdentity, values map[string]any) {
	if len(values) == 0 {
		return
	}
	setIdentityValue(&identity.SessionID, metadataString(values, "session_id", "sessionId"))
	setIdentityValue(&identity.ThreadID, metadataString(values, "thread_id", "threadId"))
	setIdentityValue(&identity.ParentThreadID, metadataString(values, "parent_thread_id", "parentThreadId", "x-codex-parent-thread-id"))
	setIdentityValue(&identity.ForkedFromID, metadataString(values, "forked_from_thread_id", "forkedFromThreadId"))
	setIdentityValue(&identity.LineageRootID, metadataString(values, "lineage_root_id", "lineageRootId"))
	setIdentityValue(&identity.SubagentKind, metadataString(values, "subagent_kind", "subagentKind", "x-openai-subagent"))
	setIdentityValue(&identity.ThreadSource, metadataString(values, "thread_source", "threadSource"))
}

func setIdentityValue(target *string, value string) {
	if *target == "" && value != "" {
		*target = value
	}
}

func metadataObject(value any) map[string]any {
	switch typed := value.(type) {
	case map[string]any:
		return typed
	case string:
		var decoded map[string]any
		if json.Unmarshal([]byte(typed), &decoded) == nil {
			return decoded
		}
	}
	return nil
}

func firstMetadataValue(values map[string]any, keys ...string) any {
	for _, key := range keys {
		if value, ok := values[key]; ok {
			return value
		}
	}
	return nil
}

func metadataString(values map[string]any, keys ...string) string {
	for _, key := range keys {
		if value, ok := values[key]; ok {
			if normalized := requestIdentityString(value); normalized != "" {
				return normalized
			}
		}
	}
	return ""
}

func stringValue(value any) string {
	text, _ := value.(string)
	return strings.TrimSpace(text)
}

func requestIdentityString(value any) string {
	text := stringValue(value)
	if len(text) > maxRequestIdentityValue {
		text = text[:maxRequestIdentityValue]
	}
	return text
}

func identityParentID(identity requestIdentity) string {
	if identity.ParentThreadID != "" {
		return identity.ParentThreadID
	}
	return identity.ForkedFromID
}

func threadBindingStateKey(model, threadID string) string {
	return model + ":" + threadID
}

func (a *app) resolveRequestIdentity(identity requestIdentity, model string) (requestIdentity, string, bool) {
	parentID := identityParentID(identity)
	now := time.Now().UTC()
	a.mu.RLock()
	parent, parentFound := a.state.ThreadBindings[threadBindingStateKey(model, parentID)]
	parentFound = parentFound && parent.ExpiresAt.After(now)
	a.mu.RUnlock()
	if identity.LineageRootID == "" {
		switch {
		case parentFound && parent.LineageRootID != "":
			identity.LineageRootID = parent.LineageRootID
		case parentFound:
			identity.LineageRootID = parent.ThreadID
		case parentID != "":
			identity.LineageRootID = parentID
		case identity.ThreadID != "":
			identity.LineageRootID = identity.ThreadID
		}
	}
	if parentFound {
		return identity, parent.AccountID, true
	}
	return identity, "", false
}

func (a *app) requestIdentityForSticky(stickyKey, model string) (requestIdentity, bool) {
	now := time.Now().UTC()
	a.mu.RLock()
	defer a.mu.RUnlock()
	for _, binding := range a.state.ThreadBindings {
		if binding.ModelID != model || binding.StickyKey != stickyKey || !binding.ExpiresAt.After(now) {
			continue
		}
		identity := requestIdentity{
			SessionID:      binding.SessionID,
			ThreadID:       binding.ThreadID,
			ParentThreadID: binding.ParentThreadID,
			LineageRootID:  binding.LineageRootID,
			SubagentKind:   binding.SubagentKind,
		}
		identity.IsSubagent = identity.ParentThreadID != "" || identity.SubagentKind != ""
		return identity, true
	}
	return requestIdentity{}, false
}

func (a *app) liveStickySession(stickyKey string) bool {
	now := time.Now().UTC()
	a.mu.RLock()
	defer a.mu.RUnlock()
	binding, found := a.state.StickySessions[stickyKey]
	return found && !a.stickySessionExpiredLocked(binding, now)
}

// scopedPromptCacheKey builds the prompt_cache_key. The default "auto" (and the
// explicit "project"/"user") scopes group conversations that share the same
// static prefix (system prompt + tools) under one key so they reuse each other's
// prompt cache on the account the router already concentrates them on. stickyKey,
// the stable per-conversation routing key, is hashed into a small number of
// buckets so each (prefix + key) combination stays under OpenAI's ~15 RPM
// cache-routing limit. When no coarse signal is available the key falls back to
// the historical per-conversation format, so behaviour never gets worse.
func (a *app) scopedPromptCacheKey(r *http.Request, model, apiKey, source, value, stickyKey string) string {
	scope, coarse := a.promptCacheScope(r, apiKey)
	if scope == "" {
		return promptCacheKeyHash(model, source, value)
	}
	bucket := promptCacheBucketIndex(stickyKey, a.promptCacheBuckets)
	return promptCacheKeyHash(model, scope, fmt.Sprintf("%s#%d", coarse, bucket))
}

// promptCacheScope returns the coarse grouping ("project" or "user") and its
// value, or ("", "") to fall back to per-conversation keys.
func (a *app) promptCacheScope(r *http.Request, apiKey string) (string, string) {
	project := strings.TrimSpace(r.Header.Get("X-Codex-Pool-Project"))
	fingerprint := apiKeyFingerprint(apiKey)
	hasUser := fingerprint != "anonymous"
	switch a.promptCacheKeyScope {
	case "project":
		if project != "" {
			return "project", project
		}
		if hasUser {
			return "user", fingerprint
		}
	case "user":
		if hasUser {
			return "user", fingerprint
		}
	case "auto":
		if project != "" {
			return "project", project
		}
		if hasUser {
			return "user", fingerprint
		}
	}
	return "", ""
}

func promptCacheBucketIndex(stickyKey string, buckets int) int {
	if buckets <= 1 {
		return 0
	}
	sum := sha256.Sum256([]byte(stickyKey))
	return int(sum[0]) % buckets
}

func (a *app) responseBinding(responseID, model string) (responseBinding, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	binding, ok := a.state.ResponseBindings[responseID]
	if !ok || binding.ModelID != model || !binding.ExpiresAt.After(now) {
		if ok {
			delete(a.state.ResponseBindings, responseID)
			_ = a.saveLocked()
		}
		return responseBinding{}, false
	}
	return binding, true
}

func (a *app) fallbackRoutingDecision(payload map[string]any, model, apiKey string) routingDecision {
	prefix := normalizedPromptPrefix(payload)
	keyMaterial := append([]byte(apiKeyFingerprint(apiKey)+":"+model+":"), prefix...)
	value := shortHash(keyMaterial)
	stickyID := "prompt:" + value
	stickyKey := model + ":" + stickyID
	return routingDecision{StickyKey: stickyKey, Source: "prompt", SourceValue: value}
}

func (a *app) upstreamPromptCacheKey(r *http.Request, model, apiKey string, route routingDecision) string {
	policy := a.promptCacheKeyPolicy
	if policy == "" {
		policy = "preserve"
	}
	bucket := promptCacheBucketIndex(route.StickyKey, a.promptCacheBuckets)
	switch policy {
	case "lineage":
		root := route.Identity.LineageRootID
		if root == "" {
			root = route.Identity.ThreadID
		}
		if root == "" {
			root = route.StickyKey
		}
		return promptCacheKeyHash(model, "lineage", fmt.Sprintf("%s#%d", root, bucket))
	case "project":
		project := strings.TrimSpace(r.Header.Get("X-Codex-Pool-Project"))
		if project == "" {
			project = "user:" + apiKeyFingerprint(apiKey)
		}
		return promptCacheKeyHash(model, "project", fmt.Sprintf("%s#%d", project, bucket))
	case "user":
		return promptCacheKeyHash(model, "user", fmt.Sprintf("%s#%d", apiKeyFingerprint(apiKey), bucket))
	default:
		if route.ClientPromptCacheKey != "" {
			return route.ClientPromptCacheKey
		}
		if !a.autoPromptCacheKeyEnabled() {
			return ""
		}
		return a.scopedPromptCacheKey(r, model, apiKey, route.Source, route.SourceValue, route.StickyKey)
	}
}

func (a *app) applyPromptCacheControls(payload map[string]any, route routingDecision) {
	if a.promptCacheRetention != "" {
		if _, exists := payload["prompt_cache_retention"]; !exists {
			payload["prompt_cache_retention"] = a.promptCacheRetention
		}
	}
	policy := a.promptCacheKeyPolicy
	// Preserve is the compatibility default: an existing Codex thread key must
	// not be rewritten. Only an explicit lineage/project/user policy may replace
	// a client key, independently of the legacy missing-key auto-injection mode.
	if policy == "" || policy == "preserve" {
		if !a.autoPromptCacheKeyEnabled() {
			return
		}
		if _, exists := payload["prompt_cache_key"]; exists {
			return
		}
	}
	if route.UpstreamPromptCacheKey != "" {
		payload["prompt_cache_key"] = route.UpstreamPromptCacheKey
	}
}

func (a *app) autoPromptCacheKeyEnabled() bool {
	return a.promptCacheKeyMode == "" || a.promptCacheKeyMode == "auto"
}

func conversationID(payload map[string]any) string {
	switch value := payload["conversation"].(type) {
	case string:
		return strings.TrimSpace(value)
	case map[string]any:
		for _, key := range []string{"id", "conversation_id", "conversationId"} {
			if id, ok := value[key].(string); ok && strings.TrimSpace(id) != "" {
				return strings.TrimSpace(id)
			}
		}
	}
	return ""
}

// hostedToolNamespaces maps hosted Responses tool types to the tool namespace
// the ChatGPT backend reserves for them when such a tool is declared in the
// same request.
var hostedToolNamespaces = map[string]string{
	"image_generation":   "image_gen",
	"image_gen":          "image_gen",
	"web_search":         "web_search",
	"web_search_preview": "web_search",
}

// alwaysReservedToolNamespaces are namespaces the ChatGPT Codex backend owns
// implicitly: the hosted twin is attached server-side for current models even
// when the request declares no hosted tool, so a client-declared twin always
// fails with "Function 'image_gen.imagegen' conflicts with a hosted tool in
// the same request". Verified against the live backend (2026-07): declaring a
// `namespace` tool named `image_gen` under an `additional_tools` input item
// reproduces that exact 400 with no hosted tool anywhere in the request.
var alwaysReservedToolNamespaces = []string{
	// TODO(upstream): DELETE the "image_gen" entry (and this comment) once
	// OpenAI fixes the Codex client/hosted collision tracked in
	// https://github.com/openai/codex/issues/28464 — their stated plan is to
	// retire the hosted image tool in favor of the standalone client
	// extension, at which point this reservation would silently strip a
	// then-legitimate client tool and disable image generation through the
	// pool. How to verify the fix shipped: POST /v1/responses upstream with an
	// `additional_tools` input item declaring {"type":"namespace","name":
	// "image_gen","tools":[{"type":"function","name":"imagegen",...}]}. While
	// the bug exists this returns 400 "Function 'image_gen.imagegen' conflicts
	// with a hosted tool in the same request"; once it is accepted, remove
	// this entry, update SPEC.md 6.4.2, and keep the hosted-pair dedupe below.
	"image_gen",
}

// dropHostedToolConflicts removes client-declared tools whose name lives in a
// namespace the upstream backend reserves — either implicitly (see
// alwaysReservedToolNamespaces) or because the same request also declares the
// hosted tool. Codex clients declare tools in two places: the top-level
// `tools` array and, since Codex 0.144, `additional_tools` items inside
// `input`; namespaced tools arrive as `{"type":"namespace","name":...}` and
// upstream flattens their functions into `namespace.function` names. Both
// locations and both shapes must be filtered, and the hosted capability is
// kept because upstream owns the namespace either way. Do not simplify this
// into forwarding tools verbatim: the conflict is generated by the Codex
// client's experimental feature set (for example multi-agent/image
// generation), not by user configuration, so the pool must stay tolerant.
func dropHostedToolConflicts(payload map[string]any) {
	reserved := map[string]bool{}
	for _, namespace := range alwaysReservedToolNamespaces {
		reserved[namespace] = true
	}
	topTools, _ := payload["tools"].([]any)
	for _, raw := range topTools {
		tool, _ := raw.(map[string]any)
		toolType, _ := tool["type"].(string)
		if namespace, hosted := hostedToolNamespaces[toolType]; hosted {
			reserved[namespace] = true
		}
	}
	if filtered, changed := filterReservedTools(topTools, reserved); changed {
		payload["tools"] = filtered
	}
	input, _ := payload["input"].([]any)
	for _, raw := range input {
		item, _ := raw.(map[string]any)
		if itemType, _ := item["type"].(string); itemType != "additional_tools" {
			continue
		}
		if tools, ok := item["tools"].([]any); ok {
			if filtered, changed := filterReservedTools(tools, reserved); changed {
				item["tools"] = filtered
			}
		}
	}
}

func filterReservedTools(tools []any, reserved map[string]bool) ([]any, bool) {
	if len(tools) == 0 {
		return tools, false
	}
	filtered := make([]any, 0, len(tools))
	for _, raw := range tools {
		tool, _ := raw.(map[string]any)
		toolType, _ := tool["type"].(string)
		if _, hosted := hostedToolNamespaces[toolType]; !hosted {
			name, _ := tool["name"].(string)
			namespace, _, hasNamespace := strings.Cut(name, ".")
			if reserved[name] || (hasNamespace && reserved[namespace]) {
				continue
			}
		}
		filtered = append(filtered, raw)
	}
	return filtered, len(filtered) != len(tools)
}

func normalizedPromptPrefix(payload map[string]any) []byte {
	prefix := map[string]any{}
	if value, ok := payload["input"]; ok {
		prefix["input"] = value
	} else if value, ok := payload["messages"]; ok {
		prefix["messages"] = value
	}
	for _, name := range []string{"tools", "text", "response_format"} {
		if value, ok := payload[name]; ok {
			prefix[name] = value
		}
	}
	encoded, _ := json.Marshal(prefix)
	const maxPromptPrefixBytes = 8192
	if len(encoded) > maxPromptPrefixBytes {
		encoded = encoded[:maxPromptPrefixBytes]
	}
	return encoded
}

func promptCacheKeyHash(model, source, value string) string {
	return "cp_" + shortHash([]byte(model+":"+source+":"+value))
}

func apiKeyFingerprint(value string) string {
	if strings.TrimSpace(value) == "" {
		return "anonymous"
	}
	return shortHash([]byte(value))
}

func shortHash(value []byte) string {
	sum := sha256.Sum256(value)
	return base64.RawURLEncoding.EncodeToString(sum[:16])
}

func allowedModel(item account, model string) bool {
	for _, excluded := range item.ExcludedModels {
		if excluded == model {
			return false
		}
	}
	if len(item.AllowedModels) == 0 {
		return true
	}
	for _, allowed := range item.AllowedModels {
		if allowed == model {
			return true
		}
	}
	return false
}

func normalWireAPI(value string) string {
	switch strings.ToLower(value) {
	case "chat_completions", "chat-completions", "openai_chat", "openai-chat", "chat":
		return "chat_completions"
	default:
		return "responses"
	}
}
func isCodexDeviceAuth(item account) bool {
	return item.AuthType == "codex_device_auth"
}

type codexAuthInfo struct {
	AccessToken      string
	AccountID        string
	Email            string
	OrganizationName string
	PlanType         string
	PlanLimit        string
	FedRAMP          bool
}

type codexAuthFile struct {
	AuthMode    string     `json:"auth_mode"`
	LastRefresh *time.Time `json:"last_refresh,omitempty"`
	Tokens      *struct {
		IDToken      string  `json:"id_token"`
		AccessToken  string  `json:"access_token"`
		RefreshToken string  `json:"refresh_token"`
		AccountID    *string `json:"account_id"`
	} `json:"tokens"`
}

// cliproxyCodexAuthFile is CLIProxyAPI's file-backed Codex OAuth record. The
// sidecar owns refreshes of this copy so Pool never races it for a refresh token.
type cliproxyCodexAuthFile struct {
	Type             string `json:"type"`
	Email            string `json:"email,omitempty"`
	IDToken          string `json:"id_token,omitempty"`
	AccessToken      string `json:"access_token"`
	RefreshToken     string `json:"refresh_token,omitempty"`
	AccountID        string `json:"account_id,omitempty"`
	OrganizationName string `json:"organization_name,omitempty"`
	LastRefresh      string `json:"last_refresh,omitempty"`
	Expire           string `json:"expired,omitempty"`
	Prefix           string `json:"prefix"`
	PlanType         string `json:"plan_type,omitempty"`
	PlanLimit        string `json:"plan_limit,omitempty"`
}

type codexRefreshResponse struct {
	IDToken      string `json:"id_token"`
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
}

type codexUsageResponse struct {
	PlanType            string                `json:"plan_type"`
	SubscriptionPlan    string                `json:"subscription_plan"`
	RateLimit           *codexRateLimitInfo   `json:"rate_limit"`
	CodeReviewRateLimit *codexRateLimitInfo   `json:"code_review_rate_limit"`
	ResetCredits        *codexResetCreditInfo `json:"rate_limit_reset_credits"`
}

type codexSubscriptionMetadata struct {
	AccountID        string
	OrganizationName string
	PlanType         string
	PlanLimit        string
}

type codexRateLimitInfo struct {
	Allowed         *bool            `json:"allowed"`
	LimitReached    *bool            `json:"limit_reached"`
	PrimaryWindow   *codexWindowInfo `json:"primary_window"`
	SecondaryWindow *codexWindowInfo `json:"secondary_window"`
}

type codexWindowInfo struct {
	UsedPercent        *int   `json:"used_percent"`
	LimitWindowSeconds *int64 `json:"limit_window_seconds"`
	ResetAfterSeconds  *int64 `json:"reset_after_seconds"`
	ResetAt            *int64 `json:"reset_at"`
}

type codexResetCreditInfo struct {
	AvailableCount *int64 `json:"available_count"`
}

func (a *app) codexAuth(item account) (codexAuthInfo, error) {
	auth, err := a.readCodexAuthFile(item)
	if err != nil {
		return codexAuthInfo{}, markAccountAuthError(err)
	}
	return codexAuthInfoFromFile(auth), nil
}

func (a *app) readCodexAuthFile(item account) (codexAuthFile, error) {
	path := filepath.Join(a.accountCodexHome(item.ID), "auth.json")
	var lastErr error
	for attempt := 0; attempt < codexAuthReadAttempts; attempt++ {
		auth, err := readCodexAuthFileOnce(path, item.ID)
		if err == nil {
			return auth, nil
		}
		lastErr = err
		if errors.Is(err, errCodexAuthMissing) {
			return codexAuthFile{}, err
		}
		if attempt+1 < codexAuthReadAttempts {
			// The Codex CLI and the sidecar can rewrite auth.json while requests
			// are selecting accounts. Retry invalid or incomplete content as a
			// file-write race, but do not retry a file that is simply absent:
			// empty onboarding slots are expected and must classify quickly.
			time.Sleep(codexAuthReadRetryDelay)
		}
	}
	return codexAuthFile{}, lastErr
}

func readCodexAuthFileOnce(path, accountID string) (codexAuthFile, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return codexAuthFile{}, fmt.Errorf("codex auth is missing for account %s: %w", accountID, errCodexAuthMissing)
		}
		return codexAuthFile{}, fmt.Errorf("read codex auth for account %s: %w", accountID, err)
	}
	var auth codexAuthFile
	if err := json.Unmarshal(data, &auth); err != nil {
		return codexAuthFile{}, fmt.Errorf("codex auth is invalid for account %s", accountID)
	}
	if auth.Tokens == nil || strings.TrimSpace(auth.Tokens.AccessToken) == "" {
		return codexAuthFile{}, fmt.Errorf("codex access token is missing for account %s", accountID)
	}
	return auth, nil
}

func codexAuthInfoFromFile(auth codexAuthFile) codexAuthInfo {
	info := codexAuthInfo{AccessToken: strings.TrimSpace(auth.Tokens.AccessToken)}
	if auth.Tokens.AccountID != nil {
		info.AccountID = strings.TrimSpace(*auth.Tokens.AccountID)
	}
	if claims := jwtPayload(auth.Tokens.IDToken); claims != nil {
		info.Email = claimString(claims, "email")
		info.OrganizationName = organizationNameFromMap(claims)
		info.PlanLimit = planLimitFromMap(claims)
		if profile, _ := claims["https://api.openai.com/profile"].(map[string]any); profile != nil {
			if info.Email == "" {
				info.Email = claimString(profile, "email")
			}
			if info.PlanLimit == "" {
				info.PlanLimit = planLimitFromMap(profile)
			}
		}
		if authClaims, _ := claims["https://api.openai.com/auth"].(map[string]any); authClaims != nil {
			if info.AccountID == "" {
				info.AccountID = claimString(authClaims, "chatgpt_account_id")
			}
			if organizationName := organizationNameFromMap(authClaims); organizationName != "" {
				info.OrganizationName = organizationName
			}
			info.PlanType = claimString(authClaims, "chatgpt_plan_type")
			if info.PlanLimit == "" {
				info.PlanLimit = cleanPlanLimit(info.PlanType)
			}
			if info.PlanLimit == "" {
				info.PlanLimit = planLimitFromMap(authClaims)
			}
			if fedramp, ok := authClaims["chatgpt_account_is_fedramp"].(bool); ok {
				info.FedRAMP = fedramp
			}
		}
	}
	return info
}

func (a *app) cliproxyAuthPath(accountID string) string {
	return filepath.Join(a.dataDir, "cliproxy", "auths", accountID+".json")
}

func (a *app) syncCliproxyAuth(item account, force bool) error {
	if !a.usesCliproxySidecar(item) {
		return nil
	}
	lock := a.codexAuthLock(item.ID)
	lock.Lock()
	defer lock.Unlock()
	return a.syncCliproxyAuthLocked(item, force)
}

func (a *app) syncCliproxyAuthLocked(item account, force bool) error {
	path := a.cliproxyAuthPath(item.ID)
	if !force {
		if _, err := os.Stat(path); err == nil {
			return nil
		} else if !errors.Is(err, os.ErrNotExist) {
			return fmt.Errorf("inspect cliproxy auth for account %s: %w", item.ID, err)
		}
	}
	source, err := a.readCodexAuthFile(item)
	if err != nil {
		return markAccountAuthError(err)
	}
	info := codexAuthInfoFromFile(source)
	accountID := info.AccountID
	if accountID == "" {
		accountID = item.AccountID
	}
	record := cliproxyCodexAuthFile{
		Type:             "codex",
		Email:            normalizeEmail(chooseString(info.Email, item.Email)),
		IDToken:          source.Tokens.IDToken,
		AccessToken:      source.Tokens.AccessToken,
		RefreshToken:     source.Tokens.RefreshToken,
		AccountID:        accountID,
		OrganizationName: cleanOrganizationName(chooseString(info.OrganizationName, item.OrganizationName)),
		Prefix:           cliproxyAccountPrefix(item.ID),
		PlanType:         normalizePlanType(chooseString(info.PlanType, item.PlanType)),
		PlanLimit:        cleanPlanLimit(chooseString(info.PlanLimit, item.PlanLimit)),
	}
	if source.LastRefresh != nil {
		record.LastRefresh = source.LastRefresh.UTC().Format(time.RFC3339)
	}
	if expiry, ok := jwtExpiry(source.Tokens.AccessToken); ok {
		record.Expire = expiry.UTC().Format(time.RFC3339)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return fmt.Errorf("create cliproxy auth directory: %w", err)
	}
	if err := writeJSONAtomic(path, record); err != nil {
		return fmt.Errorf("write cliproxy auth for account %s: %w", item.ID, err)
	}
	return nil
}

func (a *app) updateCliproxyAuthMetadata(item account) error {
	if !a.usesCliproxySidecar(item) {
		return nil
	}
	lock := a.codexAuthLock(item.ID)
	lock.Lock()
	defer lock.Unlock()
	path := a.cliproxyAuthPath(item.ID)
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return a.syncCliproxyAuthLocked(item, false)
	}
	if err != nil {
		return fmt.Errorf("read cliproxy auth for account %s: %w", item.ID, err)
	}
	var record cliproxyCodexAuthFile
	if err := json.Unmarshal(data, &record); err != nil {
		return fmt.Errorf("decode cliproxy auth for account %s: %w", item.ID, err)
	}
	if record.Type == "" {
		record.Type = "codex"
	}
	if item.Email != "" {
		record.Email = normalizeEmail(item.Email)
	}
	if item.AccountID != "" {
		record.AccountID = item.AccountID
	}
	if item.OrganizationName != "" || !organizationScopedPlan(item.PlanType) {
		record.OrganizationName = cleanOrganizationName(item.OrganizationName)
	}
	if item.PlanType != "" {
		record.PlanType = normalizePlanType(item.PlanType)
	}
	if item.PlanLimit != "" || normalizePlanType(item.PlanType) == "pro" {
		record.PlanLimit = cleanPlanLimit(item.PlanLimit)
	}
	record.Prefix = cliproxyAccountPrefix(item.ID)
	if err := writeJSONAtomic(path, record); err != nil {
		return fmt.Errorf("write cliproxy auth metadata for account %s: %w", item.ID, err)
	}
	return nil
}

func (a *app) cliproxyCodexAuth(item account) (codexAuthInfo, error) {
	if err := a.syncCliproxyAuth(item, false); err != nil {
		return codexAuthInfo{}, err
	}
	path := a.cliproxyAuthPath(item.ID)
	var record cliproxyCodexAuthFile
	var err error
	for attempt := 0; attempt < 3; attempt++ {
		data, readErr := os.ReadFile(path)
		if readErr == nil {
			err = json.Unmarshal(data, &record)
		} else {
			err = readErr
		}
		if err == nil && strings.TrimSpace(record.AccessToken) != "" {
			break
		}
		time.Sleep(25 * time.Millisecond)
	}
	if err != nil || strings.TrimSpace(record.AccessToken) == "" {
		return codexAuthInfo{}, fmt.Errorf("cliproxy auth is unavailable for account %s", item.ID)
	}
	info := codexAuthInfo{AccessToken: strings.TrimSpace(record.AccessToken), AccountID: strings.TrimSpace(record.AccountID), Email: normalizeEmail(record.Email), PlanType: normalizePlanType(record.PlanType), PlanLimit: cleanPlanLimit(record.PlanLimit)}
	if claims := jwtPayload(record.IDToken); claims != nil {
		if info.Email == "" {
			info.Email = normalizeEmail(claimString(claims, "email"))
		}
		if organizationName := organizationNameFromMap(claims); organizationName != "" {
			info.OrganizationName = organizationName
		}
		if info.PlanLimit == "" {
			info.PlanLimit = planLimitFromMap(claims)
		}
		if authClaims, _ := claims["https://api.openai.com/auth"].(map[string]any); authClaims != nil {
			if info.AccountID == "" {
				info.AccountID = claimString(authClaims, "chatgpt_account_id")
			}
			if organizationName := organizationNameFromMap(authClaims); organizationName != "" {
				info.OrganizationName = organizationName
			}
			if info.PlanType == "" || info.PlanType == "unknown" {
				info.PlanType = normalizePlanType(claimString(authClaims, "chatgpt_plan_type"))
			}
			if info.PlanLimit == "" {
				info.PlanLimit = cleanPlanLimit(claimString(authClaims, "chatgpt_plan_type"))
			}
			if info.PlanLimit == "" {
				info.PlanLimit = planLimitFromMap(authClaims)
			}
			if fedramp, ok := authClaims["chatgpt_account_is_fedramp"].(bool); ok {
				info.FedRAMP = fedramp
			}
		}
	}
	return info, nil
}

func (a *app) refreshCodexAuthIfNeeded(item account) error {
	return a.refreshCodexAuthIfNeededContext(context.Background(), item)
}

func (a *app) refreshCodexAuthIfNeededContext(ctx context.Context, item account) error {
	home := a.accountCodexHome(item.ID)
	path := filepath.Join(home, "auth.json")
	auth, err := a.readCodexAuthFile(item)
	if err != nil {
		return err
	}
	if auth.Tokens.RefreshToken == "" {
		return nil
	}
	expiry, ok := jwtExpiry(auth.Tokens.AccessToken)
	if !ok || time.Until(expiry) > codexTokenRefreshWindow {
		return nil
	}
	refreshed, err := a.requestCodexTokenRefreshContext(ctx, auth.Tokens.RefreshToken)
	if err != nil {
		return err
	}
	if refreshed.IDToken != "" {
		auth.Tokens.IDToken = refreshed.IDToken
	}
	if refreshed.AccessToken != "" {
		auth.Tokens.AccessToken = refreshed.AccessToken
	}
	if refreshed.RefreshToken != "" {
		auth.Tokens.RefreshToken = refreshed.RefreshToken
	}
	now := time.Now().UTC()
	auth.LastRefresh = &now
	if err := writeJSONAtomic(path, auth); err != nil {
		return fmt.Errorf("persist refreshed codex auth: %w", err)
	}
	return nil
}

func (a *app) refreshedCodexAuth(item account) (codexAuthInfo, error) {
	return a.refreshedCodexAuthContext(context.Background(), item)
}

func (a *app) refreshedCodexAuthContext(ctx context.Context, item account) (codexAuthInfo, error) {
	lock := a.codexAuthLock(item.ID)
	lock.Lock()
	defer lock.Unlock()
	if err := a.refreshCodexAuthIfNeededContext(ctx, item); err != nil {
		return codexAuthInfo{}, err
	}
	return a.codexAuth(item)
}

func (a *app) activeCodexAuthContext(ctx context.Context, item account) (codexAuthInfo, error) {
	if a.usesCliproxySidecar(item) {
		// In sidecar mode, inference uses the sidecar-owned auth copy. Pool must
		// not refresh the original Codex CLI auth file on the request path, or the
		// two processes can race refresh-token rotation.
		return a.cliproxyCodexAuth(item)
	}
	return a.refreshedCodexAuthContext(ctx, item)
}

func (a *app) codexAuthLock(accountID string) *sync.Mutex {
	a.authLockMu.Lock()
	defer a.authLockMu.Unlock()
	if a.authLocks == nil {
		a.authLocks = map[string]*sync.Mutex{}
	}
	lock := a.authLocks[accountID]
	if lock == nil {
		lock = &sync.Mutex{}
		a.authLocks[accountID] = lock
	}
	return lock
}

func (a *app) requestCodexTokenRefresh(refreshToken string) (codexRefreshResponse, error) {
	return a.requestCodexTokenRefreshContext(context.Background(), refreshToken)
}

func (a *app) requestCodexTokenRefreshContext(ctx context.Context, refreshToken string) (codexRefreshResponse, error) {
	request := map[string]string{
		"client_id":     envOr("CODEX_APP_SERVER_LOGIN_CLIENT_ID", codexOAuthClientIDDefault),
		"grant_type":    "refresh_token",
		"refresh_token": refreshToken,
	}
	body, err := json.Marshal(request)
	if err != nil {
		return codexRefreshResponse{}, err
	}
	endpoint := envOr("CODEX_REFRESH_TOKEN_URL_OVERRIDE", codexRefreshURLDefault)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, strings.NewReader(string(body)))
	if err != nil {
		return codexRefreshResponse{}, err
	}
	req.Header.Set("Content-Type", "application/json")
	response, err := a.client.Do(req)
	if err != nil {
		return codexRefreshResponse{}, fmt.Errorf("refresh codex token: %w", err)
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		err := fmt.Errorf("refresh codex token failed with status %d", response.StatusCode)
		if oauthRefreshAuthFailureStatus(response.StatusCode) {
			return codexRefreshResponse{}, markAccountAuthError(err)
		}
		return codexRefreshResponse{}, err
	}
	var refreshed codexRefreshResponse
	if err := json.NewDecoder(io.LimitReader(response.Body, maxRequestBody)).Decode(&refreshed); err != nil {
		return codexRefreshResponse{}, fmt.Errorf("decode refreshed codex token: %w", err)
	}
	return refreshed, nil
}

func (a *app) codexUsageURL() string {
	if value := strings.TrimSpace(os.Getenv("CODEX_POOL_CODEX_USAGE_URL")); value != "" {
		return value
	}
	return strings.TrimRight(a.codexBaseURL, "/") + "/wham/usage"
}

func (a *app) codexSubscriptionsURL() string {
	if value := strings.TrimSpace(os.Getenv("CODEX_POOL_CODEX_SUBSCRIPTIONS_URL")); value != "" {
		return value
	}
	return strings.TrimRight(a.codexBaseURL, "/") + "/subscriptions"
}

func (a *app) codexAccountsCheckURL() string {
	if value := strings.TrimSpace(os.Getenv("CODEX_POOL_CODEX_ACCOUNTS_CHECK_URL")); value != "" {
		return value
	}
	return strings.TrimRight(a.codexBaseURL, "/") + "/accounts/check/v4-2023-04-27"
}

func (a *app) fetchCodexSubscriptionMetadata(ctx context.Context, auth codexAuthInfo) (codexSubscriptionMetadata, error) {
	metadata, err := a.fetchCodexAccountCheckMetadata(ctx, auth)
	if err != nil {
		if auth.AccountID == "" {
			return codexSubscriptionMetadata{}, err
		}
		return a.fetchCodexSubscriptionsMetadata(ctx, auth, auth.AccountID)
	}
	accountID := chooseString(metadata.AccountID, auth.AccountID)
	if accountID == "" {
		return metadata, nil
	}
	if metadata.PlanType == "" || metadata.PlanLimit == "" {
		if subscriptions, err := a.fetchCodexSubscriptionsMetadata(ctx, auth, accountID); err == nil {
			metadata.AccountID = chooseString(metadata.AccountID, subscriptions.AccountID)
			metadata.OrganizationName = cleanOrganizationName(chooseString(metadata.OrganizationName, subscriptions.OrganizationName))
			if subscriptions.PlanType != "" {
				metadata.PlanType = subscriptions.PlanType
			}
			if subscriptions.PlanLimit != "" {
				metadata.PlanLimit = subscriptions.PlanLimit
			}
		}
	}
	return metadata, nil
}

func (a *app) fetchCodexAccountCheckMetadata(ctx context.Context, auth codexAuthInfo) (codexSubscriptionMetadata, error) {
	endpoint := a.codexAccountsCheckURL()
	parsed, err := url.Parse(endpoint)
	if err == nil {
		query := parsed.Query()
		query.Set("timezone_offset_min", strconv.Itoa(chatGPTTimezoneOffsetMinutes()))
		parsed.RawQuery = query.Encode()
		endpoint = parsed.String()
	}
	payload, err := a.fetchCodexMetadataPayload(ctx, auth, endpoint, "/backend-api/accounts/check/v4-2023-04-27", "")
	if err != nil {
		payload, err = a.fetchCodexMetadataPayloadWithNode(ctx, auth, endpoint, "/backend-api/accounts/check/v4-2023-04-27", "")
		if err != nil {
			return codexSubscriptionMetadata{}, err
		}
	}
	metadata, ok := subscriptionMetadataFromValue(payload, auth.AccountID)
	if !ok {
		return codexSubscriptionMetadata{}, errors.New("accounts/check did not include account metadata")
	}
	return metadata, nil
}

func (a *app) fetchCodexSubscriptionsMetadata(ctx context.Context, auth codexAuthInfo, accountID string) (codexSubscriptionMetadata, error) {
	endpoint := a.codexSubscriptionsURL()
	parsed, err := url.Parse(endpoint)
	if err == nil {
		query := parsed.Query()
		query.Set("account_id", accountID)
		parsed.RawQuery = query.Encode()
		endpoint = parsed.String()
	}
	payload, err := a.fetchCodexMetadataPayload(ctx, auth, endpoint, "/backend-api/subscriptions", "")
	if err != nil {
		return codexSubscriptionMetadata{}, err
	}
	metadata := subscriptionMetadataFromMap(payload)
	metadata.AccountID = accountID
	return metadata, nil
}

func (a *app) fetchCodexMetadataPayload(ctx context.Context, auth codexAuthInfo, endpoint, targetPath, chatGPTAccountID string) (map[string]any, error) {
	if strings.TrimSpace(auth.AccessToken) == "" {
		return nil, errors.New("codex access token is missing")
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return nil, err
	}
	applyCodexWebMetadataHeaders(request, auth, targetPath, chatGPTAccountID)
	response, err := a.client.Do(request)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return nil, fmt.Errorf("subscription metadata returned status %d", response.StatusCode)
	}
	var payload map[string]any
	if err := json.NewDecoder(io.LimitReader(response.Body, maxRequestBody)).Decode(&payload); err != nil {
		return nil, err
	}
	return payload, nil
}

const codexMetadataNodeFetchScript = `
const chunks = [];
process.stdin.on("data", (chunk) => chunks.push(chunk));
process.stdin.on("end", async () => {
  try {
    const input = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    const headers = {
      "Authorization": "Bearer " + input.accessToken,
      "Accept": "application/json",
      "Accept-Language": "en-US,en;q=0.9",
      "OAI-Language": "en-US",
      "Origin": "https://chatgpt.com",
      "Referer": "https://chatgpt.com/",
      "Sec-Fetch-Dest": "empty",
      "Sec-Fetch-Mode": "cors",
      "Sec-Fetch-Site": "same-origin",
      "User-Agent": input.userAgent
    };
    if (input.targetPath) {
      headers["X-OpenAI-Target-Path"] = input.targetPath;
      headers["X-OpenAI-Target-Route"] = input.targetPath;
    }
    if (input.chatGPTAccountID) headers["ChatGPT-Account-Id"] = input.chatGPTAccountID;
    if (input.fedramp) headers["X-OpenAI-Fedramp"] = "true";
    const response = await fetch(input.endpoint, { headers });
    const body = await response.text();
    if (!response.ok) {
      console.error("metadata fetch returned status " + response.status);
      process.exit(2);
    }
    process.stdout.write(body);
  } catch (error) {
    console.error(error && error.message ? error.message : String(error));
    process.exit(1);
  }
});
`

func (a *app) fetchCodexMetadataPayloadWithNode(ctx context.Context, auth codexAuthInfo, endpoint, targetPath, chatGPTAccountID string) (map[string]any, error) {
	if strings.TrimSpace(auth.AccessToken) == "" {
		return nil, errors.New("codex access token is missing")
	}
	input, err := json.Marshal(map[string]any{
		"accessToken":      auth.AccessToken,
		"endpoint":         endpoint,
		"targetPath":       targetPath,
		"chatGPTAccountID": chatGPTAccountID,
		"fedramp":          auth.FedRAMP,
		"userAgent":        chatGPTWebUserAgent,
	})
	if err != nil {
		return nil, err
	}
	cmd := exec.CommandContext(ctx, "node", "-e", codexMetadataNodeFetchScript)
	cmd.Stdin = strings.NewReader(string(input))
	output, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("node metadata fetch failed: %w", err)
	}
	var payload map[string]any
	if err := json.NewDecoder(io.LimitReader(bytes.NewReader(output), maxRequestBody)).Decode(&payload); err != nil {
		return nil, err
	}
	return payload, nil
}

func applyCodexWebMetadataHeaders(request *http.Request, auth codexAuthInfo, targetPath, chatGPTAccountID string) {
	request.Header.Set("Authorization", "Bearer "+auth.AccessToken)
	request.Header.Set("Accept", "application/json")
	request.Header.Set("Accept-Language", "en-US,en;q=0.9")
	request.Header.Set("OAI-Language", "en-US")
	request.Header.Set("Origin", "https://chatgpt.com")
	request.Header.Set("Referer", chatGPTWebReferer)
	request.Header.Set("Sec-Fetch-Dest", "empty")
	request.Header.Set("Sec-Fetch-Mode", "cors")
	request.Header.Set("Sec-Fetch-Site", "same-origin")
	request.Header.Set("User-Agent", chatGPTWebUserAgent)
	if targetPath != "" {
		request.Header.Set("X-OpenAI-Target-Path", targetPath)
		request.Header.Set("X-OpenAI-Target-Route", targetPath)
	}
	if chatGPTAccountID != "" {
		request.Header.Set("ChatGPT-Account-Id", chatGPTAccountID)
	}
	if auth.FedRAMP {
		request.Header.Set("X-OpenAI-Fedramp", "true")
	}
}

func chatGPTTimezoneOffsetMinutes() int {
	_, offsetSeconds := time.Now().Zone()
	return -offsetSeconds / 60
}

func subscriptionMetadataFromMap(values map[string]any) codexSubscriptionMetadata {
	rawPlan := firstMetadataString(values, "plan_type", "planType", "subscription_plan", "subscriptionPlan", "plan_name", "planName", "sku", "sku_name", "product", "product_name")
	metadata := codexSubscriptionMetadata{
		AccountID:        firstMetadataString(values, "account_id", "accountId", "id", "chatgpt_account_id", "workspace_id", "workspaceId"),
		OrganizationName: cleanOrganizationName(organizationNameFromMap(values)),
		PlanType:         normalizePlanType(rawPlan),
		PlanLimit:        cleanPlanLimit(rawPlan),
	}
	if metadata.PlanLimit == "" {
		metadata.PlanLimit = planLimitFromMap(values)
	}
	if metadata.PlanLimit == "" {
		metadata.PlanLimit = planLimitFromSubscriptionPlan(rawPlan)
	}
	if metadata.PlanType == "unknown" {
		metadata.PlanType = ""
	}
	return metadata
}

type subscriptionAccountRecord struct {
	key  string
	node map[string]any
}

func subscriptionMetadataFromValue(value any, preferredAccountID string) (codexSubscriptionMetadata, bool) {
	records := collectSubscriptionAccountRecords(value)
	if len(records) == 0 {
		values, _ := value.(map[string]any)
		if values == nil {
			return codexSubscriptionMetadata{}, false
		}
		return subscriptionMetadataFromMap(values), true
	}
	preferredAccountID = strings.TrimSpace(preferredAccountID)
	selected := records[0]
	if preferredAccountID != "" {
		for _, record := range records {
			metadata := subscriptionMetadataFromRecord(record.node)
			if metadata.AccountID == preferredAccountID || strings.TrimSpace(record.key) == preferredAccountID {
				selected = record
				break
			}
		}
	} else if orderingKey := firstAccountOrderingKey(value); orderingKey != "" {
		for _, record := range records {
			metadata := subscriptionMetadataFromRecord(record.node)
			if metadata.AccountID == orderingKey || strings.TrimSpace(record.key) == orderingKey {
				selected = record
				break
			}
		}
	}
	metadata := subscriptionMetadataFromRecord(selected.node)
	if metadata.AccountID == "" {
		metadata.AccountID = strings.TrimSpace(selected.key)
	}
	return metadata, true
}

func firstAccountOrderingKey(value any) string {
	values, _ := value.(map[string]any)
	if values == nil {
		return ""
	}
	ordering, _ := values["account_ordering"].([]any)
	if len(ordering) == 0 {
		return ""
	}
	first, _ := ordering[0].(string)
	return strings.TrimSpace(first)
}

func collectSubscriptionAccountRecords(value any) []subscriptionAccountRecord {
	switch typed := value.(type) {
	case []any:
		records := make([]subscriptionAccountRecord, 0, len(typed))
		for _, item := range typed {
			if values, ok := item.(map[string]any); ok {
				records = append(records, subscriptionAccountRecord{node: values})
			}
		}
		return records
	case map[string]any:
		for _, key := range []string{"accounts", "account_items", "items", "data"} {
			if records := collectSubscriptionAccountRecordsFromField(typed[key]); len(records) > 0 {
				return records
			}
		}
		if _, ok := typed["account"].(map[string]any); ok {
			return []subscriptionAccountRecord{{node: typed}}
		}
		if firstMetadataString(typed, "account_id", "accountId", "id", "chatgpt_account_id", "workspace_id", "workspaceId") != "" {
			return []subscriptionAccountRecord{{node: typed}}
		}
	}
	return nil
}

func collectSubscriptionAccountRecordsFromField(value any) []subscriptionAccountRecord {
	switch typed := value.(type) {
	case []any:
		records := make([]subscriptionAccountRecord, 0, len(typed))
		for _, item := range typed {
			if values, ok := item.(map[string]any); ok {
				records = append(records, subscriptionAccountRecord{node: values})
			}
		}
		return records
	case map[string]any:
		records := make([]subscriptionAccountRecord, 0, len(typed))
		for key, item := range typed {
			if values, ok := item.(map[string]any); ok {
				records = append(records, subscriptionAccountRecord{key: key, node: values})
			}
		}
		return records
	default:
		return nil
	}
}

func subscriptionMetadataFromRecord(record map[string]any) codexSubscriptionMetadata {
	accountRecord, _ := record["account"].(map[string]any)
	if accountRecord == nil {
		accountRecord = record
	}
	entitlement, _ := record["entitlement"].(map[string]any)
	rawPlan := ""
	if entitlement != nil {
		rawPlan = firstMetadataString(entitlement, "subscription_plan", "subscriptionPlan", "plan_type", "planType", "plan_name", "planName", "sku", "sku_name", "product", "product_name")
	}
	if rawPlan == "" {
		rawPlan = firstMetadataString(accountRecord, "plan_type", "planType", "subscription_plan", "subscriptionPlan", "plan_name", "planName", "sku", "sku_name", "product", "product_name")
	}
	organizationName := cleanOrganizationName(organizationNameFromMap(accountRecord))
	if organizationName == "" {
		organizationName = cleanOrganizationName(organizationNameFromMap(record))
	}
	metadata := codexSubscriptionMetadata{
		AccountID:        firstMetadataString(accountRecord, "account_id", "accountId", "id", "chatgpt_account_id", "workspace_id", "workspaceId"),
		OrganizationName: organizationName,
		PlanType:         normalizePlanType(rawPlan),
		PlanLimit:        cleanPlanLimit(rawPlan),
	}
	if metadata.PlanLimit == "" && entitlement != nil {
		metadata.PlanLimit = planLimitFromMap(entitlement)
	}
	if metadata.PlanLimit == "" {
		metadata.PlanLimit = planLimitFromMap(accountRecord)
	}
	if metadata.PlanLimit == "" {
		metadata.PlanLimit = planLimitFromSubscriptionPlan(rawPlan)
	}
	if metadata.PlanType == "unknown" {
		metadata.PlanType = ""
	}
	return metadata
}

func planLimitFromSubscriptionPlan(value string) string {
	compact := compactPlanLimitText(value)
	switch compact {
	case "chatgptpro", "chatgptproplan", "proplan":
		return "20x"
	default:
		return ""
	}
}

func firstMetadataString(values map[string]any, keys ...string) string {
	if values == nil {
		return ""
	}
	for _, key := range keys {
		if value := claimString(values, key); value != "" {
			return value
		}
	}
	for _, key := range []string{"plan", "subscription", "subscriptions", "entitlement", "billing", "account", "accounts", "items", "data", "quota", "rate_limit", "limits", "codex"} {
		nested, _ := values[key].(map[string]any)
		if value := firstMetadataString(nested, keys...); value != "" {
			return value
		}
		items, _ := values[key].([]any)
		for _, item := range items {
			nested, _ := item.(map[string]any)
			if value := firstMetadataString(nested, keys...); value != "" {
				return value
			}
		}
	}
	return ""
}

func (a *app) refreshAccountQuota(ctx context.Context, accountID string) (quotaSnapshot, error) {
	ctx, cancel := context.WithTimeout(ctx, quotaRefreshTimeout)
	defer cancel()

	a.mu.RLock()
	item := a.accountLocked(accountID)
	if item == nil {
		a.mu.RUnlock()
		return quotaSnapshot{}, errors.New("account not found")
	}
	accountCopy := *item
	a.mu.RUnlock()

	if !isCodexDeviceAuth(accountCopy) {
		return quotaSnapshot{AccountID: accountID}, errors.New("quota refresh is only available for Codex device-auth accounts")
	}
	auth, err := a.activeCodexAuthContext(ctx, accountCopy)
	if err != nil {
		if ctx.Err() != nil {
			return quotaSnapshot{}, errors.New("quota refresh cancelled")
		}
		code := "token_refresh_unavailable"
		if errors.Is(err, errAccountAuthFailed) {
			code = "account_auth_failed"
		}
		a.saveQuotaError(accountID, code, "refresh codex token failed")
		return quotaSnapshot{}, errors.New("refresh codex token failed")
	}
	if auth.AccountID == "" {
		auth.AccountID = accountCopy.AccountID
	}

	request, err := http.NewRequestWithContext(ctx, http.MethodGet, a.codexUsageURL(), nil)
	if err != nil {
		a.saveQuotaError(accountID, "request_invalid", "quota request could not be created")
		return quotaSnapshot{}, errors.New("quota request could not be created")
	}
	request.Header.Set("Authorization", "Bearer "+auth.AccessToken)
	request.Header.Set("Accept", "application/json")
	if auth.AccountID != "" {
		request.Header.Set("ChatGPT-Account-Id", auth.AccountID)
	}
	if auth.FedRAMP {
		request.Header.Set("X-OpenAI-Fedramp", "true")
	}
	response, err := a.client.Do(request)
	if err != nil {
		if ctx.Err() != nil {
			return quotaSnapshot{}, errors.New("quota refresh cancelled")
		}
		a.saveQuotaError(accountID, "request_failed", "quota request failed")
		return quotaSnapshot{}, errors.New("quota request failed")
	}
	defer response.Body.Close()
	body, err := io.ReadAll(io.LimitReader(response.Body, maxRequestBody))
	if err != nil {
		if ctx.Err() != nil {
			return quotaSnapshot{}, errors.New("quota refresh cancelled")
		}
		a.saveQuotaError(accountID, "read_failed", "quota response could not be read")
		return quotaSnapshot{}, errors.New("quota response could not be read")
	}
	if response.StatusCode < 200 || response.StatusCode > 299 {
		code := extractUpstreamErrorCode(body)
		message := fmt.Sprintf("quota API returned status %d", response.StatusCode)
		if code != "" {
			message += " [" + code + "]"
		}
		if upstreamAuthFailureStatus(response.StatusCode) && !quotaErrorBlocksRouting(&quotaErrorInfo{Code: code}) {
			code = "account_auth_failed"
		}
		a.saveQuotaError(accountID, codeOr(code, "upstream_status"), message)
		return quotaSnapshot{}, errors.New(message)
	}
	var usage codexUsageResponse
	if err := json.Unmarshal(body, &usage); err != nil {
		a.saveQuotaError(accountID, "decode_failed", "quota response JSON could not be decoded")
		return quotaSnapshot{}, errors.New("quota response JSON could not be decoded")
	}
	var usageFields map[string]any
	_ = json.Unmarshal(body, &usageFields)
	quota := quotaFromUsage(usage, time.Now().UTC())
	remaining := remainingQuotaHint(quota)
	hasQuotaWindow := quota.Hourly.Present || quota.Weekly.Present
	now := time.Now().UTC()
	planRaw := chooseString(usage.PlanType, usage.SubscriptionPlan)
	plan := normalizePlanType(chooseString(planRaw, accountCopy.PlanType))
	planLimit := cleanPlanLimit(planRaw)
	if planLimit == "" {
		planLimit = planLimitFromMap(usageFields)
	}
	if planLimit == "" {
		planLimit = cleanPlanLimit(chooseString(auth.PlanLimit, accountCopy.PlanLimit))
	}
	organizationName := cleanOrganizationName(organizationNameFromMap(usageFields))
	if organizationName == "" && auth.OrganizationName != "" {
		organizationName = cleanOrganizationName(auth.OrganizationName)
	}
	if (plan == "pro" && planLimit == "") || organizationName == "" || organizationScopedPlan(plan) {
		if metadata, err := a.fetchCodexSubscriptionMetadata(ctx, auth); err == nil {
			if metadata.AccountID != "" {
				auth.AccountID = metadata.AccountID
			}
			if metadata.PlanType != "" && metadata.PlanType != "unknown" {
				plan = metadata.PlanType
			}
			if planLimit == "" {
				planLimit = metadata.PlanLimit
			}
			if metadata.OrganizationName != "" && organizationScopedPlan(plan) {
				organizationName = metadata.OrganizationName
			} else if organizationName == "" {
				organizationName = metadata.OrganizationName
			}
		}
	}
	if plan == "pro" && planLimit == "" {
		planLimit = "20x"
	}
	if !organizationScopedPlan(plan) {
		organizationName = ""
	}
	snapshot := quotaSnapshot{AccountID: accountID, OrganizationName: organizationName, PlanType: plan, PlanLimit: planLimit, Quota: &quota, UsageUpdatedAt: now}

	a.mu.Lock()
	if a.state.Quotas == nil {
		a.state.Quotas = map[string]quotaSnapshot{}
	}
	item = a.accountLocked(accountID)
	if item == nil {
		a.mu.Unlock()
		return quotaSnapshot{}, errors.New("account no longer exists")
	}
	if planRaw != "" {
		item.PlanType = plan
		item.PlanRank = planRank(plan)
	}
	if planLimit != "" {
		item.PlanLimit = planLimit
	}
	if auth.Email != "" {
		item.Email = normalizeEmail(auth.Email)
	}
	if auth.AccountID != "" {
		item.AccountID = auth.AccountID
	}
	if organizationScopedPlan(plan) {
		item.OrganizationName = organizationName
	} else {
		item.OrganizationName = ""
	}
	item.Label = accountDisplayName(*item)
	if hasQuotaWindow {
		item.RemainingQuota = &remaining
	} else {
		item.RemainingQuota = nil
	}
	item.UpdatedAt = now
	a.state.Quotas[accountID] = snapshot
	if err := a.saveLocked(); err != nil {
		a.mu.Unlock()
		return quotaSnapshot{}, err
	}
	sidecarAccount := *item
	syncSidecar := a.usesCliproxySidecar(sidecarAccount)
	a.mu.Unlock()
	if syncSidecar {
		if err := a.updateCliproxyAuthMetadata(sidecarAccount); err != nil {
			if a.logger != nil {
				a.logger.Printf("cliproxy auth metadata update skipped for %s after quota metadata update: %s", accountID, err)
			}
		}
	}
	return snapshot, nil
}

func (a *app) saveQuotaError(accountID, code, message string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.accountLocked(accountID) == nil {
		return
	}
	if a.state.Quotas == nil {
		a.state.Quotas = map[string]quotaSnapshot{}
	}
	prior := a.state.Quotas[accountID]
	prior.AccountID = accountID
	prior.QuotaError = &quotaErrorInfo{Code: code, Message: message, Timestamp: time.Now().UTC()}
	a.state.Quotas[accountID] = prior
	_ = a.saveLocked()
}

func extractUpstreamErrorCode(body []byte) string {
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		return ""
	}
	if code := nestedString(payload, "detail", "code"); code != "" {
		return sanitizedErrorCode(code)
	}
	if code := nestedString(payload, "error", "code"); code != "" {
		return sanitizedErrorCode(code)
	}
	if code, _ := payload["code"].(string); code != "" {
		return sanitizedErrorCode(code)
	}
	return ""
}

func sanitizedErrorCode(value string) string {
	value = strings.TrimSpace(value)
	if value == "" || len(value) > 64 {
		return ""
	}
	for _, character := range value {
		if !(character >= 'a' && character <= 'z') &&
			!(character >= 'A' && character <= 'Z') &&
			!(character >= '0' && character <= '9') &&
			character != '_' && character != '-' && character != '.' {
			return ""
		}
	}
	return value
}

func nestedString(payload map[string]any, objectName, key string) string {
	object, _ := payload[objectName].(map[string]any)
	if object == nil {
		return ""
	}
	value, _ := object[key].(string)
	return value
}

func quotaFromUsage(usage codexUsageResponse, now time.Time) accountQuota {
	var primary, secondary *codexWindowInfo
	if usage.RateLimit != nil {
		primary = usage.RateLimit.PrimaryWindow
		secondary = usage.RateLimit.SecondaryWindow
	}
	quota := accountQuota{
		Hourly: normalizeQuotaWindow(primary, now),
		Weekly: normalizeQuotaWindow(secondary, now),
	}
	if usage.RateLimit != nil && !quota.Hourly.Present && !quota.Weekly.Present {
		limitReached := usage.RateLimit.LimitReached != nil && *usage.RateLimit.LimitReached
		notAllowed := usage.RateLimit.Allowed != nil && !*usage.RateLimit.Allowed
		if limitReached || notAllowed {
			quota.Hourly = quotaWindow{Percentage: 0, Present: true}
		}
	}
	return quota
}

func normalizeQuotaWindow(window *codexWindowInfo, now time.Time) quotaWindow {
	if window == nil {
		return quotaWindow{Percentage: 100, Present: false}
	}
	used := 0
	if window.UsedPercent != nil {
		used = clampInt(*window.UsedPercent, 0, 100)
	}
	var resetAt *int64
	if window.ResetAt != nil {
		value := *window.ResetAt
		resetAt = &value
	} else if window.ResetAfterSeconds != nil && *window.ResetAfterSeconds >= 0 {
		value := now.Add(time.Duration(*window.ResetAfterSeconds) * time.Second).Unix()
		resetAt = &value
	}
	var windowMinutes *int64
	if window.LimitWindowSeconds != nil && *window.LimitWindowSeconds > 0 {
		value := (*window.LimitWindowSeconds + 59) / 60
		windowMinutes = &value
	}
	return quotaWindow{Percentage: 100 - used, ResetAt: resetAt, WindowMinutes: windowMinutes, Present: true}
}

func remainingQuotaHint(quota accountQuota) int {
	values := make([]int, 0, 2)
	if quota.Hourly.Present {
		values = append(values, quota.Hourly.Percentage)
	}
	if quota.Weekly.Present {
		values = append(values, quota.Weekly.Percentage)
	}
	if len(values) == 0 {
		return 100
	}
	result := values[0]
	for _, value := range values[1:] {
		if value < result {
			result = value
		}
	}
	return clampInt(result, 0, 100)
}

func clampInt(value, min, max int) int {
	if value < min {
		return min
	}
	if value > max {
		return max
	}
	return value
}

func codeOr(value, fallback string) string {
	if value == "" {
		return fallback
	}
	return value
}

func chooseString(value, fallback string) string {
	if strings.TrimSpace(value) != "" {
		return value
	}
	return fallback
}

func jwtPayload(token string) map[string]any {
	parts := strings.Split(token, ".")
	if len(parts) < 3 || parts[1] == "" {
		return nil
	}
	data, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return nil
	}
	var claims map[string]any
	if err := json.Unmarshal(data, &claims); err != nil {
		return nil
	}
	return claims
}

func jwtExpiry(token string) (time.Time, bool) {
	claims := jwtPayload(token)
	if claims == nil {
		return time.Time{}, false
	}
	switch exp := claims["exp"].(type) {
	case float64:
		return time.Unix(int64(exp), 0), true
	case int64:
		return time.Unix(exp, 0), true
	case json.Number:
		value, err := exp.Int64()
		if err == nil {
			return time.Unix(value, 0), true
		}
	}
	return time.Time{}, false
}

func claimString(claims map[string]any, name string) string {
	if value, ok := claims[name].(string); ok {
		return value
	}
	return ""
}

func (a *app) startLoginJobLocked(item account) loginJob {
	if a.jobs == nil {
		a.jobs = map[string]*loginJob{}
	}
	if a.loginCancels == nil {
		a.loginCancels = map[string]context.CancelFunc{}
	}
	for _, job := range a.jobs {
		if job.AccountID != item.ID {
			continue
		}
		switch job.Status {
		case "running", "waiting_for_user", "finalizing":
			return *job
		}
	}
	home := a.accountCodexHome(item.ID)
	jobID := fmt.Sprintf("job-login-%s-%d-%s", item.ID, time.Now().Unix(), randomID())
	ctx, cancel := context.WithCancel(context.Background())
	now := time.Now().UTC()
	job := &loginJob{
		ID:        jobID,
		Type:      "account_login",
		Status:    "running",
		AccountID: item.ID,
		Message:   "Starting Codex device auth login",
		StartedAt: now,
		UpdatedAt: now,
	}
	a.jobs[jobID] = job
	a.loginCancels[jobID] = cancel
	go a.runLoginJob(ctx, jobID, item.ID, home)
	return *job
}

func (a *app) runLoginJob(ctx context.Context, jobID, accountID, codexHome string) {
	if err := os.MkdirAll(codexHome, 0o700); err != nil {
		a.finishLoginJob(jobID, "failed", "", "", fmt.Sprintf("create CODEX_HOME: %v", err))
		return
	}
	cmd := exec.CommandContext(ctx, "codex", "-c", "cli_auth_credentials_store=\"file\"", "login", "--device-auth")
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		if cmd.Process == nil {
			return nil
		}
		err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		if err == syscall.ESRCH {
			return nil
		}
		return err
	}
	cmd.WaitDelay = 5 * time.Second
	cmd.Env = codexLoginEnv(codexHome)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		a.finishLoginJob(jobID, "failed", "", "", fmt.Sprintf("capture stdout: %v", err))
		return
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		a.finishLoginJob(jobID, "failed", "", "", fmt.Sprintf("capture stderr: %v", err))
		return
	}
	var output strings.Builder
	var outputMu sync.Mutex
	consume := func(reader io.Reader, done chan<- struct{}) {
		defer close(done)
		scanner := bufio.NewScanner(reader)
		scanner.Buffer(make([]byte, 1024), 256*1024)
		for scanner.Scan() {
			line := stripANSI(scanner.Text())
			outputMu.Lock()
			output.WriteString(line)
			output.WriteByte('\n')
			text := output.String()
			outputMu.Unlock()
			verificationURL, userCode := parseDeviceAuthPrompt(text)
			if verificationURL != "" || userCode != "" {
				a.updateLoginJob(jobID, "waiting_for_user", verificationURL, userCode, "Open the verification URL and enter the code.")
			}
		}
	}
	if err := cmd.Start(); err != nil {
		a.finishLoginJob(jobID, "failed", "", "", fmt.Sprintf("start Codex CLI: %v", err))
		return
	}
	stdoutDone := make(chan struct{})
	stderrDone := make(chan struct{})
	go consume(stdout, stdoutDone)
	go consume(stderr, stderrDone)
	err = cmd.Wait()
	<-stdoutDone
	<-stderrDone
	outputMu.Lock()
	text := output.String()
	outputMu.Unlock()
	verificationURL, userCode := parseDeviceAuthPrompt(text)
	if ctx.Err() != nil || a.loginJobStatus(jobID) == "cancelled" {
		a.finishLoginJob(jobID, "cancelled", verificationURL, userCode, "Codex device auth login cancelled")
		return
	}
	if err != nil {
		message := strings.TrimSpace(text)
		if message == "" {
			message = err.Error()
		}
		a.finishLoginJob(jobID, "failed", verificationURL, userCode, redactLoginOutput(message))
		return
	}
	refreshQuotaAccountID := ""
	continueLogin := false
	activateAfterFinalize := false
	sidecarAccount := account{}
	syncSidecar := false
	a.mu.Lock()
	if job := a.jobs[jobID]; job != nil && job.Status != "cancelled" {
		now := time.Now().UTC()
		job.Status = "finalizing"
		job.Message = "Refreshing account quota"
		job.Error = ""
		job.VerificationURL = verificationURL
		job.UserCode = userCode
		job.UpdatedAt = now
		continueLogin = true
		if item := a.accountLocked(accountID); item != nil {
			if auth, err := a.codexAuth(*item); err == nil {
				activateAfterFinalize = item.PendingPoolActivation
				if auth.Email != "" {
					item.Email = auth.Email
				}
				if auth.AccountID != "" {
					item.AccountID = auth.AccountID
				}
				if auth.OrganizationName != "" {
					item.OrganizationName = cleanOrganizationName(auth.OrganizationName)
				}
				if auth.PlanType != "" {
					item.PlanType = normalizePlanType(auth.PlanType)
					item.PlanRank = planRank(item.PlanType)
				}
				if auth.PlanLimit != "" {
					item.PlanLimit = cleanPlanLimit(auth.PlanLimit)
				}
				a.clearAccountRuntimeStateLocked(accountID)
				item.Label = accountDisplayName(*item)
				item.LastLoginAt = time.Now().UTC()
				item.UpdatedAt = item.LastLoginAt
				_ = a.saveLocked()
				refreshQuotaAccountID = accountID
				if a.usesCliproxySidecar(*item) {
					sidecarAccount = *item
					syncSidecar = true
				}
			}
		}
	}
	a.mu.Unlock()
	if !continueLogin || a.loginJobStatus(jobID) == "cancelled" {
		return
	}
	if syncSidecar {
		if err := a.syncCliproxyAuth(sidecarAccount, true); err != nil {
			a.finishLoginJob(jobID, "failed", verificationURL, userCode, "Unable to prepare the account gateway")
			return
		}
	}
	if refreshQuotaAccountID != "" {
		if _, err := a.refreshAccountQuota(ctx, refreshQuotaAccountID); err != nil && ctx.Err() == nil {
			a.logger.Printf("quota refresh after login skipped for %s: %s", refreshQuotaAccountID, err)
		}
	}
	if ctx.Err() != nil || a.loginJobStatus(jobID) == "cancelled" {
		a.finishLoginJob(jobID, "cancelled", verificationURL, userCode, "Codex device auth login cancelled")
		return
	}
	if activateAfterFinalize {
		a.activatePendingDeviceAuthAccount(accountID)
	}
	a.finishLoginJob(jobID, "completed", verificationURL, userCode, "Codex device auth login completed")
}

func (a *app) activatePendingDeviceAuthAccount(accountID string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	item := a.accountLocked(accountID)
	if item == nil || !item.PendingPoolActivation {
		return
	}
	item.Enabled = true
	item.InPool = true
	item.PendingPoolActivation = false
	item.UpdatedAt = time.Now().UTC()
	_ = a.saveLocked()
}

func (a *app) loginJobStatus(jobID string) string {
	a.mu.RLock()
	defer a.mu.RUnlock()
	if job := a.jobs[jobID]; job != nil {
		return job.Status
	}
	return ""
}

func (a *app) cancelLoginJob(jobID string) (context.CancelFunc, loginJob, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	job := a.jobs[jobID]
	if job == nil {
		return nil, loginJob{}, errors.New("job not found")
	}
	switch job.Status {
	case "completed", "failed", "cancelled":
		return nil, *job, nil
	}
	now := time.Now().UTC()
	job.Status = "cancelled"
	job.Message = "Codex device auth login cancelled"
	job.Error = ""
	job.CompletedAt = now
	job.UpdatedAt = now
	cancel := a.loginCancels[jobID]
	delete(a.loginCancels, jobID)
	return cancel, *job, nil
}

func (a *app) cancelLoginJobsForAccountLocked(accountID string) {
	now := time.Now().UTC()
	for jobID, job := range a.jobs {
		if job.AccountID != accountID {
			continue
		}
		switch job.Status {
		case "completed", "failed", "cancelled":
			continue
		}
		job.Status = "cancelled"
		job.Message = "Codex device auth login cancelled because the account was removed"
		job.Error = ""
		job.CompletedAt = now
		job.UpdatedAt = now
		if cancel := a.loginCancels[jobID]; cancel != nil {
			cancel()
		}
		delete(a.loginCancels, jobID)
	}
}

func (a *app) updateLoginJob(jobID, status, verificationURL, userCode, message string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	job := a.jobs[jobID]
	if job == nil {
		return
	}
	if job.Status == "cancelled" || job.Status == "completed" || job.Status == "failed" {
		return
	}
	now := time.Now().UTC()
	job.Status = status
	if verificationURL != "" {
		job.VerificationURL = verificationURL
	}
	if userCode != "" {
		job.UserCode = userCode
		if job.CodeExpiresAt.IsZero() {
			job.CodeExpiresAt = now.Add(15 * time.Minute)
		}
	}
	job.Message = message
	job.UpdatedAt = now
}

func (a *app) finishLoginJob(jobID, status, verificationURL, userCode, message string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	job := a.jobs[jobID]
	if job == nil {
		return
	}
	if job.Status == "cancelled" && status != "cancelled" {
		return
	}
	now := time.Now().UTC()
	job.Status = status
	job.Message = message
	if status == "failed" {
		job.Error = message
	} else {
		job.Error = ""
	}
	if verificationURL != "" {
		job.VerificationURL = verificationURL
	}
	if userCode != "" {
		job.UserCode = userCode
		if job.CodeExpiresAt.IsZero() {
			job.CodeExpiresAt = now.Add(15 * time.Minute)
		}
	}
	job.CompletedAt = now
	job.UpdatedAt = now
	delete(a.loginCancels, jobID)
}

func codexLoginEnv(codexHome string) []string {
	accountHome := filepath.Dir(codexHome)
	env := []string{
		"CODEX_HOME=" + codexHome,
		"HOME=" + accountHome,
		"PATH=" + envOr("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
	}
	for _, name := range []string{
		"HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY",
		"http_proxy", "https_proxy", "no_proxy",
		"SSL_CERT_FILE", "SSL_CERT_DIR", "CODEX_CA_CERTIFICATE",
		"CODEX_REFRESH_TOKEN_URL_OVERRIDE", "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
	} {
		if value := os.Getenv(name); value != "" {
			env = append(env, name+"="+value)
		}
	}
	return env
}

var (
	ansiPattern       = regexp.MustCompile(`\x1b\[[0-9;]*[A-Za-z]`)
	deviceURLPattern  = regexp.MustCompile(`https?://[^\s]+`)
	deviceCodePattern = regexp.MustCompile(`[A-Z0-9]{4,}(-[A-Z0-9]{4,})+`)
	secretishPattern  = regexp.MustCompile(`(?i)(access[_ -]?token|refresh[_ -]?token|id[_ -]?token|authorization|bearer|api[_ -]?key|cookie|session[_ -]?cookie|CODEX_POOL_[A-Z0-9_]+)`)
	jwtLikePattern    = regexp.MustCompile(`eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}`)
)

func stripANSI(value string) string {
	return ansiPattern.ReplaceAllString(value, "")
}

func parseDeviceAuthPrompt(output string) (string, string) {
	var verificationURL, userCode string
	for _, line := range strings.Split(output, "\n") {
		line = strings.TrimSpace(line)
		if verificationURL == "" {
			if match := deviceURLPattern.FindString(line); match != "" {
				verificationURL = strings.TrimRight(match, ".,)")
			}
		}
		if userCode == "" {
			userCode = deviceCodePattern.FindString(line)
		}
	}
	return verificationURL, userCode
}

func redactLoginOutput(value string) string {
	lines := strings.Split(value, "\n")
	for i, line := range lines {
		if secretishPattern.MatchString(line) || jwtLikePattern.MatchString(line) {
			lines[i] = "[REDACTED]"
		}
	}
	const max = 1200
	result := strings.TrimSpace(strings.Join(lines, "\n"))
	if len(result) > max {
		return result[:max] + "..."
	}
	return result
}

func retryAfter(value string) time.Duration {
	return retryAfterOrDefault(value, time.Minute)
}

func retryAfterOrDefault(value string, fallback time.Duration) time.Duration {
	seconds, err := strconv.Atoi(value)
	if err == nil && seconds > 0 {
		return time.Duration(seconds) * time.Second
	}
	return fallback
}
func activeCooldowns(values []cooldown, now time.Time) []cooldown {
	result := make([]cooldown, 0, len(values))
	for _, item := range values {
		if item.NextRetryAt.After(now) {
			result = append(result, item)
		}
	}
	return result
}
func (a *app) dashboardSummaryLocked(now time.Time) map[string]int {
	summary := map[string]int{"total": len(a.config.Accounts), "ready": 0, "low": 0, "cooldown": 0, "error": 0, "missing_auth": 0, "duplicate": 0}
	for _, item := range a.config.Accounts {
		status, _ := a.accountStatusLocked(item, now)
		if _, ok := summary[status]; ok {
			summary[status]++
		}
	}
	return summary
}
func (a *app) publicDashboardSummaryLocked(now time.Time) map[string]int {
	summary := map[string]int{"total": len(a.config.Accounts), "ready": 0, "low": 0, "cooldown": 0, "standby": 0, "unavailable": 0}
	for _, item := range a.config.Accounts {
		status, _ := a.accountStatusLocked(item, now)
		switch status {
		case "ready":
			summary["ready"]++
		case "low":
			summary["low"]++
		case "cooldown":
			summary["cooldown"]++
		case "standby", "duplicate":
			summary["standby"]++
		default:
			summary["unavailable"]++
		}
	}
	return summary
}

// accountActiveLocked reports whether the account served a successful request
// within accountActiveWindow. It reads the passively-recorded LastSuccessAt and
// never touches upstream, so it is a cheap, side-effect-free "currently being
// consumed" signal.
func accountActiveLocked(health accountHealth, now time.Time) bool {
	return !health.LastSuccessAt.IsZero() && now.Sub(health.LastSuccessAt) < accountActiveWindow
}

// promptCacheStatsForAccountLocked aggregates the recorded prompt-cache usage
// across every model for one account. CachedTokens/InputTokens is the prompt
// (KV) cache hit rate; the numbers come straight from upstream usage payloads
// recorded on each success, so reading them adds no upstream calls.
func (a *app) promptCacheStatsForAccountLocked(accountID string) (input, cached, requests uint64) {
	for _, stat := range a.state.PromptCache {
		if stat.AccountID == accountID {
			input += stat.InputTokens
			cached += stat.CachedTokens
			requests += stat.RequestCount
		}
	}
	return input, cached, requests
}

// promptCacheWindowLocked returns the pool-wide prompt-cache totals accumulated
// since the last reset (PromptCache minus PromptCacheBaseline) so the dashboard
// can report a hit rate over fresh traffic, plus the reset timestamp. With no
// reset yet the baseline is empty and the window equals the lifetime totals.
func (a *app) promptCacheWindowLocked() map[string]any {
	return a.promptCacheWindowFilteredLocked("", a.state.PromptCacheResetAt)
}

// promptCacheWindowForAccountLocked is the per-account equivalent: it sums only
// that account's keys and reports the per-account reset time when set, otherwise
// the pool-wide reset time.
func (a *app) promptCacheWindowForAccountLocked(accountID string) map[string]any {
	resetAt := a.state.PromptCacheResetAt
	if at, ok := a.state.PromptCacheResetAtByAccount[accountID]; ok {
		resetAt = at
	}
	return a.promptCacheWindowFilteredLocked(accountID, resetAt)
}

// promptCacheWindowFilteredLocked computes the since-baseline deltas. An empty
// accountID aggregates every account.
func (a *app) promptCacheWindowFilteredLocked(accountID string, resetAt time.Time) map[string]any {
	var input, cached, requests, cold uint64
	var parentAffinityHits, parentAffinityFallbacks, lineageFailovers uint64
	agents := map[string]map[string]uint64{
		"main":     {"inputTokens": 0, "cachedTokens": 0, "requestCount": 0, "coldRequestCount": 0},
		"subagent": {"inputTokens": 0, "cachedTokens": 0, "requestCount": 0, "coldRequestCount": 0},
	}
	for key, stat := range a.state.PromptCache {
		if accountID != "" && stat.AccountID != accountID {
			continue
		}
		base := a.state.PromptCacheBaseline[key]
		inputDelta := subSat(stat.InputTokens, base.InputTokens)
		cachedDelta := subSat(stat.CachedTokens, base.CachedTokens)
		requestDelta := subSat(stat.RequestCount, base.RequestCount)
		coldDelta := subSat(stat.ColdRequestCount, base.ColdRequestCount)
		input += inputDelta
		cached += cachedDelta
		requests += requestDelta
		cold += coldDelta
		parentAffinityHits += subSat(stat.ParentAffinityHitCount, base.ParentAffinityHitCount)
		parentAffinityFallbacks += subSat(stat.ParentAffinityFallbackCount, base.ParentAffinityFallbackCount)
		lineageFailovers += subSat(stat.LineageFailoverCount, base.LineageFailoverCount)
		agentKind := stat.AgentKind
		if agentKind != "subagent" {
			agentKind = "main"
		}
		agents[agentKind]["inputTokens"] += inputDelta
		agents[agentKind]["cachedTokens"] += cachedDelta
		agents[agentKind]["requestCount"] += requestDelta
		agents[agentKind]["coldRequestCount"] += coldDelta
	}
	return map[string]any{
		"inputTokens": input, "cachedTokens": cached, "requestCount": requests, "coldRequestCount": cold,
		"main": agents["main"], "subagent": agents["subagent"],
		"parentAffinityHitCount": parentAffinityHits, "parentAffinityFallbackCount": parentAffinityFallbacks,
		"lineageFailoverCount": lineageFailovers, "resetAt": resetAt,
	}
}

// subSat is a saturating subtraction; a baseline can briefly exceed the live
// counter if an account's stats were cleared after the snapshot, so clamp to 0
// instead of underflowing the unsigned counter.
func subSat(value, base uint64) uint64 {
	if value < base {
		return 0
	}
	return value - base
}

// resetPromptCacheWindowLocked snapshots the current totals as the new baseline,
// starting a fresh pool-wide window without discarding lifetime totals. It also
// clears any per-account overrides so every account shares this reset time.
func (a *app) resetPromptCacheWindowLocked(now time.Time) {
	baseline := make(map[string]promptCacheStat, len(a.state.PromptCache))
	for key, stat := range a.state.PromptCache {
		baseline[key] = stat
	}
	a.state.PromptCacheBaseline = baseline
	a.state.PromptCacheResetAt = now
	a.state.PromptCacheResetAtByAccount = nil
}

// resetPromptCacheWindowForAccountLocked rebaselines only one account's keys and
// records a per-account reset time, leaving every other account's window intact.
func (a *app) resetPromptCacheWindowForAccountLocked(accountID string, now time.Time) {
	if a.state.PromptCacheBaseline == nil {
		a.state.PromptCacheBaseline = map[string]promptCacheStat{}
	}
	for key, stat := range a.state.PromptCache {
		if stat.AccountID == accountID {
			a.state.PromptCacheBaseline[key] = stat
		}
	}
	if a.state.PromptCacheResetAtByAccount == nil {
		a.state.PromptCacheResetAtByAccount = map[string]time.Time{}
	}
	a.state.PromptCacheResetAtByAccount[accountID] = now
}

func (a *app) accountHealthItemLocked(item account, now time.Time) map[string]any {
	cooldowns := activeCooldowns(a.state.Cooldowns[item.ID], now)
	status, reason := a.accountStatusLocked(item, now)
	health := a.state.Health[item.ID]
	quota := a.state.Quotas[item.ID]
	cacheInput, cacheCached, cacheRequests := a.promptCacheStatsForAccountLocked(item.ID)
	return map[string]any{"accountId": item.ID, "available": status == "ready" || status == "low", "status": status, "statusReason": reason, "cooldowns": cooldowns, "lastSuccessAt": health.LastSuccessAt, "lastFailureAt": health.LastFailureAt, "lastFailureReason": health.LastFailureReason, "consecutiveFailure": health.ConsecutiveFailure, "active": accountActiveLocked(health, now), "activeRouteCount": a.activeRouteCountLocked(item.ID, now), "cacheInputTokens": cacheInput, "cacheCachedTokens": cacheCached, "cacheRequestCount": cacheRequests, "cacheWindow": a.promptCacheWindowForAccountLocked(item.ID), "remainingQuota": item.RemainingQuota, "quota": quota.Quota, "usageUpdatedAt": quota.UsageUpdatedAt, "quotaError": quota.QuotaError}
}

func (a *app) activeRouteCountLocked(accountID string, now time.Time) int {
	count := 0
	for _, route := range a.state.StickySessions {
		if route.AccountID == accountID && !a.stickySessionExpiredLocked(route, now) {
			count++
		}
	}
	return count
}

func (a *app) currentAccountStatusLocked(item account, index int, now time.Time) map[string]any {
	status, reason := a.accountStatusLocked(item, now)
	quota := a.state.Quotas[item.ID]
	displayItem := item
	if quota.OrganizationName != "" {
		displayItem.OrganizationName = quota.OrganizationName
	}
	if quota.PlanType != "" {
		displayItem.PlanType = quota.PlanType
		displayItem.PlanRank = planRank(quota.PlanType)
	}
	if quota.PlanLimit != "" {
		displayItem.PlanLimit = quota.PlanLimit
	}
	remainingQuota := displayItem.RemainingQuota
	if remainingQuota == nil && quota.Quota != nil {
		remaining := remainingQuotaHint(*quota.Quota)
		remainingQuota = &remaining
	}
	metadata := credentialMetadata(displayItem)
	return map[string]any{
		"label":              currentAccountDisplayName(displayItem, index),
		"displayName":        currentAccountDisplayName(displayItem, index),
		"credentialMetadata": metadata,
		"email":              metadata["email"],
		"organizationName":   metadata["organizationName"],
		"planType":           metadata["planType"],
		"planLimit":          metadata["planLimit"],
		"planDisplayName":    metadata["planDisplayName"],
		"planRank":           metadata["planRank"],
		"status":             status,
		"statusReason":       reason,
		"available":          status == "ready" || status == "low",
		"remainingQuota":     remainingQuota,
		"quota":              quota.Quota,
		"usageUpdatedAt":     quota.UsageUpdatedAt,
		"quotaError":         quota.QuotaError,
	}
}

func (a *app) accountStatusLocked(item account, now time.Time) (string, string) {
	if !item.Enabled {
		return "disabled", "Account is disabled"
	}
	if !item.InPool {
		return "standby", "Account is not in the pool"
	}
	if isCodexDeviceAuth(item) {
		// Disabled/out-of-pool device-auth slots must be cheap to render. Only
		// read auth.json for accounts that can actually participate in routing;
		// staging slots otherwise serialize dashboard/status requests behind
		// repeated missing-auth retries while the global state lock is held.
		if _, err := a.codexAuth(item); err != nil {
			return "missing_auth", "Device auth login is required"
		}
	}
	// Check duplicate identity before cooldown/quota so the operator sees the
	// structural reason this slot is not routable. The primary slot owns the
	// upstream identity; runtime cooldown and quota are evaluated on that slot.
	if primaryID := a.duplicateUpstreamAccountPrimaryLocked(item, now); primaryID != "" {
		return "duplicate", "Duplicate upstream account; routing uses " + primaryID
	}
	if cooldowns := activeCooldowns(a.state.Cooldowns[item.ID], now); len(cooldowns) > 0 {
		return "cooldown", cooldowns[0].Reason
	}
	// Last failure and transient quota polling failures are diagnostic history,
	// not availability gates. Routing only excludes active cooldowns, missing
	// auth, exhausted quota, and explicit credential errors; otherwise a usage API
	// outage can hide a healthy Pro fallback and create a false 503 exactly when a
	// non-Pro account runs out.
	quotaSnapshot := a.state.Quotas[item.ID]
	if quotaErrorBlocksRouting(quotaSnapshot.QuotaError) {
		reason := "Quota refresh failed"
		if quotaSnapshot.QuotaError.Code != "" {
			reason += ": " + quotaSnapshot.QuotaError.Code
		}
		return "error", reason
	}
	if quotaSnapshot.Quota != nil {
		remaining := remainingQuotaHint(*quotaSnapshot.Quota)
		if remaining <= 20 {
			return "low", "Quota window is at or below 20%"
		}
	}
	if item.RemainingQuota != nil && *item.RemainingQuota <= 20 {
		return "low", "Remaining quota is at or below 20%"
	}
	return "ready", "Ready"
}
func publicAccounts(values []account) []map[string]any {
	result := make([]map[string]any, 0, len(values))
	for index, item := range values {
		result = append(result, publicAccount(item, index))
	}
	return result
}
func publicAccount(item account, index int) map[string]any {
	displayName := managementCredentialDisplayName(item)
	metadata := credentialMetadata(item)
	return map[string]any{"id": item.ID, "label": displayName, "displayName": displayName, "credentialMetadata": metadata, "email": metadata["email"], "organizationName": metadata["organizationName"], "planType": metadata["planType"], "planLimit": metadata["planLimit"], "planDisplayName": metadata["planDisplayName"], "planRank": metadata["planRank"], "authType": item.AuthType, "enabled": item.Enabled, "inPool": item.InPool, "priority": item.Priority, "remainingQuota": item.RemainingQuota, "allowedModels": item.AllowedModels, "excludedModels": item.ExcludedModels, "wireApi": item.WireAPI, "hasUpstreamApiKey": item.UpstreamAPIKey != "", "lastLoginAt": item.LastLoginAt}
}

func (a *app) publicDashboardAccountLocked(item account, index int, now time.Time) map[string]any {
	status, _ := a.accountStatusLocked(item, now)
	quota := a.state.Quotas[item.ID]
	displayItem := item
	if quota.OrganizationName != "" {
		displayItem.OrganizationName = quota.OrganizationName
	}
	if quota.PlanType != "" {
		displayItem.PlanType = quota.PlanType
		displayItem.PlanRank = planRank(quota.PlanType)
	}
	if quota.PlanLimit != "" {
		displayItem.PlanLimit = quota.PlanLimit
	}
	statusTone, statusLabel := publicDashboardStatus(status)
	remainingQuota := displayItem.RemainingQuota
	if remainingQuota == nil && quota.Quota != nil {
		remaining := remainingQuotaHint(*quota.Quota)
		remainingQuota = &remaining
	}
	cacheInput, cacheCached, cacheRequests := a.promptCacheStatsForAccountLocked(item.ID)
	return map[string]any{
		"displayName":       publicDashboardAccountLabel(displayItem, index),
		"detail":            publicDashboardAccountDetail(displayItem),
		"statusTone":        statusTone,
		"statusLabel":       statusLabel,
		"poolLabel":         publicPoolLabel(item),
		"poolRef":           a.publicAccountRefLocked(item.ID),
		"poolAction":        publicPoolAction(item),
		"poolActionLabel":   publicPoolActionLabel(item),
		"remainingQuota":    remainingQuota,
		"quota":             quota.Quota,
		"quotaUnavailable":  quota.QuotaError != nil,
		"active":            accountActiveLocked(a.state.Health[item.ID], now),
		"cacheInputTokens":  cacheInput,
		"cacheCachedTokens": cacheCached,
		"cacheRequestCount": cacheRequests,
		"cacheWindow":       a.promptCacheWindowForAccountLocked(item.ID),
	}
}

func publicDashboardAccountDetail(item account) string {
	if normalizePlanType(item.PlanType) == "unknown" {
		return ""
	}
	return accountPlanDisplayName(item, false)
}

func publicDashboardStatus(status string) (string, string) {
	switch status {
	case "ready":
		return "ready", "Ready"
	case "low":
		return "low", "Limited"
	case "cooldown":
		return "cooldown", "Cooling down"
	case "standby":
		return "standby", "Out of pool"
	case "duplicate":
		// Public mode groups duplicate slots with standby visually, but keeps the
		// label explicit so users do not interpret the slot as extra capacity.
		return "standby", "Duplicate"
	default:
		return "error", "Unavailable"
	}
}

func publicPoolLabel(item account) string {
	if !item.Enabled {
		return "Unavailable"
	}
	if !item.InPool {
		return "Out of pool"
	}
	return "In pool"
}

func publicPoolAction(item account) string {
	if item.InPool {
		return "pool-remove"
	}
	return "pool-add"
}

func publicPoolActionLabel(item account) string {
	if item.InPool {
		return "Leave pool"
	}
	return "Join pool"
}

func maskedPublicEmail(value string) string {
	value = normalizeEmail(value)
	parts := strings.Split(value, "@")
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return ""
	}
	local := parts[0]
	if len(local) <= 2 {
		local = local[:1] + "***"
	} else if len(local) <= 4 {
		local = local[:2] + "***"
	} else {
		local = local[:2] + "***" + local[len(local)-2:]
	}
	return local + "@" + parts[1]
}

var emailInDisplayPattern = regexp.MustCompile(`[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}`)

func publicOrganizationName(value string) string {
	value = cleanOrganizationName(value)
	if value == "" {
		return ""
	}
	return emailInDisplayPattern.ReplaceAllStringFunc(value, maskedPublicEmail)
}

func effectiveOrganizationName(item account) string {
	return cleanOrganizationName(item.OrganizationName)
}

func credentialMetadata(item account) map[string]any {
	return map[string]any{
		"email":            maskedPublicEmail(item.Email),
		"organizationName": publicOrganizationName(effectiveOrganizationName(item)),
		"planType":         normalizePlanType(item.PlanType),
		"planLimit":        cleanPlanLimit(item.PlanLimit),
		"planDisplayName":  accountPlanDisplayName(item, false),
		"planRank":         item.PlanRank,
	}
}

func managementCredentialDisplayName(item account) string {
	if email := maskedPublicEmail(item.Email); email != "" {
		return email
	}
	if label := credentialLabel(item); label != "" {
		return label
	}
	if strings.TrimSpace(item.ID) != "" {
		return item.ID
	}
	return "Credential"
}

func publicCredentialDisplayName(item account, index int) string {
	if email := maskedPublicEmail(item.Email); email != "" {
		return email
	}
	if label := credentialLabel(item); label != "" && label != item.ID {
		return label
	}
	if index >= 0 {
		return fmt.Sprintf("Credential %d", index+1)
	}
	return "Credential"
}

func credentialLabel(item account) string {
	label := strings.TrimSpace(item.Label)
	if label == "" || metadataDerivedAccountLabel(item, label) {
		return ""
	}
	return label
}

func metadataDerivedAccountLabel(item account, label string) bool {
	label = strings.TrimSpace(label)
	if label == "" {
		return false
	}
	if strings.Contains(label, "@") {
		return true
	}
	generated := []string{
		legacyAccountDisplayName(item),
		accountPlanDisplayName(item, false),
		accountPlanDisplayName(item, true),
		planDisplayName(item.PlanType),
		planDisplayName(item.PlanType) + " account",
	}
	for _, value := range generated {
		if value != "" && strings.EqualFold(label, value) {
			return true
		}
	}
	return false
}

func legacyAccountDisplayName(item account) string {
	plan := accountPlanDisplayName(item, false)
	email := strings.TrimSpace(item.Email)
	if email != "" && normalizePlanType(item.PlanType) != "unknown" {
		return fmt.Sprintf("%s · %s", email, plan)
	}
	if email != "" {
		return email
	}
	if normalizePlanType(item.PlanType) != "unknown" && organizationScopedPlan(item.PlanType) && effectiveOrganizationName(item) != "" {
		return plan
	}
	if normalizePlanType(item.PlanType) != "unknown" {
		return plan
	}
	return ""
}

func currentAccountDisplayName(item account, index int) string {
	return publicCredentialDisplayName(item, index)
}

func (a *app) publicAccountRefLocked(accountID string) string {
	key := a.sessionKey
	if len(key) == 0 {
		key = []byte("codex-pool-public-account-ref")
	}
	mac := hmac.New(sha256.New, key)
	_, _ = mac.Write([]byte("public-account:" + accountID))
	sum := mac.Sum(nil)
	return base64.RawURLEncoding.EncodeToString(sum[:16])
}

func (a *app) publicAccountRefMatchesLocked(accountID, ref string) bool {
	expected := a.publicAccountRefLocked(accountID)
	return subtle.ConstantTimeCompare([]byte(expected), []byte(ref)) == 1
}

func normalizeEmail(value string) string {
	return strings.ToLower(strings.TrimSpace(value))
}

func cleanOrganizationName(value string) string {
	value = strings.TrimSpace(value)
	if value == "" {
		return ""
	}
	var builder strings.Builder
	for _, char := range value {
		if unicode.IsControl(char) {
			continue
		}
		builder.WriteRune(char)
	}
	parts := strings.Fields(builder.String())
	if len(parts) == 0 {
		return ""
	}
	value = strings.Join(parts, " ")
	if len([]rune(value)) > 120 {
		runes := []rune(value)
		value = string(runes[:120])
	}
	return value
}

func organizationNameFromMap(values map[string]any) string {
	if values == nil {
		return ""
	}
	for _, key := range []string{"organization_name", "organization_display_name", "org_name", "org_display_name", "workspace_name", "workspace_display_name", "team_name", "team_display_name", "account_name", "account_display_name", "chatgpt_organization_name", "chatgpt_org_name", "chatgpt_workspace_name", "chatgpt_account_name"} {
		if value := cleanOrganizationName(claimString(values, key)); value != "" {
			return value
		}
	}
	if strings.EqualFold(claimString(values, "structure"), "workspace") {
		if value := organizationNameFromNestedMap(values); value != "" {
			return value
		}
	}
	for _, key := range []string{"organization", "org", "workspace", "team"} {
		nested, _ := values[key].(map[string]any)
		if value := organizationNameFromNestedMap(nested); value != "" {
			return value
		}
	}
	for _, key := range []string{"account", "accounts", "subscription", "subscriptions", "entitlement", "billing", "items", "data"} {
		nested, _ := values[key].(map[string]any)
		if value := organizationNameFromMap(nested); value != "" {
			return value
		}
		items, _ := values[key].([]any)
		for _, item := range items {
			nested, _ := item.(map[string]any)
			if value := organizationNameFromMap(nested); value != "" {
				return value
			}
		}
	}
	return ""
}

func organizationNameFromNestedMap(values map[string]any) string {
	if values == nil {
		return ""
	}
	for _, key := range []string{"display_name", "name", "title"} {
		if value := cleanOrganizationName(claimString(values, key)); value != "" {
			return value
		}
	}
	return ""
}

func cleanPlanLimit(value string) string {
	compact := compactPlanLimitText(value)
	if compact == "" {
		return ""
	}
	for _, limit := range []string{"20x", "10x", "5x"} {
		if compact == limit || strings.Contains(compact, limit) {
			return limit
		}
	}
	return ""
}

func compactPlanLimitText(value string) string {
	var builder strings.Builder
	for _, char := range strings.ToLower(strings.TrimSpace(value)) {
		if (char >= 'a' && char <= 'z') || (char >= '0' && char <= '9') {
			builder.WriteRune(char)
		}
	}
	return builder.String()
}

func planLimitFromMap(values map[string]any) string {
	if values == nil {
		return ""
	}
	for _, key := range []string{
		"plan_limit", "planLimit", "codex_plan_limit", "codexPlanLimit",
		"usage_tier", "usageTier", "quota_tier", "quotaTier",
		"rate_limit_tier", "rateLimitTier", "plan_tier", "planTier", "pro_tier", "proTier",
		"subscription_plan", "subscriptionPlan", "plan_type", "planType", "plan_name", "planName",
		"plan_display_name", "planDisplayName", "sku", "sku_name", "product", "product_name",
		"entitlement", "entitlement_name",
	} {
		if limit := planLimitFromValue(values[key]); limit != "" {
			return limit
		}
	}
	for _, key := range []string{
		"multiplier", "usage_multiplier", "usageMultiplier", "quota_multiplier", "quotaMultiplier",
		"rate_limit_multiplier", "rateLimitMultiplier", "codex_" + "rate_limit_multiplier", "codexRateLimitMultiplier",
	} {
		if limit := planLimitFromValue(values[key]); limit != "" {
			return limit
		}
	}
	for _, key := range []string{"plan", "subscription", "subscriptions", "entitlement", "account", "accounts", "items", "data", "billing", "quota", "rate_limit", "limits", "codex"} {
		if limit := planLimitFromValue(values[key]); limit != "" {
			return limit
		}
	}
	return ""
}

func planLimitFromValue(value any) string {
	switch typed := value.(type) {
	case string:
		return cleanPlanLimit(typed)
	case json.Number:
		if number, err := typed.Int64(); err == nil {
			return planLimitFromNumber(number)
		}
	case float64:
		number := int64(typed)
		if typed == float64(number) {
			return planLimitFromNumber(number)
		}
	case int:
		return planLimitFromNumber(int64(typed))
	case int64:
		return planLimitFromNumber(typed)
	case map[string]any:
		return planLimitFromMap(typed)
	case []any:
		for _, item := range typed {
			if limit := planLimitFromValue(item); limit != "" {
				return limit
			}
		}
	}
	return ""
}

func planLimitFromNumber(value int64) string {
	switch value {
	case 5, 10, 20:
		return strconv.FormatInt(value, 10) + "x"
	default:
		return ""
	}
}

func normalizePlanType(value string) string {
	value = strings.ToLower(strings.TrimSpace(value))
	value = strings.ReplaceAll(value, "-", "_")
	value = strings.ReplaceAll(value, " ", "_")
	switch value {
	case "free", "plus", "pro", "team", "business", "enterprise", "edu":
		return value
	case "chatgpt_plus":
		return "plus"
	case "chatgpt_pro":
		return "pro"
	case "chatgpt_team":
		return "team"
	case "chatgpt_business":
		return "business"
	case "chatgpt_enterprise":
		return "enterprise"
	case "chatgpt_edu":
		return "edu"
	default:
		if value == "" {
			return "unknown"
		}
		compact := compactPlanLimitText(value)
		for _, candidate := range []string{"enterprise", "business", "team", "plus", "pro", "free", "edu"} {
			if strings.Contains(compact, candidate) {
				return candidate
			}
		}
		return value
	}
}

func planRank(plan string) int {
	switch normalizePlanType(plan) {
	case "enterprise":
		return 500
	case "team", "business":
		return 400
	case "pro":
		return 300
	case "plus":
		return 200
	case "edu":
		return 150
	case "free":
		return 100
	default:
		return 0
	}
}

func planDisplayName(plan string) string {
	normalized := normalizePlanType(plan)
	switch normalized {
	case "free":
		return "Free"
	case "plus":
		return "Plus"
	case "pro":
		return "Pro"
	case "team":
		return "Team"
	case "business":
		return "Business"
	case "enterprise":
		return "Enterprise"
	case "edu":
		return "Edu"
	case "unknown":
		return "Unknown tier"
	default:
		return strings.ToUpper(normalized[:1]) + normalized[1:]
	}
}

func accountDisplayName(item account) string {
	return managementCredentialDisplayName(item)
}

func publicDashboardAccountLabel(item account, index int) string {
	return publicCredentialDisplayName(item, index)
}

func accountPlanDisplayName(item account, withAccountSuffix bool) string {
	plan := normalizePlanType(item.PlanType)
	name := planDisplayName(plan)
	if plan == "pro" {
		if limit := cleanPlanLimit(item.PlanLimit); limit != "" {
			name += " " + limit
		}
	}
	if withAccountSuffix {
		name += " account"
	}
	organizationName := publicOrganizationName(effectiveOrganizationName(item))
	if organizationName != "" && organizationScopedPlan(plan) {
		return name + " · " + organizationName
	}
	return name
}

func organizationScopedPlan(plan string) bool {
	switch normalizePlanType(plan) {
	case "team", "business", "enterprise", "edu":
		return true
	default:
		return false
	}
}

func generatedAccountIDBase(item account) string {
	if isCodexDeviceAuth(item) {
		return "acct-credential"
	}
	if item.AuthType == "provider_api_key" {
		return "acct-provider"
	}
	return "acct-account"
}

func (a *app) uniqueAccountIDLocked(base string) string {
	base = strings.Trim(base, "-")
	if !validAccountID(base) {
		base = "acct-account"
	}
	id := base
	for index := 2; a.accountLocked(id) != nil; index++ {
		suffix := fmt.Sprintf("-%d", index)
		prefix := base
		if len(prefix)+len(suffix) > 80 {
			prefix = strings.TrimRight(prefix[:80-len(suffix)], "-")
		}
		id = prefix + suffix
	}
	return id
}
func validAccountID(id string) bool {
	if id == "" || len(id) > 80 {
		return false
	}
	for _, c := range id {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_') {
			return false
		}
	}
	return true
}
func isLoopbackAddress(address string) bool {
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return false
	}
	if host == "localhost" {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}
func envOr(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

func envOrValue(value, fallback string) string {
	if value != "" {
		return value
	}
	return fallback
}

func codexGatewayModeFromEnv() (string, error) {
	mode := strings.ToLower(strings.TrimSpace(envOr("CODEX_POOL_CODEX_GATEWAY_MODE", "sidecar")))
	switch mode {
	case "sidecar", "direct":
		return mode, nil
	default:
		return "", errors.New("CODEX_POOL_CODEX_GATEWAY_MODE must be sidecar or direct")
	}
}

func promptCacheKeyModeFromEnv() (string, error) {
	mode := strings.ToLower(strings.TrimSpace(envOr("CODEX_POOL_PROMPT_CACHE_KEY_MODE", "auto")))
	switch mode {
	case "auto", "off", "passthrough":
		return mode, nil
	default:
		return "", errors.New("CODEX_POOL_PROMPT_CACHE_KEY_MODE must be auto, off, or passthrough")
	}
}

func promptCacheKeyScopeFromEnv() (string, error) {
	scope := strings.ToLower(strings.TrimSpace(envOr("CODEX_POOL_PROMPT_CACHE_KEY_SCOPE", "auto")))
	switch scope {
	case "auto", "conversation", "project", "user":
		return scope, nil
	default:
		return "", errors.New("CODEX_POOL_PROMPT_CACHE_KEY_SCOPE must be auto, conversation, project, or user")
	}
}

func promptCacheKeyPolicyFromEnv() (string, error) {
	policy := strings.ToLower(strings.TrimSpace(envOr("CODEX_POOL_PROMPT_CACHE_KEY_POLICY", "preserve")))
	switch policy {
	case "preserve", "lineage", "project", "user":
		return policy, nil
	default:
		return "", errors.New("CODEX_POOL_PROMPT_CACHE_KEY_POLICY must be preserve, lineage, project, or user")
	}
}

func promptCacheBucketsFromEnv() (int, error) {
	value := strings.TrimSpace(os.Getenv("CODEX_POOL_PROMPT_CACHE_BUCKETS"))
	if value == "" {
		return promptCacheBucketsDefault, nil
	}
	buckets, err := strconv.Atoi(value)
	if err != nil || buckets < 1 || buckets > 256 {
		return 0, errors.New("CODEX_POOL_PROMPT_CACHE_BUCKETS must be an integer between 1 and 256")
	}
	return buckets, nil
}

func promptCacheRetentionFromEnv() (string, error) {
	value := strings.ToLower(strings.TrimSpace(os.Getenv("CODEX_POOL_PROMPT_CACHE_RETENTION")))
	switch value {
	case "":
		// Default to extended retention so prompt (KV) caches survive the idle
		// gaps between conversation turns, which is the single biggest lever for
		// cache hit rate. Set "passthrough" to opt out and leave requests
		// untouched.
		return "24h", nil
	case "passthrough":
		return "", nil
	case "24h", "in_memory":
		return value, nil
	default:
		return "", errors.New("CODEX_POOL_PROMPT_CACHE_RETENTION must be empty, passthrough, 24h, or in_memory")
	}
}

func routingStrategyFromEnv() (string, error) {
	value := strings.ToLower(strings.TrimSpace(envOr("CODEX_POOL_ROUTING_STRATEGY", routingStrategyBalanced)))
	switch value {
	case routingStrategyBalanced, routingStrategyFailover:
		return value, nil
	default:
		return "", errors.New("CODEX_POOL_ROUTING_STRATEGY must be sticky_balanced or sticky_failover")
	}
}

func boolFromEnv(name string) (bool, error) {
	return boolFromEnvDefault(name, false)
}

func boolFromEnvDefault(name string, fallback bool) (bool, error) {
	value := strings.ToLower(strings.TrimSpace(os.Getenv(name)))
	switch value {
	case "":
		return fallback, nil
	case "1", "true", "yes", "on":
		return true, nil
	case "0", "false", "no", "off":
		return false, nil
	default:
		return false, fmt.Errorf("%s must be true or false", name)
	}
}

func sessionAffinityTTLFromEnv() (time.Duration, error) {
	raw := strings.TrimSpace(os.Getenv("CODEX_POOL_SESSION_AFFINITY_TTL_MS"))
	if raw == "" {
		return sessionAffinityTTLDefault, nil
	}
	millis, err := strconv.ParseInt(raw, 10, 64)
	if err != nil || millis <= 0 {
		return 0, fmt.Errorf("CODEX_POOL_SESSION_AFFINITY_TTL_MS must be a positive integer number of milliseconds")
	}
	return time.Duration(millis) * time.Millisecond, nil
}
func maxRetryAccountsFromEnv() (int, error) {
	raw := strings.TrimSpace(os.Getenv("CODEX_POOL_MAX_RETRY_ACCOUNTS"))
	if raw == "" {
		return 0, nil
	}
	value, err := strconv.Atoi(raw)
	if err != nil || value < 0 {
		return 0, fmt.Errorf("CODEX_POOL_MAX_RETRY_ACCOUNTS must be zero or a positive integer")
	}
	return value, nil
}
func chooseTime(value, fallback time.Time) time.Time {
	if value.IsZero() {
		return fallback
	}
	return value
}
func randomID() string {
	value := make([]byte, 16)
	if _, err := rand.Read(value); err != nil {
		panic(err)
	}
	return base64.RawURLEncoding.EncodeToString(value)
}

// PBKDF2 is implemented locally so image builds do not depend on a host or module registry.
func newPasswordHash(password string) (string, error) {
	salt := make([]byte, 16)
	if _, err := rand.Read(salt); err != nil {
		return "", err
	}
	const iterations = 600000
	derived := pbkdf2SHA256([]byte(password), salt, iterations, 32)
	return fmt.Sprintf("pbkdf2-sha256:%d:%s:%s", iterations, base64.RawStdEncoding.EncodeToString(salt), base64.RawStdEncoding.EncodeToString(derived)), nil
}

func validPasswordHash(value string) bool {
	_, _, _, ok := parsePasswordHash(value)
	return ok
}

func verifyPasswordHash(encoded, password string) bool {
	iterations, salt, expected, ok := parsePasswordHash(encoded)
	if !ok {
		return false
	}
	actual := pbkdf2SHA256([]byte(password), salt, iterations, len(expected))
	return subtle.ConstantTimeCompare(actual, expected) == 1
}

func parsePasswordHash(value string) (int, []byte, []byte, bool) {
	parts := strings.Split(value, ":")
	if len(parts) != 4 || parts[0] != "pbkdf2-sha256" {
		return 0, nil, nil, false
	}
	iterations, err := strconv.Atoi(parts[1])
	if err != nil || iterations < 100000 || iterations > 5000000 {
		return 0, nil, nil, false
	}
	salt, saltErr := base64.RawStdEncoding.DecodeString(parts[2])
	hash, hashErr := base64.RawStdEncoding.DecodeString(parts[3])
	if saltErr != nil || hashErr != nil || len(salt) < 16 || len(hash) != 32 {
		return 0, nil, nil, false
	}
	return iterations, salt, hash, true
}

func pbkdf2SHA256(password, salt []byte, iterations, keyLength int) []byte {
	const hashLength = 32
	blocks := (keyLength + hashLength - 1) / hashLength
	derived := make([]byte, 0, blocks*hashLength)
	for block := 1; block <= blocks; block++ {
		message := make([]byte, len(salt)+4)
		copy(message, salt)
		message[len(salt)] = byte(block >> 24)
		message[len(salt)+1] = byte(block >> 16)
		message[len(salt)+2] = byte(block >> 8)
		message[len(salt)+3] = byte(block)
		mac := hmac.New(sha256.New, password)
		_, _ = mac.Write(message)
		u := mac.Sum(nil)
		result := append([]byte(nil), u...)
		for round := 1; round < iterations; round++ {
			mac = hmac.New(sha256.New, password)
			_, _ = mac.Write(u)
			u = mac.Sum(nil)
			for i := range result {
				result[i] ^= u[i]
			}
		}
		derived = append(derived, result...)
	}
	return derived[:keyLength]
}
func readJSON(path string, target any) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	return json.NewDecoder(file).Decode(target)
}
func writeJSONAtomic(path string, value any) error {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	temporary := path + ".tmp"
	if err := os.WriteFile(temporary, append(data, '\n'), 0o600); err != nil {
		return err
	}
	return os.Rename(temporary, path)
}
func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}
func writeOpenAIError(w http.ResponseWriter, status int, code, message string) {
	writeJSON(w, status, map[string]any{"error": map[string]string{"message": message, "type": "invalid_request_error", "code": code}})
}
func recovery(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if recover() != nil {
				writeOpenAIError(w, 500, "internal_error", "internal server error")
			}
		}()
		next.ServeHTTP(w, r)
	})
}
