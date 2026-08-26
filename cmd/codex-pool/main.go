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
	listenAddressDefault      = ":8317"
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
	maxOwnerNoteRunes         = 80
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
	// Request-level routing/cache diagnostics are intentionally a short rolling
	// window, not a durable token ledger. Keep both limits even if one is later
	// made configurable: count bounds busy pools while TTL removes stale events
	// after traffic stops.
	routingCacheEventLimit     = 500
	routingCacheEventViewLimit = 50
	routingCacheEventTTL       = 24 * time.Hour
	// Throughput buckets are in-memory operational aggregates, not a request log.
	// One-minute account buckets preserve enough resolution for five-minute
	// management rows while the public chart combines them into 10-minute points.
	// Retain 48 hours and enforce a hard cap so telemetry cannot consume memory
	// without limit; these buckets must never be written to runtime.json.
	throughputBucketInterval = time.Minute
	throughputSeriesInterval = 10 * time.Minute
	throughputBucketTTL      = 48 * time.Hour
	throughputBucketLimit    = 100000
	// promptCacheBucketsDefault spreads a coarse (project/user) prompt cache key
	// across a few buckets so a hot scope stays under OpenAI's ~15 RPM per
	// (prefix + prompt_cache_key) limit while still sharing the static prefix
	// across conversations. 4 covers a heavy single user (~60 RPM) before any
	// overflow; raise it if the dashboard shows a hot account with a low hit rate.
	promptCacheBucketsDefault = 4
	quotaRefreshInterval      = 5 * time.Minute
	quotaRefreshTimeout       = 30 * time.Second
	// Quota telemetry is refreshed every five minutes. Give one missed poll a
	// grace interval for display, but do not use freshness itself as a routing
	// gate: SPEC section 6.4 deliberately keeps telemetry failures fail-open.
	quotaTelemetryFreshness  = 10 * time.Minute
	codexAuthReadAttempts    = 20
	codexAuthReadRetryDelay  = 50 * time.Millisecond
	upstreamFirstByteTimeout = 45 * time.Second
	upstream5xxCooldown      = 10 * time.Second
	upstream5xxFailoverAfter = 3
	upstream5xxFailureWindow = 2 * time.Minute
	// Codex treats remote model metadata as authoritative for ChatGPT-backed
	// providers. This fallback must remain non-empty or a schema-only fix would
	// silently remove the coding-agent instructions after a successful refresh.
	codexModelBaseInstructions = "You are Codex, a coding agent running in a terminal-based coding assistant. Inspect the workspace before acting, follow repository instructions such as AGENTS.md, use the available tools to implement and verify changes, keep the user informed, and continue until the task is genuinely handled."
)

var throughputLatencyBoundsMillis = []uint64{
	100, 250, 500, 1000, 2000, 5000, 10000, 30000,
	60000, 120000, 300000, 600000, 900000,
}

var (
	errAccountAuthFailed            = errors.New("account authentication failed")
	errCodexAuthMissing             = errors.New("codex auth missing")
	errAccountAuthRepairPending     = errors.New("sticky account sign-in repair is in progress")
	errAnotherLoginJobInProgress    = errors.New("another device-auth login is already in progress")
	errPublicRepairIdentityMismatch = errors.New("public sign-in repair identity did not match the existing account")
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
	ID    string `json:"id"`
	Label string `json:"label"`
	// OwnerNote is deliberately user-supplied public display text. It helps
	// shared users identify who owns a credential without exposing the local or
	// upstream account IDs that the unauthenticated dashboard must keep hidden.
	OwnerNote        string `json:"ownerNote,omitempty"`
	Email            string `json:"email,omitempty"`
	AccountID        string `json:"accountId,omitempty"`
	OrganizationName string `json:"organizationName,omitempty"`
	// Deprecated: migrated into OrganizationName at load time. Not exposed in admin APIs.
	OrganizationNameOverride string `json:"organizationNameOverride,omitempty"`
	PlanType                 string `json:"planType,omitempty"`
	PlanLimit                string `json:"planLimit,omitempty"`
	PlanRank                 int    `json:"planRank,omitempty"`
	// RawPlanType preserves the sanitized upstream wire value. PlanFamily is the
	// explicit display/ranking family; neither field is evidence of a Business
	// Standard/Premium seat. SeatType must stay empty until a supported endpoint
	// and exact field path are proven with a sanitized real fixture.
	RawPlanType string `json:"rawPlanType,omitempty"`
	PlanFamily  string `json:"planFamily,omitempty"`
	SeatType    string `json:"seatType,omitempty"`
	// SeatTypeRaw is display-only forward compatibility for an authoritative but
	// not-yet-recognized seat value. It must never grant models or capacity.
	SeatTypeRaw    string   `json:"seatTypeRaw,omitempty"`
	QuotaPolicy    []string `json:"quotaPolicy,omitempty"`
	AuthType       string   `json:"authType"`
	CodexHome      string   `json:"codexHome,omitempty"`
	Enabled        bool     `json:"enabled"`
	InPool         bool     `json:"inPool"`
	Priority       int      `json:"priority"`
	RemainingQuota *int     `json:"remainingQuota,omitempty"`
	// Quota protection is local-slot policy, not upstream-account capacity.
	// Missing fields load as disabled so upgrades preserve fail-open routing.
	QuotaProtectionEnabled   bool     `json:"quotaProtectionEnabled,omitempty"`
	QuotaProtectionThreshold int      `json:"quotaProtectionThreshold,omitempty"`
	AllowedModels            []string `json:"allowedModels,omitempty"`
	ExcludedModels           []string `json:"excludedModels,omitempty"`
	UpstreamBaseURL          string   `json:"upstreamBaseUrl,omitempty"`
	UpstreamAPIKey           string   `json:"upstreamApiKey,omitempty"`
	WireAPI                  string   `json:"wireApi,omitempty"`
	// PendingPoolActivation keeps a newly-created device-auth slot disabled and
	// out of the pool until login has produced usable auth and gateway state.
	// Without this staging flag, empty slots can stall status/routing paths while
	// they repeatedly classify missing auth under the global state lock.
	PendingPoolActivation bool `json:"pendingPoolActivation,omitempty"`
	// PendingAuthVerification is durable so a restart between auth.json rewrite
	// and identity/sidecar finalization cannot make the slot routable. It is
	// cleared only after the upstream account id has been checked and every
	// required gateway/state write succeeds.
	PendingAuthVerification bool `json:"pendingAuthVerification,omitempty"`
	// PendingAuthExpectedAccountID freezes the last verified upstream identity
	// before Codex rewrites credentials. Keep it until finalization succeeds:
	// config.json and runtime.json are separate atomic files, so a partial save
	// must not let a newly written account id inherit older runtime history.
	PendingAuthExpectedAccountID string    `json:"pendingAuthExpectedAccountId,omitempty"`
	CreatedAt                    time.Time `json:"createdAt"`
	UpdatedAt                    time.Time `json:"updatedAt"`
	LastLoginAt                  time.Time `json:"lastLoginAt,omitempty"`
}

type cooldown struct {
	ModelID     string    `json:"modelId"`
	NextRetryAt time.Time `json:"nextRetryAt"`
	Reason      string    `json:"reason"`
}

type quotaWindow struct {
	Role             string   `json:"role,omitempty"`
	Label            string   `json:"label,omitempty"`
	Percentage       int      `json:"percentage"`
	UsedPercent      *float64 `json:"usedPercent,omitempty"`
	RemainingPercent *float64 `json:"remainingPercent,omitempty"`
	ResetAt          *int64   `json:"resetAt,omitempty"`
	WindowMinutes    *int64   `json:"windowMinutes,omitempty"`
	// Observed means the upstream returned a window object. Present is stricter:
	// it is true only when used_percent was an explicit valid number.
	Observed bool `json:"observed,omitempty"`
	Present  bool `json:"present"`
}

type quotaCredits struct {
	HasCredits bool    `json:"hasCredits"`
	Unlimited  bool    `json:"unlimited"`
	Balance    *string `json:"balance,omitempty"`
}

type quotaSpendControl struct {
	Reached          bool   `json:"reached"`
	Source           string `json:"source,omitempty"`
	Limit            string `json:"limit,omitempty"`
	Used             string `json:"used,omitempty"`
	Remaining        string `json:"remaining,omitempty"`
	RemainingPercent *int   `json:"remainingPercent,omitempty"`
	ResetAt          *int64 `json:"resetAt,omitempty"`
}

type quotaLimit struct {
	LimitID          string        `json:"limitId,omitempty"`
	LimitName        string        `json:"limitName,omitempty"`
	Primary          quotaWindow   `json:"primary"`
	Secondary        quotaWindow   `json:"secondary"`
	Windows          []quotaWindow `json:"windows,omitempty"`
	Allowed          *bool         `json:"allowed,omitempty"`
	LimitReached     *bool         `json:"limitReached,omitempty"`
	Exhausted        bool          `json:"exhausted"`
	ExhaustionReason string        `json:"exhaustionReason,omitempty"`
}

type quotaResetCredits struct {
	AvailableCount *int64 `json:"availableCount,omitempty"`
}

type accountQuota struct {
	LimitID              string             `json:"limitId,omitempty"`
	LimitName            string             `json:"limitName,omitempty"`
	Primary              quotaWindow        `json:"primary"`
	Secondary            quotaWindow        `json:"secondary"`
	Windows              []quotaWindow      `json:"windows,omitempty"`
	Credits              *quotaCredits      `json:"credits,omitempty"`
	IndividualLimit      *quotaSpendControl `json:"individualLimit,omitempty"`
	Allowed              *bool              `json:"allowed,omitempty"`
	LimitReached         *bool              `json:"limitReached,omitempty"`
	RateLimitReachedType string             `json:"rateLimitReachedType,omitempty"`
	AdditionalLimits     []quotaLimit       `json:"additionalLimits,omitempty"`
	ResetCredits         *quotaResetCredits `json:"resetCredits,omitempty"`
	Exhausted            bool               `json:"exhausted"`
	ExhaustionReason     string             `json:"exhaustionReason,omitempty"`
	Provenance           string             `json:"provenance,omitempty"`
	ObservedAt           time.Time          `json:"observedAt,omitempty"`
	// Hourly/Weekly are compatibility aliases only. They are populated by actual
	// 300-minute and 10,080-minute durations and must not drive new UI semantics.
	Hourly quotaWindow `json:"hourly"`
	Weekly quotaWindow `json:"weekly"`
}

type quotaErrorInfo struct {
	Code      string    `json:"code,omitempty"`
	Message   string    `json:"message"`
	Timestamp time.Time `json:"timestamp"`
}

type quotaSnapshot struct {
	AccountID               string          `json:"accountId"`
	OrganizationName        string          `json:"organizationName,omitempty"`
	RawPlanType             string          `json:"rawPlanType,omitempty"`
	PlanFamily              string          `json:"planFamily,omitempty"`
	PlanType                string          `json:"planType,omitempty"`
	PlanLimit               string          `json:"planLimit,omitempty"`
	SeatType                string          `json:"seatType,omitempty"`
	SeatTypeRaw             string          `json:"seatTypeRaw,omitempty"`
	QuotaPolicy             []string        `json:"quotaPolicy,omitempty"`
	Quota                   *accountQuota   `json:"quota,omitempty"`
	ObservedAt              time.Time       `json:"observedAt,omitempty"`
	LastSuccessfulRefreshAt time.Time       `json:"lastSuccessfulRefreshAt,omitempty"`
	UsageUpdatedAt          time.Time       `json:"usageUpdatedAt,omitempty"`
	Freshness               string          `json:"freshness,omitempty"`
	Provenance              string          `json:"provenance,omitempty"`
	QuotaError              *quotaErrorInfo `json:"quotaError,omitempty"`
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
	AccountID                      string `json:"accountId"`
	ModelID                        string `json:"modelId"`
	AgentKind                      string `json:"agentKind,omitempty"`
	RequestCount                   uint64 `json:"requestCount"`
	UsageObservedRequestCount      uint64 `json:"usageObservedRequestCount"`
	InputTokens                    uint64 `json:"inputTokens"`
	CachedTokens                   uint64 `json:"cachedTokens"`
	CacheWriteTokens               uint64 `json:"cacheWriteTokens"`
	CacheWriteInputTokens          uint64 `json:"cacheWriteInputTokens"`
	CacheWriteObservedRequestCount uint64 `json:"cacheWriteObservedRequestCount"`
	CacheHitRequestCount           uint64 `json:"cacheHitRequestCount"`
	CacheEligibleRequestCount      uint64 `json:"cacheEligibleRequestCount"`
	// ColdRequestCount counts cache-eligible requests (input >= 1024 tokens)
	// that returned zero cached tokens, i.e. a cold start. It quantifies why a
	// hit rate is low: new conversations, failover hand-offs, or 15 RPM overflow.
	ColdRequestCount            uint64    `json:"coldRequestCount"`
	ParentAffinityHitCount      uint64    `json:"parentAffinityHitCount,omitempty"`
	ParentAffinityFallbackCount uint64    `json:"parentAffinityFallbackCount,omitempty"`
	LineageFailoverCount        uint64    `json:"lineageFailoverCount,omitempty"`
	RoutingFailoverCount        uint64    `json:"routingFailoverCount,omitempty"`
	UpdatedAt                   time.Time `json:"updatedAt"`
}

// routingCacheEvent contains only bounded operational metadata. Raw thread,
// sticky, prompt-cache, response, and request identifiers are hashed before the
// event is persisted; prompt bodies, tool arguments, credentials, emails, and
// upstream account identities must never be added here.
type routingCacheEvent struct {
	Timestamp             time.Time `json:"timestamp"`
	RequestIDHash         string    `json:"requestIdHash,omitempty"`
	ResponseIDHash        string    `json:"responseIdHash,omitempty"`
	ModelID               string    `json:"modelId"`
	AccountID             string    `json:"accountId"`
	AgentKind             string    `json:"agentKind"`
	ThreadIDHash          string    `json:"threadIdHash,omitempty"`
	LineageRootIDHash     string    `json:"lineageRootIdHash,omitempty"`
	StickyKeyHash         string    `json:"stickyKeyHash,omitempty"`
	PromptCacheKeyHash    string    `json:"promptCacheKeyHash,omitempty"`
	RoutingOutcome        string    `json:"routingOutcome"`
	RoutingSource         string    `json:"routingSource"`
	TerminalEvent         string    `json:"terminalEvent,omitempty"`
	TerminalFailureClass  string    `json:"terminalFailureClass,omitempty"`
	TerminalErrorCode     string    `json:"terminalErrorCode,omitempty"`
	ParentAffinity        string    `json:"parentAffinity"`
	FailoverFromAccountID string    `json:"failoverFromAccountId,omitempty"`
	UsageObserved         bool      `json:"usageObserved"`
	InputTokens           uint64    `json:"inputTokens"`
	CachedTokens          uint64    `json:"cachedTokens"`
	CacheWriteTokens      *uint64   `json:"cacheWriteTokens,omitempty"`
	UncachedInputTokens   uint64    `json:"uncachedInputTokens"`
	CacheReadRate         *float64  `json:"cacheReadRate,omitempty"`
	CacheWriteRate        *float64  `json:"cacheWriteRate,omitempty"`
	CacheReuseBalance     *int64    `json:"cacheReuseBalance,omitempty"`
	CacheHit              bool      `json:"cacheHit"`
	ColdCacheEligible     bool      `json:"coldCacheEligible"`
}

// throughputBucket is an in-memory aggregate used for the 48-hour chart and
// per-account rolling rows. Account is retained only for management filtering
// and identity cleanup; model, agent, request, route, thread, prompt-cache, and
// response identifiers must never be added. Keeping these buckets out of
// runtime.json avoids turning exploratory telemetry into durable traffic logs.
type throughputBucket struct {
	BucketAt                   time.Time `json:"bucketAt"`
	AccountID                  string    `json:"accountId,omitempty"`
	RequestCount               uint64    `json:"requestCount"`
	SuccessCount               uint64    `json:"successCount"`
	FailureCount               uint64    `json:"failureCount"`
	CancelledCount             uint64    `json:"cancelledCount,omitempty"`
	StreamingRequestCount      uint64    `json:"streamingRequestCount,omitempty"`
	UsageObservedRequestCount  uint64    `json:"usageObservedRequestCount,omitempty"`
	OutputObservedRequestCount uint64    `json:"outputObservedRequestCount,omitempty"`
	InputTokens                uint64    `json:"inputTokens,omitempty"`
	CachedTokens               uint64    `json:"cachedTokens,omitempty"`
	OutputTokens               uint64    `json:"outputTokens,omitempty"`
	TotalDurationMillis        uint64    `json:"totalDurationMillis,omitempty"`
	DurationHistogram          []uint64  `json:"durationHistogram,omitempty"`
}

type throughputMeasurement struct {
	StartedAt   time.Time
	CompletedAt time.Time
	AccountID   string
	Streaming   bool
	Success     bool
	Cancelled   bool
	Finished    bool
	Usage       promptCacheUsage
}

type accountHealth struct {
	LastSuccessAt      time.Time `json:"lastSuccessAt,omitempty"`
	LastFailureAt      time.Time `json:"lastFailureAt,omitempty"`
	LastFailureReason  string    `json:"lastFailureReason,omitempty"`
	ConsecutiveFailure int       `json:"consecutiveFailure"`
}

type loginJob struct {
	ID        string `json:"jobId"`
	Type      string `json:"type"`
	Status    string `json:"status"`
	AccountID string `json:"accountId"`
	// PublicRepair is process-local policy, never API output. Public repair jobs
	// may expose only a redacted projection and must not accept a different
	// upstream identity, unlike the authenticated owner's replacement flow.
	PublicRepair     bool      `json:"-"`
	Reauthentication bool      `json:"reauthentication,omitempty"`
	HistoryReset     bool      `json:"historyReset,omitempty"`
	VerificationURL  string    `json:"verificationUrl,omitempty"`
	UserCode         string    `json:"userCode,omitempty"`
	CodeExpiresAt    time.Time `json:"codeExpiresAt,omitempty"`
	Message          string    `json:"message,omitempty"`
	Error            string    `json:"error,omitempty"`
	StartedAt        time.Time `json:"startedAt"`
	UpdatedAt        time.Time `json:"updatedAt"`
	CompletedAt      time.Time `json:"completedAt,omitempty"`
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
	RoutingCacheEvents          []routingCacheEvent  `json:"routingCacheEvents,omitempty"`
	// LegacyThroughputBuckets is read once when upgrading from the persisted
	// 30-minute implementation, then migrated into app memory and cleared before
	// the next runtime save. New throughput history must never be written here.
	LegacyThroughputBuckets     []throughputBucket `json:"throughputBuckets,omitempty"`
	RequestCount                uint64             `json:"requestCount"`
	SuccessCount                uint64             `json:"successCount"`
	FailureCount                uint64             `json:"failureCount"`
	UpstreamResponseFailedCount uint64             `json:"upstreamResponseFailedCount,omitempty"`
	StreamIncompleteCount       uint64             `json:"streamIncompleteCount,omitempty"`
	UpdatedAt                   time.Time          `json:"updatedAt"`
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
	listenAddress        string
	publicDashboard      bool
	codexBaseURL         string
	codexGatewayMode     string
	cliproxyBaseURL      string
	cliproxyAPIKey       string
	jobs                 map[string]*loginJob
	loginCancels         map[string]context.CancelFunc
	loginFailures        map[string]loginFailure
	authLocks            map[string]*sync.Mutex
	activeProxyRequests  uint64
	throughputBuckets    []throughputBucket
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
	listenAddress := combinedListenAddressFromEnv()
	allowRemote := os.Getenv("CODEX_POOL_ALLOW_REMOTE_ADMIN") == "true"
	// The provider API and management UI intentionally share one listener. Keep
	// this explicit opt-in for non-loopback binds: merging the ports must not
	// silently expose password login and public repair controls wherever /v1 is
	// reachable.
	if !allowRemote && !isLoopbackAddress(listenAddress) {
		return nil, errors.New("combined HTTP address must be loopback unless CODEX_POOL_ALLOW_REMOTE_ADMIN=true")
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
	// Product contract: the combined-port root is a public control page, while
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
		listenAddress:        listenAddress,
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
	// Upgrade compatibility: import the old persisted 30-minute buckets once,
	// then clear the JSON field. From this point onward the 48-hour series is
	// intentionally process-memory telemetry and starts fresh after a restart.
	migratedThroughput := len(a.state.LegacyThroughputBuckets) > 0
	if migratedThroughput {
		a.throughputBuckets = append(a.throughputBuckets, a.state.LegacyThroughputBuckets...)
		a.state.LegacyThroughputBuckets = nil
		a.pruneThroughputBucketsLocked(time.Now().UTC())
	}
	if a.pruneExpiredRuntimeStateLocked(time.Now().UTC()) || migratedThroughput {
		_ = a.saveLocked()
	}
	for i := range a.config.Accounts {
		a.config.Accounts[i].OwnerNote = cleanOwnerNote(a.config.Accounts[i].OwnerNote)
		a.config.Accounts[i].Email = normalizeEmail(a.config.Accounts[i].Email)
		a.config.Accounts[i].OrganizationName = cleanOrganizationName(a.config.Accounts[i].OrganizationName)
		if a.config.Accounts[i].OrganizationName == "" {
			a.config.Accounts[i].OrganizationName = cleanOrganizationName(a.config.Accounts[i].OrganizationNameOverride)
		}
		a.config.Accounts[i].OrganizationNameOverride = ""
		normalizeAccountPlanMetadata(&a.config.Accounts[i])
		a.config.Accounts[i].PlanLimit = cleanPlanLimit(a.config.Accounts[i].PlanLimit)
		a.config.Accounts[i].PlanRank = planRank(effectivePlanFamily(a.config.Accounts[i]))
		if a.config.Accounts[i].QuotaProtectionThreshold < 0 || a.config.Accounts[i].QuotaProtectionThreshold > 100 {
			// Invalid persisted policy must fail open rather than unexpectedly
			// removing a credential from routing after an upgrade.
			a.config.Accounts[i].QuotaProtectionEnabled = false
			a.config.Accounts[i].QuotaProtectionThreshold = 0
		}
		if strings.TrimSpace(a.config.Accounts[i].Label) == "" {
			a.config.Accounts[i].Label = accountDisplayName(a.config.Accounts[i])
		}
		if isCodexDeviceAuth(a.config.Accounts[i]) {
			a.config.Accounts[i].CodexHome = a.accountCodexHome(a.config.Accounts[i].ID)
			a.config.Accounts[i].UpstreamBaseURL = ""
			a.config.Accounts[i].UpstreamAPIKey = ""
		} else {
			// API-key providers are API-metered and do not have ChatGPT
			// subscription windows in this implementation.
			a.config.Accounts[i].QuotaProtectionEnabled = false
			a.config.Accounts[i].QuotaProtectionThreshold = 0
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
	server := &http.Server{Addr: a.listenAddress, Handler: a.mux(), ReadHeaderTimeout: 10 * time.Second}
	listener, err := net.Listen("tcp", a.listenAddress)
	if err != nil {
		return fmt.Errorf("listen combined HTTP service: %w", err)
	}
	a.logger.Printf("provider API and control UI listening on %s", a.listenAddress)
	a.startQuotaRefresher(context.Background())
	err = server.Serve(listener)
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

func (a *app) mux() http.Handler {
	mux := http.NewServeMux()
	// Surface contract: one listener serves both machine clients and humans.
	// Provider routes stay under /v1 and require the client API key; the control
	// page stays at /admin (and the convenient root alias), while owner-only
	// actions retain session and CSRF checks. Do not collapse these auth policies
	// merely because the network port is shared.
	mux.HandleFunc("GET /{$}", a.handleAdminPage)
	mux.HandleFunc("GET /healthz", a.requireAPIKey(a.handleHealthz))
	mux.HandleFunc("GET /v1/codex-pool/status", a.requireAPIKey(a.handleCurrentStatus))
	mux.HandleFunc("GET /v1/models", a.requireAPIKey(a.handleModels))
	mux.HandleFunc("POST /v1/responses", a.requireAPIKey(a.handleResponses))
	mux.HandleFunc("POST /v1/responses/compact", a.requireAPIKey(a.handleResponses))
	mux.HandleFunc("POST /v1/chat/completions", a.requireAPIKey(a.handleChatCompletions))
	// The combined-port root is the public control page. It is intentionally
	// visible without a password; owner-only actions are protected by
	// requireAdmin on the management API routes below.
	//
	// The unauthenticated/login chrome intentionally uses low-key wording. Do not
	// "clarify" the visible title or login copy into obvious Codex/pool/provider
	// management terms without owner approval: casual browsing and keyword probes
	// should not learn more than the public control surface must reveal. This is
	// only passive exposure reduction; requireAdmin remains the security boundary.
	mux.HandleFunc("GET /admin", a.handleAdminPage)
	mux.HandleFunc("GET /admin/assets/app.css", handleAdminCSS)
	mux.HandleFunc("GET /admin/assets/app.js", handleAdminJS)
	mux.HandleFunc("GET /admin/assets/logo.svg", handleAdminLogo)
	mux.HandleFunc("GET /admin/manifest.webmanifest", handleAdminManifest)
	mux.HandleFunc("GET /admin/api/public-dashboard", a.handlePublicDashboard)
	mux.HandleFunc("GET /admin/api/public-dashboard/accounts/", a.handlePublicAccountAction)
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
	streaming, _ := payload["stream"].(bool)
	throughput := a.beginThroughputMeasurement(streaming)
	defer a.finishThroughputMeasurement(throughput)
	updatedBody, err := json.Marshal(payload)
	if err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "unable to encode request")
		return
	}
	requestID := requestOperationalID(r)
	failoverFromAccountID := ""
	failoverOutcome := ""
	excluded := map[string]bool{}
	for attempt := 0; attempt < a.proxyAttemptLimit(); attempt++ {
		candidate, err := a.selectAccountForRoute(route, model, excluded)
		if err != nil {
			if errors.Is(err, errAccountAuthRepairPending) {
				writeOpenAIError(w, http.StatusServiceUnavailable, "account_authenticating", "the session account is completing sign-in repair; retry shortly")
				return
			}
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
		throughput.AccountID = candidate.ID
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
				throughput.Cancelled = true
				return
			}
			excluded[candidate.ID] = true
			// A revoked or invalid device-auth credential must not end the user
			// request while other upstream accounts are eligible. Mark it
			// unavailable and continue; selectAccount also suppresses any local
			// duplicate slot for the same upstream identity during this attempt.
			if errors.Is(err, errAccountAuthFailed) {
				failoverFromAccountID = candidate.ID
				failoverOutcome = "auth_failover"
				a.markAccountAuthFailure(candidate.ID, model, "account_auth_failed")
				continue
			}
			failoverFromAccountID = candidate.ID
			failoverOutcome = "transport_failover"
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
			failoverFromAccountID = candidate.ID
			failoverOutcome = "auth_failover"
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
				failoverFromAccountID = candidate.ID
				failoverOutcome = "rate_limit_failover"
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
					failoverFromAccountID = candidate.ID
					failoverOutcome = "repeated_5xx_failover"
					a.markCooldown(candidate.ID, model, reason, wait)
					continue
				}
			}
			excluded[candidate.ID] = true
			failoverFromAccountID = candidate.ID
			failoverOutcome = "repeated_5xx_failover"
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
		info.RequestID = requestID
		info.FailoverFromAccountID = failoverFromAccountID
		info.FailoverOutcome = failoverOutcome
		throughput.CompletedAt = info.CompletedAt
		throughput.Usage = info.Usage
		if info.TerminalEvent == "response.failed" || info.TerminalEvent == "response.incomplete" {
			// Bytes have already been forwarded. Never retry another account here:
			// splicing a second stream could duplicate work and corrupt the SSE
			// sequence. Record the terminal failure without refreshing affinity.
			throughput.Success = false
			a.markTerminalResponseFailureWithMeasurement(route, model, candidate.ID, info, throughput)
			return
		}
		// Account health treats a well-formed non-retryable upstream response as
		// a healthy connection, but client throughput success is stricter: only
		// a 2xx response belongs in the success numerator.
		throughput.Success = response.StatusCode >= http.StatusOK && response.StatusCode < http.StatusMultipleChoices
		a.markSuccessWithMeasurement(route, model, candidate.ID, info, throughput)
		return
	}
	writeOpenAIError(w, http.StatusBadGateway, "bad_gateway", "all eligible upstream accounts failed")
}

func requestContextFinished(ctx context.Context, err error) bool {
	return ctx.Err() != nil || errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded)
}

func requestOperationalID(r *http.Request) string {
	for _, name := range []string{"X-Request-Id", "X-Codex-Request-Id"} {
		if value := strings.TrimSpace(r.Header.Get(name)); value != "" {
			return value
		}
	}
	return randomID()
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
	InputTokens      uint64
	CachedTokens     uint64
	CacheWriteTokens *uint64
	OutputTokens     uint64
	OutputPresent    bool
	Present          bool
}

type proxyResponseInfo struct {
	ResponseID            string
	Usage                 promptCacheUsage
	CompletedAt           time.Time
	TerminalEvent         string
	TerminalFailureClass  string
	TerminalErrorCode     string
	RequestID             string
	FailoverFromAccountID string
	FailoverOutcome       string
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
	info := responseInfoFromPayload(data)
	info.CompletedAt = time.Now().UTC()
	return info, true
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
	info.CompletedAt = time.Now().UTC()
	_, _ = w.Write(body)
	return info, true
}

func copyStreamingProxyResponse(w http.ResponseWriter, body io.Reader) proxyResponseInfo {
	var info proxyResponseInfo
	reader := bufio.NewReader(body)
	flusher, _ := w.(http.Flusher)
	var eventBlock strings.Builder
	mergeBlock := func() {
		if eventBlock.Len() == 0 {
			return
		}
		info.merge(responseInfoFromSSEBlock(eventBlock.String()))
		eventBlock.Reset()
	}
	for {
		line, err := reader.ReadString('\n')
		if line != "" {
			_, _ = io.WriteString(w, line)
			eventBlock.WriteString(line)
			if strings.TrimRight(line, "\r\n") == "" {
				mergeBlock()
			}
			if flusher != nil {
				flusher.Flush()
			}
		}
		if errors.Is(err, io.EOF) {
			mergeBlock()
			break
		}
		if err != nil {
			mergeBlock()
			break
		}
	}
	info.CompletedAt = time.Now().UTC()
	return info
}

func (info *proxyResponseInfo) merge(next proxyResponseInfo) {
	if next.ResponseID != "" {
		info.ResponseID = next.ResponseID
	}
	if next.Usage.Present {
		// A streaming gateway may report cache-write usage on an intermediate
		// event and omit the field from the final summary. Once observed, retain
		// that explicit value; absence in a later event must not turn it into a
		// confirmed zero or erase the observation.
		if next.Usage.CacheWriteTokens == nil && info.Usage.CacheWriteTokens != nil {
			next.Usage.CacheWriteTokens = info.Usage.CacheWriteTokens
		}
		if !next.Usage.OutputPresent && info.Usage.OutputPresent {
			next.Usage.OutputTokens = info.Usage.OutputTokens
			next.Usage.OutputPresent = true
		}
		info.Usage = next.Usage
	}
	if !next.CompletedAt.IsZero() {
		info.CompletedAt = next.CompletedAt
	}
	if next.TerminalEvent != "" {
		info.TerminalEvent = next.TerminalEvent
		info.TerminalFailureClass = next.TerminalFailureClass
		info.TerminalErrorCode = next.TerminalErrorCode
	}
}

func responseInfoFromSSELine(line string) proxyResponseInfo {
	return responseInfoFromSSEBlock(line)
}

func responseInfoFromSSEBlock(block string) proxyResponseInfo {
	var eventType string
	dataLines := make([]string, 0, 1)
	for _, line := range strings.Split(block, "\n") {
		line = strings.TrimSuffix(line, "\r")
		switch {
		case strings.HasPrefix(line, "event:"):
			eventType = cleanMetadataToken(strings.TrimSpace(strings.TrimPrefix(line, "event:")))
		case strings.HasPrefix(line, "data:"):
			dataLines = append(dataLines, strings.TrimSpace(strings.TrimPrefix(line, "data:")))
		}
	}
	data := strings.Join(dataLines, "\n")
	if data == "" || data == "[DONE]" {
		if isTerminalResponseEvent(eventType) && eventType != "response.completed" {
			return proxyResponseInfo{TerminalEvent: eventType, TerminalFailureClass: terminalFailureClass(eventType, "")}
		}
		return proxyResponseInfo{}
	}
	var payload map[string]any
	if err := json.Unmarshal([]byte(data), &payload); err != nil {
		if isTerminalResponseEvent(eventType) && eventType != "response.completed" {
			return proxyResponseInfo{TerminalEvent: eventType, TerminalFailureClass: terminalFailureClass(eventType, "")}
		}
		return proxyResponseInfo{}
	}
	if payloadType, _ := payload["type"].(string); isTerminalResponseEvent(cleanMetadataToken(payloadType)) {
		eventType = cleanMetadataToken(payloadType)
	}
	var info proxyResponseInfo
	if response, ok := payload["response"].(map[string]any); ok {
		info = responseInfoFromPayload(response)
		if errorPayload, _ := response["error"].(map[string]any); errorPayload != nil {
			info.TerminalErrorCode = sanitizedErrorCode(claimString(errorPayload, "code"))
		}
	} else {
		info = responseInfoFromPayload(payload)
		if errorPayload, _ := payload["error"].(map[string]any); errorPayload != nil {
			info.TerminalErrorCode = sanitizedErrorCode(claimString(errorPayload, "code"))
		}
	}
	if isTerminalResponseEvent(eventType) {
		info.TerminalEvent = eventType
		info.TerminalFailureClass = terminalFailureClass(eventType, info.TerminalErrorCode)
	}
	return info
}

func isTerminalResponseEvent(value string) bool {
	switch value {
	case "response.completed", "response.failed", "response.incomplete":
		return true
	default:
		return false
	}
}

func terminalFailureClass(eventType, code string) string {
	if eventType == "response.incomplete" {
		return "incomplete"
	}
	if eventType != "response.failed" {
		return ""
	}
	switch sanitizedErrorCode(code) {
	case "rate_limit_exceeded", "insufficient_quota", "usage_not_included", "server_is_overloaded", "slow_down":
		return "capacity"
	case "account_auth_failed", "authentication_error", "invalid_api_key", "invalid_token", "token_invalidated", "token_revoked", "unauthorized", "forbidden":
		return "authentication"
	case "context_length_exceeded", "invalid_prompt", "cyber_policy", "content_policy_violation", "policy_violation":
		return "request"
	default:
		return "unknown"
	}
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
	outputTokens, outputOK := firstUint64Field(usage, "output_tokens", "completion_tokens")
	var cachedTokens uint64
	var cachedOK bool
	var cacheWriteTokens uint64
	var cacheWriteOK bool
	for _, name := range []string{"input_tokens_details", "prompt_tokens_details"} {
		details, _ := usage[name].(map[string]any)
		if details == nil {
			continue
		}
		if !cachedOK {
			cachedTokens, cachedOK = firstUint64Field(details, "cached_tokens", "cache_read_tokens", "cache_read_input_tokens")
		}
		if !cacheWriteOK {
			cacheWriteTokens, cacheWriteOK = firstUint64Field(details, "cache_write_tokens", "cache_creation_tokens", "cache_creation_input_tokens", "cache_write_input_tokens")
		}
	}
	if !cachedOK {
		cachedTokens, cachedOK = firstUint64Field(usage, "cached_tokens", "cache_read_tokens", "cache_read_input_tokens")
	}
	if !cacheWriteOK {
		cacheWriteTokens, cacheWriteOK = firstUint64Field(usage, "cache_write_tokens", "cache_creation_tokens", "cache_creation_input_tokens", "cache_write_input_tokens")
	}
	var cacheWrite *uint64
	if cacheWriteOK {
		value := cacheWriteTokens
		cacheWrite = &value
	}
	return promptCacheUsage{
		InputTokens: inputTokens, CachedTokens: cachedTokens, CacheWriteTokens: cacheWrite,
		OutputTokens: outputTokens, OutputPresent: outputOK,
		Present: inputOK || cachedOK || cacheWriteOK || outputOK,
	}
}

func firstUint64Field(values map[string]any, names ...string) (uint64, bool) {
	for _, name := range names {
		if value, ok := uint64Field(values, name); ok {
			return value, true
		}
	}
	return 0, false
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
	return map[string]any{"running": true, "routingStrategy": a.effectiveRoutingStrategy(), "defaultModel": a.config.DefaultModel, "preserveProQuota": a.preserveProQuota, "promptCacheKeyMode": envOrValue(a.promptCacheKeyMode, "auto"), "promptCacheKeyScope": envOrValue(a.promptCacheKeyScope, "auto"), "promptCacheKeyPolicy": envOrValue(a.promptCacheKeyPolicy, "preserve"), "promptCacheBuckets": a.promptCacheBuckets, "promptCacheRetention": a.promptCacheRetention, "promptCache": a.state.PromptCache, "promptCacheWindow": a.promptCacheWindowLocked(), "throughput": a.throughputSnapshotLocked("", now), "routingCacheEvents": a.routingCacheEventViewsLocked(now), "threadBindings": a.state.ThreadBindings, "accounts": publicAccounts(a.config.Accounts), "requestCount": a.state.RequestCount, "successCount": a.state.SuccessCount, "failureCount": a.state.FailureCount, "upstreamResponseFailedCount": a.state.UpstreamResponseFailedCount, "streamIncompleteCount": a.state.StreamIncompleteCount, "summary": a.dashboardSummaryLocked(now)}
}

func (a *app) handlePublicDashboard(w http.ResponseWriter, _ *http.Request) {
	if !a.publicDashboard {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "not found")
		return
	}
	// This endpoint is intentionally unauthenticated: it powers the public
	// control page on the unified port. Keep it redacted and limited to pool
	// state; owner-only state stays behind requireAdmin routes.
	a.mu.RLock()
	defer a.mu.RUnlock()
	now := time.Now().UTC()
	accounts := make([]map[string]any, 0, len(a.config.Accounts))
	for index, item := range a.config.Accounts {
		accounts = append(accounts, a.publicDashboardAccountLocked(item, index, now))
	}
	// The default page intentionally shows pool-wide throughput so the owner can
	// monitor normal traffic without entering management mode. Keep this limited
	// to aggregate rolling windows: per-account throughput, raw buckets, model
	// attribution, and request-level routing events remain management-only.
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "dashboard": map[string]any{"updatedAt": a.state.UpdatedAt, "summary": a.publicDashboardSummaryLocked(now), "accounts": accounts, "promptCacheWindow": a.promptCacheWindowLocked(), "throughput": a.throughputSnapshotLocked("", now)}})
}

func (a *app) handlePublicAccountAction(w http.ResponseWriter, r *http.Request) {
	if !a.publicDashboard {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "not found")
		return
	}
	// Public users act only through an opaque per-process reference. Repair is a
	// deliberately narrow exception to owner-only login: it is offered only for
	// an invalid in-pool credential, exposes a redacted job, and requires the
	// exact previously verified upstream identity. Never return local account or
	// raw job IDs from this surface.
	parts := strings.Split(strings.TrimPrefix(r.URL.Path, "/admin/api/public-dashboard/accounts/"), "/")
	if len(parts) != 2 || parts[0] == "" {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "public account action not found")
		return
	}
	ref, action := parts[0], parts[1]
	if action != "pool-add" && action != "pool-remove" && action != "repair" && action != "note" {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "public account action not found")
		return
	}
	if r.Method == http.MethodGet && action != "repair" {
		writeOpenAIError(w, http.StatusNotFound, "not_found", "public account action not found")
		return
	}
	if r.Method != http.MethodGet && r.Method != http.MethodPost {
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
		case "repair":
			if r.Method == http.MethodGet {
				job := a.latestPublicRepairJobForAccountLocked(item.ID)
				if job == nil {
					writeOpenAIError(w, http.StatusNotFound, "not_found", "repair job not found")
					return
				}
				writeJSON(w, http.StatusOK, map[string]any{"ok": true, "job": publicRepairJob(*job)})
				return
			}
			if !a.publicRepairAvailableLocked(*item) {
				writeOpenAIError(w, http.StatusConflict, "repair_unavailable", "this credential does not currently require public repair")
				return
			}
			job, err := a.startPublicRepairJobLocked(*item)
			if err != nil {
				if errors.Is(err, errAnotherLoginJobInProgress) {
					writeOpenAIError(w, http.StatusConflict, "login_in_progress", "another sign-in is already in progress")
					return
				}
				if errors.Is(err, errPublicRepairIdentityMismatch) {
					writeOpenAIError(w, http.StatusConflict, "repair_unavailable", "this credential cannot be repaired from the public page")
					return
				}
				writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to start sign-in repair")
				return
			}
			writeJSON(w, http.StatusAccepted, map[string]any{"ok": true, "job": publicRepairJob(job)})
			return
		case "note":
			if r.Method != http.MethodPost {
				writeOpenAIError(w, http.StatusNotFound, "not_found", "public account action not found")
				return
			}
			var request struct {
				OwnerNote string `json:"ownerNote"`
			}
			if err := json.NewDecoder(io.LimitReader(r.Body, maxRequestBody)).Decode(&request); err != nil {
				writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid account note JSON")
				return
			}
			note, err := validateOwnerNote(request.OwnerNote)
			if err != nil {
				writeOpenAIError(w, http.StatusBadRequest, "invalid_request", err.Error())
				return
			}
			item.OwnerNote = note
			item.UpdatedAt = now
			if err := a.saveLocked(); err != nil {
				writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist account note")
				return
			}
			writeJSON(w, http.StatusOK, map[string]any{"ok": true, "account": a.publicDashboardAccountLocked(*item, index, now)})
			return
		case "pool-add":
			if item.PendingAuthVerification || a.accountLoginInProgressLocked(item.ID) {
				writeOpenAIError(w, http.StatusConflict, "account_authenticating", "complete sign-in repair before changing pool participation")
				return
			}
			item.Enabled = true
			item.InPool = true
		case "pool-remove":
			if item.PendingAuthVerification || a.accountLoginInProgressLocked(item.ID) {
				writeOpenAIError(w, http.StatusConflict, "account_authenticating", "complete sign-in repair before changing pool participation")
				return
			}
			if a.publicRepairAvailableLocked(*item) {
				// The public contract replaces Leave pool with Repair while the
				// credential is invalid. Enforce that server-side too so a stale
				// page cannot silently discard affinity instead of repairing it.
				writeOpenAIError(w, http.StatusConflict, "account_authenticating", "complete sign-in repair before changing pool participation")
				return
			}
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
	ownerNote, err := validateOwnerNote(input.OwnerNote)
	if err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", err.Error())
		return
	}
	input.OwnerNote = ownerNote
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
		input.RawPlanType = ""
		input.PlanFamily = ""
		input.PlanLimit = ""
		input.SeatType = ""
		input.SeatTypeRaw = ""
		input.QuotaPolicy = nil
		input.PlanRank = 0
		if input.QuotaProtectionThreshold < 0 || input.QuotaProtectionThreshold > 100 {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "quotaProtectionThreshold must be an integer from 0 to 100")
			return
		}
		// Verification markers are internal lifecycle state, never client input.
		// Accepting them here could create a permanently blocked slot or inject an
		// identity comparison baseline that never came from a verified login.
		input.PendingAuthVerification = false
		input.PendingAuthExpectedAccountID = ""
	} else {
		normalizeAccountPlanMetadata(&input)
		input.PlanLimit = cleanPlanLimit(input.PlanLimit)
		input.PlanRank = planRank(effectivePlanFamily(input))
		input.SeatType = ""
		input.SeatTypeRaw = ""
		input.QuotaPolicy = nil
		input.QuotaProtectionEnabled = false
		input.QuotaProtectionThreshold = 0
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
		snapshot = quotaSnapshotForDisplay(snapshot, time.Now().UTC())
		results = append(results, map[string]any{"accountId": accountID, "ok": true, "quota": snapshot.Quota, "organizationName": publicOrganizationName(snapshot.OrganizationName), "rawPlanType": snapshot.RawPlanType, "planFamily": snapshot.PlanFamily, "planType": snapshot.PlanType, "planLimit": snapshot.PlanLimit, "seatType": snapshot.SeatType, "seatTypeRaw": snapshot.SeatTypeRaw, "quotaPolicy": snapshot.QuotaPolicy, "usageUpdatedAt": snapshot.UsageUpdatedAt, "lastSuccessfulRefreshAt": snapshot.LastSuccessfulRefreshAt, "freshness": snapshot.Freshness, "provenance": snapshot.Provenance})
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
		snapshot = quotaSnapshotForDisplay(snapshot, time.Now().UTC())
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "accountId": id, "quota": snapshot.Quota, "organizationName": publicOrganizationName(snapshot.OrganizationName), "rawPlanType": snapshot.RawPlanType, "planFamily": snapshot.PlanFamily, "planType": snapshot.PlanType, "planLimit": snapshot.PlanLimit, "seatType": snapshot.SeatType, "seatTypeRaw": snapshot.SeatTypeRaw, "quotaPolicy": snapshot.QuotaPolicy, "usageUpdatedAt": snapshot.UsageUpdatedAt, "lastSuccessfulRefreshAt": snapshot.LastSuccessfulRefreshAt, "freshness": snapshot.Freshness, "provenance": snapshot.Provenance})
		return
	}
	defer a.mu.Unlock()
	if item.PendingAuthVerification {
		switch action {
		case "enable", "disable", "pool-add", "pool-remove":
			writeOpenAIError(w, http.StatusConflict, "account_authenticating", "complete sign-in repair before changing pool participation")
			return
		}
	}
	switch action {
	case "login":
		if !isCodexDeviceAuth(*item) {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "device auth login is only available for Codex accounts")
			return
		}
		job, err := a.startLoginJobLocked(*item)
		if err != nil {
			if errors.Is(err, errAnotherLoginJobInProgress) {
				writeOpenAIError(w, http.StatusConflict, "login_in_progress", err.Error())
				return
			}
			writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist sign-in repair state")
			return
		}
		writeJSON(w, http.StatusAccepted, map[string]any{"ok": true, "job": job})
		return
	case "note":
		var request struct {
			OwnerNote string `json:"ownerNote"`
		}
		if err := json.NewDecoder(io.LimitReader(r.Body, maxRequestBody)).Decode(&request); err != nil {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid account note JSON")
			return
		}
		note, err := validateOwnerNote(request.OwnerNote)
		if err != nil {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", err.Error())
			return
		}
		item.OwnerNote = note
		item.UpdatedAt = time.Now().UTC()
		if err := a.saveLocked(); err != nil {
			writeOpenAIError(w, http.StatusInternalServerError, "storage_error", "unable to persist account note")
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"ok": true, "account": publicAccount(*item, index)})
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
	case "quota-protection/set":
		if !isCodexDeviceAuth(*item) {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "quota protection is only available for Codex device-auth accounts")
			return
		}
		var request struct {
			Enabled   bool `json:"enabled"`
			Threshold *int `json:"threshold"`
		}
		if err := json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&request); err != nil || request.Threshold == nil || *request.Threshold < 0 || *request.Threshold > 100 {
			writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "threshold must be an integer from 0 to 100")
			return
		}
		// This is deliberately per local slot. Duplicate slots may share upstream
		// capacity, but changing one slot must not silently mutate another slot's
		// protection policy or its sticky history.
		item.QuotaProtectionEnabled = request.Enabled
		item.QuotaProtectionThreshold = *request.Threshold
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
			a.clearAccountIdentityScopedStateLocked(id)
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
		} else if item != nil && a.accountAuthVerificationPendingLocked(*item) {
			// Do not turn a short credential repair into a cold failover for a
			// live session. Preserve the binding and ask the client to retry;
			// unbound sessions may still use another healthy account.
			return account{}, errAccountAuthRepairPending
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
	leftQuotaClass, leftRemaining := a.identityRepresentativeQuotaClassLocked(left, model, now)
	rightQuotaClass, rightRemaining := a.identityRepresentativeQuotaClassLocked(right, model, now)
	if leftQuotaClass != rightQuotaClass {
		return leftQuotaClass > rightQuotaClass
	}
	if leftQuotaClass == 2 && leftRemaining != rightRemaining {
		return leftRemaining > rightRemaining
	}
	return a.preferredBeforeLocked(left, right)
}

func (a *app) identityRepresentativeQuotaClassLocked(item account, model string, now time.Time) (int, int) {
	snapshot := a.state.Quotas[item.ID]
	if quotaErrorBlocksRouting(snapshot.QuotaError) {
		return 0, 0
	}
	if snapshot.Quota != nil {
		if quotaExplicitlyBlocksRouting(*snapshot.Quota, model, now) || a.quotaProtectionStatusLocked(item, snapshot, now).Blocked {
			return 0, 0
		}
		remaining := remainingQuotaHint(*snapshot.Quota)
		return 2, remaining
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
	if a.accountAuthVerificationPendingLocked(item) {
		return false
	}
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
	// Re-authentication rewrites auth.json and the sidecar credential in place.
	// Keep the slot out of selection for the whole login job without disabling
	// it: disable/pool-remove clears sticky routes, which would throw away the KV
	// cache locality this repair flow exists to preserve.
	if a.accountAuthVerificationPendingLocked(item) {
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
		if quotaExplicitlyBlocksRouting(*snapshot.Quota, model, now) {
			return a.sameIdentityQuotaHintAvailableLocked(item, model, now)
		}
		if a.quotaProtectionStatusLocked(item, snapshot, now).Blocked {
			return false
		}
		return true
	}
	if available, decided := manualQuotaAvailable(item); decided {
		if available {
			return true
		}
		return a.sameIdentityQuotaHintAvailableLocked(item, model, now)
	}
	return true
}

func (a *app) quotaSnapshotAvailableLocked(item account, model string, now time.Time) (bool, bool) {
	accountID := item.ID
	snapshot := a.state.Quotas[accountID]
	if quotaErrorBlocksRouting(snapshot.QuotaError) {
		return false, true
	}
	if snapshot.Quota != nil {
		if quotaExplicitlyBlocksRouting(*snapshot.Quota, model, now) || a.quotaProtectionStatusLocked(item, snapshot, now).Blocked {
			return false, true
		}
		return true, true
	}
	return false, false
}

func manualQuotaAvailable(item account) (bool, bool) {
	if item.RemainingQuota != nil {
		return *item.RemainingQuota > 0, true
	}
	return false, false
}

type quotaProtectionStatus struct {
	Supported            bool         `json:"supported"`
	Enabled              bool         `json:"enabled"`
	Threshold            int          `json:"threshold"`
	State                string       `json:"state"`
	Message              string       `json:"message"`
	Blocked              bool         `json:"blocked"`
	EffectiveWindow      *quotaWindow `json:"effectiveWindow,omitempty"`
	Freshness            string       `json:"freshness"`
	LastSuccessfulUpdate time.Time    `json:"lastSuccessfulUpdate,omitempty"`
}

func (a *app) quotaProtectionStatusLocked(item account, snapshot quotaSnapshot, now time.Time) quotaProtectionStatus {
	status := quotaProtectionStatus{
		Supported:            isCodexDeviceAuth(item),
		Enabled:              item.QuotaProtectionEnabled,
		Threshold:            clampInt(item.QuotaProtectionThreshold, 0, 100),
		State:                "disabled",
		Message:              "Protection disabled",
		Freshness:            quotaTelemetryState(snapshot, now),
		LastSuccessfulUpdate: snapshot.LastSuccessfulRefreshAt,
	}
	if status.LastSuccessfulUpdate.IsZero() {
		status.LastSuccessfulUpdate = snapshot.UsageUpdatedAt
	}
	if !status.Supported {
		status.Message = "Protection unavailable: API-key accounts do not use ChatGPT quota windows"
		return status
	}
	if !status.Enabled {
		return status
	}
	if snapshot.Quota == nil {
		status.State = "unavailable"
		status.Message = "Protection unavailable: quota not reported; routing remains fail-open"
		return status
	}
	var effective *quotaWindow
	for _, duration := range []int64{300, 10080} {
		for _, window := range quotaReportedWindows(*snapshot.Quota) {
			if window.Present && window.WindowMinutes != nil && *window.WindowMinutes == duration {
				copy := window
				effective = &copy
				break
			}
		}
		if effective != nil {
			break
		}
	}
	if effective == nil {
		status.State = "unavailable"
		status.Message = "Protection unavailable: 5h/week quota not reported; routing remains fail-open"
		return status
	}
	status.EffectiveWindow = effective
	remaining := quotaWindowRemaining(*effective)
	if remaining <= float64(status.Threshold) {
		if effective.ResetAt != nil && now.Unix() >= *effective.ResetAt {
			status.State = "unavailable"
			status.Message = "Protection unavailable: observed threshold window reset expired; routing remains fail-open"
			return status
		}
		if effective.ResetAt == nil && status.Freshness != "fresh" {
			status.State = "unavailable"
			status.Message = "Protection unavailable: quota telemetry is stale; routing remains fail-open"
			return status
		}
		// A positively observed violation remains evidence until the same window
		// resets, even if a later advisory refresh fails. This is narrower than
		// fail-closed polling: missing/failed telemetry never creates a new block.
		status.State = "blocked"
		status.Message = "Protected: threshold reached"
		status.Blocked = true
		return status
	}
	if status.Freshness != "fresh" {
		status.State = "unavailable"
		status.Message = "Protection unavailable: quota not reported/stale; routing remains fail-open"
		return status
	}
	status.State = "available"
	status.Message = "Protection active: quota is above threshold"
	return status
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
		if available, decided := a.quotaSnapshotAvailableLocked(candidate, model, now); decided && available {
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

func (a *app) beginThroughputMeasurement(streaming bool) *throughputMeasurement {
	measurement := &throughputMeasurement{
		StartedAt: time.Now().UTC(),
		Streaming: streaming,
	}
	a.mu.Lock()
	a.activeProxyRequests++
	a.mu.Unlock()
	return measurement
}

func (a *app) finishThroughputMeasurement(measurement *throughputMeasurement) {
	if measurement == nil {
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()
	if measurement.Finished {
		return
	}
	a.finishThroughputMeasurementLocked(measurement, time.Now().UTC())
}

func (a *app) finishThroughputMeasurementLocked(measurement *throughputMeasurement, now time.Time) {
	if measurement == nil || measurement.Finished {
		return
	}
	measurement.Finished = true
	if measurement.CompletedAt.IsZero() {
		measurement.CompletedAt = now
	}
	if a.activeProxyRequests > 0 {
		a.activeProxyRequests--
	}
	a.recordThroughputMeasurementLocked(*measurement)
}

func (a *app) recordThroughputMeasurementLocked(measurement throughputMeasurement) {
	completedAt := measurement.CompletedAt
	if completedAt.IsZero() {
		completedAt = time.Now().UTC()
	}
	if measurement.StartedAt.IsZero() || completedAt.Before(measurement.StartedAt) {
		measurement.StartedAt = completedAt
	}
	a.pruneThroughputBucketsLocked(completedAt)
	bucketAt := completedAt.Truncate(throughputBucketInterval)
	index := -1
	for i := len(a.throughputBuckets) - 1; i >= 0; i-- {
		bucket := a.throughputBuckets[i]
		if bucket.BucketAt.Equal(bucketAt) && bucket.AccountID == measurement.AccountID {
			index = i
			break
		}
	}
	if index < 0 {
		a.throughputBuckets = append(a.throughputBuckets, throughputBucket{
			BucketAt:  bucketAt,
			AccountID: measurement.AccountID,
		})
		index = len(a.throughputBuckets) - 1
	}
	bucket := &a.throughputBuckets[index]
	bucket.RequestCount++
	switch {
	case measurement.Cancelled:
		bucket.CancelledCount++
	case measurement.Success:
		bucket.SuccessCount++
	default:
		bucket.FailureCount++
	}
	if measurement.Streaming {
		bucket.StreamingRequestCount++
	}
	if measurement.Usage.Present {
		bucket.UsageObservedRequestCount++
		bucket.InputTokens += measurement.Usage.InputTokens
		bucket.CachedTokens += measurement.Usage.CachedTokens
	}
	if measurement.Usage.OutputPresent {
		bucket.OutputObservedRequestCount++
		bucket.OutputTokens += measurement.Usage.OutputTokens
	}
	durationMillis := elapsedMillis(measurement.StartedAt, completedAt)
	bucket.TotalDurationMillis += durationMillis
	bucket.DurationHistogram = addHistogramObservation(bucket.DurationHistogram, durationMillis)
	if extra := len(a.throughputBuckets) - throughputBucketLimit; extra > 0 {
		sort.Slice(a.throughputBuckets, func(i, j int) bool {
			return a.throughputBuckets[i].BucketAt.Before(a.throughputBuckets[j].BucketAt)
		})
		a.throughputBuckets = append([]throughputBucket(nil), a.throughputBuckets[extra:]...)
	}
}

func elapsedMillis(start, end time.Time) uint64 {
	if start.IsZero() || end.IsZero() || end.Before(start) {
		return 0
	}
	return uint64(end.Sub(start) / time.Millisecond)
}

func addHistogramObservation(histogram []uint64, value uint64) []uint64 {
	required := len(throughputLatencyBoundsMillis) + 1
	if len(histogram) != required {
		resized := make([]uint64, required)
		copy(resized, histogram)
		histogram = resized
	}
	index := len(throughputLatencyBoundsMillis)
	for i, bound := range throughputLatencyBoundsMillis {
		if value <= bound {
			index = i
			break
		}
	}
	histogram[index]++
	return histogram
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
	a.markSuccessWithMeasurement(route, model, accountID, info, nil)
}

func (a *app) markTerminalResponseFailureWithMeasurement(route routingDecision, model, accountID string, info proxyResponseInfo, measurement *throughputMeasurement) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	a.finishThroughputMeasurementLocked(measurement, now)
	class := info.TerminalFailureClass
	if class == "" {
		class = "unknown"
	}
	reason := codeOr(sanitizedErrorCode(info.TerminalErrorCode), "upstream_response_failed")
	if info.TerminalEvent == "response.incomplete" {
		reason = "stream_incomplete"
	}
	switch class {
	case "capacity":
		health := a.state.Health[accountID]
		health.LastFailureAt = now
		health.LastFailureReason = reason
		health.ConsecutiveFailure++
		a.state.Health[accountID] = health
		duration := 30 * time.Second
		if reason == "server_is_overloaded" || reason == "slow_down" {
			duration = upstream5xxCooldown
		}
		a.state.Cooldowns[accountID] = append(a.state.Cooldowns[accountID], cooldown{ModelID: model, NextRetryAt: now.Add(duration), Reason: reason})
	case "authentication":
		health := a.state.Health[accountID]
		health.LastFailureAt = now
		health.LastFailureReason = reason
		health.ConsecutiveFailure++
		a.state.Health[accountID] = health
		if item := a.accountLocked(accountID); item != nil && isCodexDeviceAuth(*item) {
			prior := a.state.Quotas[accountID]
			prior.AccountID = accountID
			prior.QuotaError = &quotaErrorInfo{Code: reason, Message: "account credential is unavailable; sign in again", Timestamp: now}
			a.state.Quotas[accountID] = prior
		} else {
			a.state.Cooldowns[accountID] = append(a.state.Cooldowns[accountID], cooldown{ModelID: model, NextRetryAt: now.Add(15 * time.Minute), Reason: reason})
		}
	}
	// Request-specific and unknown terminal failures intentionally do not touch
	// account health. Context length, invalid prompt, policy rejection, or a new
	// unknown code does not prove another credential could serve the request.
	a.recordPromptCacheResultWithRoutingLocked(accountID, model, route.Identity, info.Usage, false, false, false, "upstream_response_failed", now)
	a.appendRoutingCacheEventLocked(route, model, accountID, info.FailoverFromAccountID, "upstream_response_failed", info, now)
	a.state.RequestCount++
	a.state.FailureCount++
	if info.TerminalEvent == "response.failed" {
		a.state.UpstreamResponseFailedCount++
	} else if info.TerminalEvent == "response.incomplete" {
		a.state.StreamIncompleteCount++
	}
	_ = a.saveLocked()
}

func (a *app) markSuccessWithMeasurement(route routingDecision, model, accountID string, info proxyResponseInfo, measurement *throughputMeasurement) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now().UTC()
	a.finishThroughputMeasurementLocked(measurement, now)
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
	routingOutcome, failoverFromAccountID := a.routingOutcomeLocked(route, model, accountID, prior, info, parentAffinityHit, parentAffinityFallback, now)
	a.recordPromptCacheResultWithRoutingLocked(accountID, model, route.Identity, info.Usage, parentAffinityHit, parentAffinityFallback, lineageFailover, routingOutcome, now)
	a.appendRoutingCacheEventLocked(route, model, accountID, failoverFromAccountID, routingOutcome, info, now)
	a.state.RequestCount++
	a.state.SuccessCount++
	_ = a.saveLocked()
}

func (a *app) recordPromptCacheUsageLocked(accountID, model string, usage promptCacheUsage, now time.Time) {
	a.recordPromptCacheResultLocked(accountID, model, requestIdentity{}, usage, false, false, false, now)
}

func (a *app) recordPromptCacheResultLocked(accountID, model string, identity requestIdentity, usage promptCacheUsage, parentAffinityHit, parentAffinityFallback, lineageFailover bool, now time.Time) {
	a.recordPromptCacheResultWithRoutingLocked(accountID, model, identity, usage, parentAffinityHit, parentAffinityFallback, lineageFailover, "", now)
}

func (a *app) recordPromptCacheResultWithRoutingLocked(accountID, model string, identity requestIdentity, usage promptCacheUsage, parentAffinityHit, parentAffinityFallback, lineageFailover bool, routingOutcome string, now time.Time) {
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
		stat.UsageObservedRequestCount++
		stat.InputTokens += usage.InputTokens
		stat.CachedTokens += usage.CachedTokens
		if usage.CachedTokens > 0 {
			stat.CacheHitRequestCount++
		}
		if usage.InputTokens >= promptCacheMinTokens {
			stat.CacheEligibleRequestCount++
		}
		if usage.CacheWriteTokens != nil {
			stat.CacheWriteTokens += *usage.CacheWriteTokens
			stat.CacheWriteInputTokens += usage.InputTokens
			stat.CacheWriteObservedRequestCount++
		}
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
	if strings.HasSuffix(routingOutcome, "_failover") {
		stat.RoutingFailoverCount++
	}
	stat.UpdatedAt = now
	a.state.PromptCache[key] = stat
}

func (a *app) routingOutcomeLocked(route routingDecision, model, accountID string, prior stickySession, info proxyResponseInfo, parentAffinityHit, parentAffinityFallback bool, now time.Time) (string, string) {
	if outcome := normalizedRoutingOutcome(info.FailoverOutcome); outcome != "" {
		return outcome, info.FailoverFromAccountID
	}
	if prior.AccountID != "" && prior.AccountID != accountID {
		if outcome := a.persistedFailoverOutcomeLocked(prior.AccountID, model, now); outcome != "" {
			return outcome, prior.AccountID
		}
	}
	if parentAffinityHit {
		return "parent_affinity", ""
	}
	if parentAffinityFallback {
		return "parent_affinity_fallback", ""
	}
	if prior.AccountID != "" && prior.AccountID == accountID {
		return "sticky_reuse", ""
	}
	return "new_route_assignment", ""
}

func normalizedRoutingOutcome(value string) string {
	switch value {
	case "sticky_reuse", "new_route_assignment", "parent_affinity", "parent_affinity_fallback", "quota_failover", "rate_limit_failover", "auth_failover", "transport_failover", "repeated_5xx_failover":
		return value
	default:
		return ""
	}
}

func (a *app) persistedFailoverOutcomeLocked(accountID, model string, now time.Time) string {
	item := a.accountLocked(accountID)
	if item == nil {
		return "auth_failover"
	}
	snapshot := a.state.Quotas[accountID]
	if quotaErrorBlocksRouting(snapshot.QuotaError) {
		return "auth_failover"
	}
	if snapshot.Quota != nil {
		if quotaExplicitlyBlocksRouting(*snapshot.Quota, model, now) || a.quotaProtectionStatusLocked(*item, snapshot, now).Blocked {
			return "quota_failover"
		}
	}
	if available, decided := manualQuotaAvailable(*item); decided && !available {
		return "quota_failover"
	}
	for _, cd := range a.state.Cooldowns[accountID] {
		if cd.ModelID != model || !cd.NextRetryAt.After(now) {
			continue
		}
		switch sanitizedErrorCode(cd.Reason) {
		case "rate_limited":
			return "rate_limit_failover"
		case "account_auth_failed", "invalid_token", "token_invalidated", "token_revoked", "unauthorized", "forbidden":
			return "auth_failover"
		case "upstream_transport_error":
			return "transport_failover"
		case "upstream_5xx":
			return "repeated_5xx_failover"
		}
	}
	return ""
}

func (a *app) appendRoutingCacheEventLocked(route routingDecision, model, accountID, failoverFromAccountID, routingOutcome string, info proxyResponseInfo, now time.Time) {
	usage := info.Usage
	agentKind := "main"
	if route.Identity.IsSubagent {
		agentKind = "subagent"
	}
	parentAffinity := "not_attempted"
	if route.ParentAffinityAttempted {
		parentAffinity = "fallback"
		if route.PreferredParentAccountID != "" && route.PreferredParentAccountID == accountID {
			parentAffinity = "hit"
		}
	}
	var readRate *float64
	if usage.Present && usage.InputTokens > 0 {
		value := float64(usage.CachedTokens) / float64(usage.InputTokens)
		readRate = &value
	}
	var writeRate *float64
	var reuseBalance *int64
	if usage.CacheWriteTokens != nil {
		if usage.InputTokens > 0 {
			value := float64(*usage.CacheWriteTokens) / float64(usage.InputTokens)
			writeRate = &value
		}
		value := tokenBalance(usage.CachedTokens, *usage.CacheWriteTokens)
		reuseBalance = &value
	}
	event := routingCacheEvent{
		Timestamp:             now,
		RequestIDHash:         operationalIdentifierHash("request", info.RequestID),
		ResponseIDHash:        operationalIdentifierHash("response", info.ResponseID),
		ModelID:               model,
		AccountID:             accountID,
		AgentKind:             agentKind,
		ThreadIDHash:          operationalIdentifierHash("thread", route.Identity.ThreadID),
		LineageRootIDHash:     operationalIdentifierHash("lineage", route.Identity.LineageRootID),
		StickyKeyHash:         operationalIdentifierHash("sticky", route.StickyKey),
		PromptCacheKeyHash:    operationalIdentifierHash("prompt-cache", route.UpstreamPromptCacheKey),
		RoutingOutcome:        routingOutcome,
		RoutingSource:         routingEventSource(route.Source),
		TerminalEvent:         info.TerminalEvent,
		TerminalFailureClass:  info.TerminalFailureClass,
		TerminalErrorCode:     sanitizedErrorCode(info.TerminalErrorCode),
		ParentAffinity:        parentAffinity,
		FailoverFromAccountID: failoverFromAccountID,
		UsageObserved:         usage.Present,
		InputTokens:           usage.InputTokens,
		CachedTokens:          usage.CachedTokens,
		CacheWriteTokens:      usage.CacheWriteTokens,
		UncachedInputTokens:   subSat(usage.InputTokens, usage.CachedTokens),
		CacheReadRate:         readRate,
		CacheWriteRate:        writeRate,
		CacheReuseBalance:     reuseBalance,
		CacheHit:              usage.Present && usage.CachedTokens > 0,
		ColdCacheEligible:     usage.Present && usage.InputTokens >= promptCacheMinTokens && usage.CachedTokens == 0,
	}
	a.pruneRoutingCacheEventsLocked(now)
	a.state.RoutingCacheEvents = append(a.state.RoutingCacheEvents, event)
	if extra := len(a.state.RoutingCacheEvents) - routingCacheEventLimit; extra > 0 {
		a.state.RoutingCacheEvents = append([]routingCacheEvent(nil), a.state.RoutingCacheEvents[extra:]...)
	}
}

func operationalIdentifierHash(kind, value string) string {
	value = strings.TrimSpace(value)
	if value == "" {
		return ""
	}
	return shortHash([]byte(kind + "\x00" + value))
}

func routingEventSource(source string) string {
	// Internal fallback routing historically calls its prompt-prefix hash source
	// "prompt". Diagnostics expose the product-level "fallback" name instead;
	// do not rename the internal source because cache-key compatibility logic
	// also consumes it.
	if source == "prompt" {
		return "fallback"
	}
	switch source {
	case "previous_response_id", "thread_id", "session", "project", "prompt_cache_key", "conversation", "session_id", "conversation_id":
		return source
	default:
		return "fallback"
	}
}

func tokenBalance(cached, written uint64) int64 {
	const maxInt64 = uint64(1<<63 - 1)
	if cached >= written {
		delta := cached - written
		if delta > maxInt64 {
			return int64(maxInt64)
		}
		return int64(delta)
	}
	delta := written - cached
	if delta > maxInt64 {
		return -int64(maxInt64)
	}
	return -int64(delta)
}

func deletePromptCacheForAccount(values map[string]promptCacheStat, accountID string) {
	for key, value := range values {
		if value.AccountID == accountID {
			delete(values, key)
		}
	}
}

func (a *app) clearAccountIdentityScopedStateLocked(accountID string) {
	delete(a.state.Health, accountID)
	delete(a.state.Cooldowns, accountID)
	delete(a.state.Quotas, accountID)
	deletePromptCacheForAccount(a.state.PromptCache, accountID)
	deletePromptCacheForAccount(a.state.PromptCacheBaseline, accountID)
	delete(a.state.PromptCacheResetAtByAccount, accountID)
	a.clearStickyForAccountLocked(accountID)
	// Throughput is identity-scoped history. Pool removal/disable only clears
	// routes and must not erase the operator's rolling traffic view, while
	// account deletion or a verified identity change must remove attribution.
	if len(a.throughputBuckets) > 0 {
		filtered := a.throughputBuckets[:0]
		for _, bucket := range a.throughputBuckets {
			if bucket.AccountID != accountID {
				filtered = append(filtered, bucket)
			}
		}
		a.throughputBuckets = append([]throughputBucket(nil), filtered...)
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
	if a.pruneRoutingCacheEventsLocked(now) {
		changed = true
	}
	return changed
}

func (a *app) pruneRoutingCacheEventsLocked(now time.Time) bool {
	if len(a.state.RoutingCacheEvents) == 0 {
		return false
	}
	cutoff := now.Add(-routingCacheEventTTL)
	first := 0
	for first < len(a.state.RoutingCacheEvents) && a.state.RoutingCacheEvents[first].Timestamp.Before(cutoff) {
		first++
	}
	if remaining := len(a.state.RoutingCacheEvents) - first; remaining > routingCacheEventLimit {
		first += remaining - routingCacheEventLimit
	}
	if first == 0 {
		return false
	}
	a.state.RoutingCacheEvents = append([]routingCacheEvent(nil), a.state.RoutingCacheEvents[first:]...)
	return true
}

func (a *app) pruneThroughputBucketsLocked(now time.Time) bool {
	if len(a.throughputBuckets) == 0 {
		return false
	}
	cutoff := now.Add(-throughputBucketTTL)
	filtered := a.throughputBuckets[:0]
	for _, bucket := range a.throughputBuckets {
		if bucket.BucketAt.Add(throughputBucketInterval).After(cutoff) {
			filtered = append(filtered, bucket)
		}
	}
	changed := len(filtered) != len(a.throughputBuckets)
	if len(filtered) > throughputBucketLimit {
		sort.Slice(filtered, func(i, j int) bool {
			return filtered[i].BucketAt.Before(filtered[j].BucketAt)
		})
		filtered = filtered[len(filtered)-throughputBucketLimit:]
		changed = true
	}
	if changed {
		a.throughputBuckets = append([]throughputBucket(nil), filtered...)
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
	if len(a.state.RoutingCacheEvents) > 0 {
		filtered := a.state.RoutingCacheEvents[:0]
		for _, event := range a.state.RoutingCacheEvents {
			if event.AccountID != id && event.FailoverFromAccountID != id {
				filtered = append(filtered, event)
			}
		}
		a.state.RoutingCacheEvents = append([]routingCacheEvent(nil), filtered...)
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

func (a *app) saveCompletedAuthVerificationLocked() error {
	now := time.Now().UTC()
	a.config.UpdatedAt = now
	a.state.UpdatedAt = now
	// Finalization intentionally reverses saveLocked's normal order. Runtime
	// identity/history changes must reach disk before config.json clears the
	// durable routing gate; otherwise a config-only partial write could release
	// a new identity alongside stale cache and affinity state after restart.
	if err := writeJSONAtomic(filepath.Join(a.dataDir, "state", "runtime.json"), a.state); err != nil {
		return err
	}
	return writeJSONAtomic(filepath.Join(a.dataDir, "config.json"), a.config)
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

func quotaMeteringKind(item account) string {
	if isCodexDeviceAuth(item) {
		return "chatgpt_subscription"
	}
	return "api_metered"
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
	PlanType             string                      `json:"plan_type"`
	SubscriptionPlan     string                      `json:"subscription_plan"`
	RateLimit            *codexRateLimitInfo         `json:"rate_limit"`
	CodeReviewRateLimit  *codexRateLimitInfo         `json:"code_review_rate_limit"`
	Credits              *codexCreditInfo            `json:"credits"`
	SpendControl         *codexSpendControlInfo      `json:"spend_control"`
	AdditionalRateLimits *[]codexAdditionalRateLimit `json:"additional_rate_limits"`
	RateLimitReachedType codexReachedType            `json:"rate_limit_reached_type"`
	ResetCredits         *codexResetCreditInfo       `json:"rate_limit_reset_credits"`
}

type codexSubscriptionMetadata struct {
	AccountID        string
	OrganizationName string
	RawPlanType      string
	PlanFamily       string
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
	UsedPercent        optionalNumber `json:"used_percent"`
	LimitWindowSeconds *int64         `json:"limit_window_seconds"`
	ResetAfterSeconds  *int64         `json:"reset_after_seconds"`
	ResetAt            *int64         `json:"reset_at"`
}

// optionalNumber deliberately accepts malformed future wire values without
// failing the whole quota refresh. Only an explicitly reported JSON number is
// percentage evidence; null, missing, strings, and objects stay Not reported.
type optionalNumber struct {
	Value    float64
	Reported bool
	Valid    bool
}

func (number *optionalNumber) UnmarshalJSON(data []byte) error {
	number.Reported = true
	if strings.TrimSpace(string(data)) == "null" {
		return nil
	}
	var value float64
	if err := json.Unmarshal(data, &value); err != nil {
		return nil
	}
	number.Value = value
	number.Valid = true
	return nil
}

type codexCreditInfo struct {
	HasCredits *bool   `json:"has_credits"`
	Unlimited  *bool   `json:"unlimited"`
	Balance    *string `json:"balance"`
}

type codexSpendControlInfo struct {
	Reached         *bool                `json:"reached"`
	IndividualLimit *codexSpendLimitInfo `json:"individual_limit"`
}

type codexSpendLimitInfo struct {
	Source            string `json:"source"`
	Limit             string `json:"limit"`
	Used              string `json:"used"`
	Remaining         string `json:"remaining"`
	RemainingPercent  *int   `json:"remaining_percent"`
	ResetAfterSeconds *int64 `json:"reset_after_seconds"`
	ResetAt           *int64 `json:"reset_at"`
}

type codexAdditionalRateLimit struct {
	LimitName      string              `json:"limit_name"`
	MeteredFeature string              `json:"metered_feature"`
	RateLimit      *codexRateLimitInfo `json:"rate_limit"`
}

type codexReachedType struct {
	Type string
}

func (reached *codexReachedType) UnmarshalJSON(data []byte) error {
	if strings.TrimSpace(string(data)) == "null" {
		return nil
	}
	var object struct {
		Type string `json:"type"`
	}
	if err := json.Unmarshal(data, &object); err == nil && object.Type != "" {
		reached.Type = cleanMetadataToken(object.Type)
		return nil
	}
	var value string
	if err := json.Unmarshal(data, &value); err == nil {
		reached.Type = cleanMetadataToken(value)
	}
	return nil
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
		PlanType:         cleanRawPlanType(chooseString(info.PlanType, chooseString(item.RawPlanType, item.PlanType))),
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
		record.PlanType = cleanRawPlanType(chooseString(item.RawPlanType, item.PlanType))
	}
	if item.PlanLimit != "" || effectivePlanFamily(item) == "pro" {
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
	info := codexAuthInfo{AccessToken: strings.TrimSpace(record.AccessToken), AccountID: strings.TrimSpace(record.AccountID), Email: normalizeEmail(record.Email), PlanType: cleanRawPlanType(record.PlanType), PlanLimit: cleanPlanLimit(record.PlanLimit)}
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
				info.PlanType = cleanRawPlanType(claimString(authClaims, "chatgpt_plan_type"))
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
	if metadata.RawPlanType == "" || metadata.PlanLimit == "" {
		if subscriptions, err := a.fetchCodexSubscriptionsMetadata(ctx, auth, accountID); err == nil {
			metadata.AccountID = chooseString(metadata.AccountID, subscriptions.AccountID)
			metadata.OrganizationName = cleanOrganizationName(chooseString(metadata.OrganizationName, subscriptions.OrganizationName))
			if subscriptions.RawPlanType != "" {
				metadata.RawPlanType = subscriptions.RawPlanType
				metadata.PlanFamily = subscriptions.PlanFamily
				metadata.PlanType = subscriptions.PlanFamily
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
	rawPlan = cleanRawPlanType(rawPlan)
	planFamily := planFamilyFromRaw(rawPlan)
	metadata := codexSubscriptionMetadata{
		AccountID:        firstMetadataString(values, "account_id", "accountId", "id", "chatgpt_account_id", "workspace_id", "workspaceId"),
		OrganizationName: cleanOrganizationName(organizationNameFromMap(values)),
		RawPlanType:      rawPlan,
		PlanFamily:       planFamily,
		PlanType:         planFamily,
		PlanLimit:        planLimitFromMap(values),
	}
	if metadata.PlanType == "unknown" {
		metadata.PlanType = ""
		metadata.PlanFamily = ""
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
	// accounts/check lists every workspace the login may act as, and the records
	// are collected from a Go map whose iteration order is randomized. Picking an
	// arbitrary record labels this credential with a foreign workspace's
	// entitlement: a login that holds both a personal Pro subscription and a Team
	// workspace would show its Team slot as "Pro 20x", lose the organization
	// suffix, and then persist the foreign account ID as the slot's upstream
	// identity, which routing uses for duplicate detection.
	if preferredAccountID != "" {
		if record, ok := subscriptionAccountRecordByID(records, preferredAccountID); ok {
			return subscriptionMetadataFromSelectedRecord(record), true
		}
	}
	// A sole accessible workspace stays unambiguous even when it is not the one
	// the credential last reported: the login can no longer act as the previous
	// workspace, and the caller deliberately treats that as an upstream identity
	// change that resets cache and sticky history. Several workspaces with no
	// match carry no evidence tying any of them to this credential, so report
	// nothing and let the caller fall back to the workspace-scoped
	// /subscriptions lookup instead of guessing. account_ordering only names the
	// login default, which is not necessarily the workspace this slot acts as.
	if len(records) != 1 {
		return codexSubscriptionMetadata{}, false
	}
	return subscriptionMetadataFromSelectedRecord(records[0]), true
}

func subscriptionAccountRecordByID(records []subscriptionAccountRecord, accountID string) (subscriptionAccountRecord, bool) {
	for _, record := range records {
		if strings.TrimSpace(record.key) == accountID {
			return record, true
		}
		if subscriptionMetadataFromRecord(record.node).AccountID == accountID {
			return record, true
		}
	}
	return subscriptionAccountRecord{}, false
}

func subscriptionMetadataFromSelectedRecord(record subscriptionAccountRecord) codexSubscriptionMetadata {
	metadata := subscriptionMetadataFromRecord(record.node)
	if metadata.AccountID == "" {
		metadata.AccountID = strings.TrimSpace(record.key)
	}
	return metadata
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
	rawPlan = cleanRawPlanType(rawPlan)
	planFamily := planFamilyFromRaw(rawPlan)
	organizationName := cleanOrganizationName(organizationNameFromMap(accountRecord))
	if organizationName == "" {
		organizationName = cleanOrganizationName(organizationNameFromMap(record))
	}
	metadata := codexSubscriptionMetadata{
		AccountID:        firstMetadataString(accountRecord, "account_id", "accountId", "id", "chatgpt_account_id", "workspace_id", "workspaceId"),
		OrganizationName: organizationName,
		RawPlanType:      rawPlan,
		PlanFamily:       planFamily,
	}
	metadata.PlanType = metadata.PlanFamily
	if entitlement != nil {
		metadata.PlanLimit = planLimitFromMap(entitlement)
	}
	if metadata.PlanLimit == "" {
		metadata.PlanLimit = planLimitFromMap(accountRecord)
	}
	if metadata.PlanType == "unknown" {
		metadata.PlanType = ""
		metadata.PlanFamily = ""
	}
	return metadata
}

func planLimitFromSubscriptionPlan(value string) string {
	// Plan names are descriptive metadata, not multiplier evidence. Generic Pro
	// (including legacy chatgptproplan) must remain Not reported unless an exact
	// supported numeric multiplier is present in its own upstream field.
	return ""
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
	return a.refreshAccountQuotaWithExpectedIdentity(ctx, accountID, "")
}

func (a *app) refreshAccountQuotaForRepair(ctx context.Context, accountID, expectedUpstreamAccountID string) (quotaSnapshot, error) {
	return a.refreshAccountQuotaWithExpectedIdentity(ctx, accountID, strings.TrimSpace(expectedUpstreamAccountID))
}

func (a *app) refreshAccountQuotaWithExpectedIdentity(ctx context.Context, accountID, expectedUpstreamAccountID string) (quotaSnapshot, error) {
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
	verifiedUpstreamAccountID := strings.TrimSpace(auth.AccountID)
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
	now := time.Now().UTC()
	quota := quotaFromUsage(usage, now)
	planRaw := cleanRawPlanType(chooseString(usage.PlanType, usage.SubscriptionPlan))
	plan := planFamilyFromRaw(planRaw)
	planLimit := planLimitFromMap(usageFields)
	if planLimit == "" {
		planLimit = cleanPlanLimit(auth.PlanLimit)
	}
	organizationName := cleanOrganizationName(organizationNameFromMap(usageFields))
	if organizationName == "" && auth.OrganizationName != "" {
		organizationName = cleanOrganizationName(auth.OrganizationName)
	}
	metadataResolved := false
	if planRaw == "" || organizationName == "" || organizationScopedPlan(plan) {
		if metadata, err := a.fetchCodexSubscriptionMetadata(ctx, auth); err == nil {
			metadataResolved = true
			if metadata.AccountID != "" {
				auth.AccountID = metadata.AccountID
				verifiedUpstreamAccountID = strings.TrimSpace(metadata.AccountID)
			}
			if metadata.RawPlanType != "" {
				planRaw = metadata.RawPlanType
			}
			if metadata.PlanFamily != "" && metadata.PlanFamily != "unknown" {
				plan = metadata.PlanFamily
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
	if plan != "" && plan != "unknown" && !organizationScopedPlan(plan) {
		organizationName = ""
	}
	if expectedUpstreamAccountID != "" && verifiedUpstreamAccountID != expectedUpstreamAccountID {
		// Public repair is not an account-replacement API. Perform this check
		// after subscription/workspace enrichment but before any config/runtime
		// mutation so a visitor cannot transfer a slot or its history.
		return quotaSnapshot{}, errPublicRepairIdentityMismatch
	}
	a.mu.Lock()
	if a.state.Quotas == nil {
		a.state.Quotas = map[string]quotaSnapshot{}
	}
	item = a.accountLocked(accountID)
	if item == nil {
		a.mu.Unlock()
		return quotaSnapshot{}, errors.New("account no longer exists")
	}
	prior := a.state.Quotas[accountID]
	quota = mergeSparseQuota(prior.Quota, quota, usage)
	snapshot := quotaSnapshot{
		AccountID:               accountID,
		OrganizationName:        prior.OrganizationName,
		RawPlanType:             prior.RawPlanType,
		PlanFamily:              prior.PlanFamily,
		PlanType:                prior.PlanType,
		PlanLimit:               prior.PlanLimit,
		SeatType:                prior.SeatType,
		SeatTypeRaw:             prior.SeatTypeRaw,
		QuotaPolicy:             append([]string(nil), prior.QuotaPolicy...),
		Quota:                   &quota,
		ObservedAt:              now,
		LastSuccessfulRefreshAt: now,
		UsageUpdatedAt:          now,
		Freshness:               "fresh",
		Provenance:              "chatgpt_wham_usage",
	}
	if snapshot.RawPlanType == "" {
		snapshot.RawPlanType = cleanRawPlanType(item.RawPlanType)
	}
	if snapshot.PlanFamily == "" {
		snapshot.PlanFamily = effectivePlanFamily(*item)
	}
	if snapshot.PlanType == "" {
		snapshot.PlanType = snapshot.PlanFamily
	}
	if snapshot.PlanLimit == "" {
		snapshot.PlanLimit = cleanPlanLimit(item.PlanLimit)
	}
	if snapshot.OrganizationName == "" {
		snapshot.OrganizationName = cleanOrganizationName(item.OrganizationName)
	}
	if planRaw != "" {
		snapshot.RawPlanType = planRaw
		snapshot.PlanFamily = plan
		snapshot.PlanType = plan
		// A generic Pro plan is not multiplier evidence. An exact multiplier may
		// still arrive from its own supported field in this same full refresh.
		snapshot.PlanLimit = planLimit
		item.RawPlanType = planRaw
		item.PlanFamily = plan
		item.PlanType = plan
		item.PlanRank = planRank(plan)
	} else if planLimit != "" {
		snapshot.PlanLimit = planLimit
	}
	item.PlanLimit = snapshot.PlanLimit
	item.SeatType = snapshot.SeatType
	item.SeatTypeRaw = snapshot.SeatTypeRaw
	item.QuotaPolicy = append([]string(nil), snapshot.QuotaPolicy...)
	if organizationName != "" || metadataResolved || (planRaw != "" && !organizationScopedPlan(plan)) {
		snapshot.OrganizationName = organizationName
	}
	if auth.Email != "" {
		item.Email = normalizeEmail(auth.Email)
	}
	if auth.AccountID != "" {
		item.AccountID = auth.AccountID
	}
	if organizationScopedPlan(snapshot.PlanFamily) {
		item.OrganizationName = snapshot.OrganizationName
	} else {
		item.OrganizationName = ""
		snapshot.OrganizationName = ""
	}
	item.Label = accountDisplayName(*item)
	hasQuotaWindow := false
	for _, window := range quotaReportedWindows(quota) {
		if window.Present {
			hasQuotaWindow = true
			break
		}
	}
	if hasQuotaWindow {
		remaining := remainingQuotaHint(quota)
		item.RemainingQuota = &remaining
	} else if quota.Exhausted {
		remaining := 0
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
	base := quotaLimitFromRateLimit("codex", "", usage.RateLimit, now)
	quota := accountQuota{
		LimitID:          base.LimitID,
		LimitName:        base.LimitName,
		Primary:          base.Primary,
		Secondary:        base.Secondary,
		Windows:          base.Windows,
		Allowed:          base.Allowed,
		LimitReached:     base.LimitReached,
		Exhausted:        base.Exhausted,
		ExhaustionReason: base.ExhaustionReason,
		Provenance:       "chatgpt_wham_usage",
		ObservedAt:       now,
	}
	if usage.Credits != nil {
		credits := &quotaCredits{}
		if usage.Credits.HasCredits != nil {
			credits.HasCredits = *usage.Credits.HasCredits
		}
		if usage.Credits.Unlimited != nil {
			credits.Unlimited = *usage.Credits.Unlimited
		}
		if usage.Credits.Balance != nil {
			value := cleanMetadataText(*usage.Credits.Balance, 80)
			if value != "" {
				credits.Balance = &value
			}
		}
		quota.Credits = credits
	}
	if usage.SpendControl != nil {
		limit := &quotaSpendControl{}
		if usage.SpendControl.Reached != nil {
			limit.Reached = *usage.SpendControl.Reached
		}
		if details := usage.SpendControl.IndividualLimit; details != nil {
			limit.Source = cleanMetadataToken(details.Source)
			limit.Limit = cleanMetadataText(details.Limit, 80)
			limit.Used = cleanMetadataText(details.Used, 80)
			limit.Remaining = cleanMetadataText(details.Remaining, 80)
			if details.RemainingPercent != nil {
				value := clampInt(*details.RemainingPercent, 0, 100)
				limit.RemainingPercent = &value
			}
			limit.ResetAt = normalizedResetAt(details.ResetAt, details.ResetAfterSeconds, now)
		}
		quota.IndividualLimit = limit
		if limit.Reached {
			quota.Exhausted = true
			quota.ExhaustionReason = "spend_control_reached"
		}
	}
	quota.RateLimitReachedType = cleanMetadataToken(usage.RateLimitReachedType.Type)
	if quota.RateLimitReachedType != "" {
		quota.Exhausted = true
		quota.ExhaustionReason = quota.RateLimitReachedType
	}
	if usage.AdditionalRateLimits != nil {
		for _, additional := range *usage.AdditionalRateLimits {
			quota.AdditionalLimits = append(quota.AdditionalLimits, quotaLimitFromRateLimit(
				cleanMetadataToken(additional.MeteredFeature),
				cleanMetadataText(additional.LimitName, 120),
				additional.RateLimit,
				now,
			))
		}
	}
	if usage.CodeReviewRateLimit != nil {
		quota.AdditionalLimits = append(quota.AdditionalLimits, quotaLimitFromRateLimit(
			"code_review",
			"Code review",
			usage.CodeReviewRateLimit,
			now,
		))
	}
	if usage.ResetCredits != nil {
		quota.ResetCredits = &quotaResetCredits{AvailableCount: usage.ResetCredits.AvailableCount}
	}
	quota.Hourly = quotaWindowByDuration(quota, 300)
	quota.Weekly = quotaWindowByDuration(quota, 10080)
	return quota
}

func mergeSparseQuota(prior *accountQuota, current accountQuota, usage codexUsageResponse) accountQuota {
	if prior == nil {
		return current
	}
	// Codex rolling snapshots may omit optional metadata while still providing
	// fresh windows. Missing/null credits, spend control, reset credits, and
	// additional limits are not clear signals, so retain the last verified value.
	if usage.Credits == nil {
		current.Credits = prior.Credits
	}
	if usage.SpendControl == nil {
		current.IndividualLimit = prior.IndividualLimit
	}
	if usage.ResetCredits == nil {
		current.ResetCredits = prior.ResetCredits
	}
	if usage.AdditionalRateLimits == nil && usage.CodeReviewRateLimit == nil {
		current.AdditionalLimits = append([]quotaLimit(nil), prior.AdditionalLimits...)
	}
	return current
}

func quotaLimitFromRateLimit(limitID, limitName string, rateLimit *codexRateLimitInfo, now time.Time) quotaLimit {
	result := quotaLimit{LimitID: limitID, LimitName: limitName}
	if rateLimit == nil {
		return result
	}
	result.Allowed = cloneBool(rateLimit.Allowed)
	result.LimitReached = cloneBool(rateLimit.LimitReached)
	result.Primary = normalizeQuotaWindow("primary", rateLimit.PrimaryWindow, now)
	result.Secondary = normalizeQuotaWindow("secondary", rateLimit.SecondaryWindow, now)
	for _, window := range []quotaWindow{result.Primary, result.Secondary} {
		if window.Observed || window.Present {
			result.Windows = append(result.Windows, window)
		}
	}
	if rateLimit.LimitReached != nil && *rateLimit.LimitReached {
		result.Exhausted = true
		result.ExhaustionReason = "rate_limit_reached"
	} else if rateLimit.Allowed != nil && !*rateLimit.Allowed {
		result.Exhausted = true
		result.ExhaustionReason = "not_allowed"
	}
	return result
}

func normalizeQuotaWindow(role string, window *codexWindowInfo, now time.Time) quotaWindow {
	if window == nil {
		return quotaWindow{Role: role}
	}
	result := quotaWindow{Role: role, Observed: true}
	if window.LimitWindowSeconds != nil && *window.LimitWindowSeconds > 0 {
		value := (*window.LimitWindowSeconds + 59) / 60
		result.WindowMinutes = &value
	}
	result.Label = quotaWindowLabel(result.WindowMinutes)
	result.ResetAt = normalizedResetAt(window.ResetAt, window.ResetAfterSeconds, now)
	if window.UsedPercent.Reported && window.UsedPercent.Valid {
		used := clampFloat(window.UsedPercent.Value, 0, 100)
		remaining := 100 - used
		result.UsedPercent = &used
		result.RemainingPercent = &remaining
		result.Percentage = int(remaining)
		result.Present = true
	}
	return result
}

func normalizedResetAt(resetAt, resetAfterSeconds *int64, now time.Time) *int64 {
	if resetAt != nil && *resetAt > 0 {
		value := *resetAt
		return &value
	}
	if resetAfterSeconds != nil && *resetAfterSeconds >= 0 {
		value := now.Add(time.Duration(*resetAfterSeconds) * time.Second).Unix()
		return &value
	}
	return nil
}

func quotaWindowLabel(minutes *int64) string {
	if minutes == nil || *minutes <= 0 {
		return "Window"
	}
	switch *minutes {
	case 300:
		return "5h"
	case 10080:
		return "Week"
	}
	if *minutes < 60 {
		return fmt.Sprintf("%dm", *minutes)
	}
	if *minutes%1440 == 0 {
		return fmt.Sprintf("%dd", *minutes/1440)
	}
	if *minutes%60 == 0 {
		return fmt.Sprintf("%dh", *minutes/60)
	}
	return fmt.Sprintf("%dh %dm", *minutes/60, *minutes%60)
}

func quotaWindowRemaining(window quotaWindow) float64 {
	if window.RemainingPercent != nil {
		return clampFloat(*window.RemainingPercent, 0, 100)
	}
	return float64(clampInt(window.Percentage, 0, 100))
}

func quotaWindowByDuration(quota accountQuota, minutes int64) quotaWindow {
	for _, window := range quotaReportedWindows(quota) {
		if window.WindowMinutes != nil && *window.WindowMinutes == minutes {
			return window
		}
	}
	return quotaWindow{Label: quotaWindowLabel(&minutes)}
}

func quotaReportedWindows(quota accountQuota) []quotaWindow {
	if len(quota.Windows) > 0 {
		return quota.Windows
	}
	result := make([]quotaWindow, 0, 2)
	for _, window := range []quotaWindow{quota.Primary, quota.Secondary} {
		if window.Observed || window.Present || window.WindowMinutes != nil {
			result = append(result, window)
		}
	}
	if len(result) > 0 {
		return result
	}
	// Persisted snapshots from older releases may only contain these aliases.
	for _, window := range []quotaWindow{quota.Hourly, quota.Weekly} {
		if window.Observed || window.Present || window.WindowMinutes != nil {
			result = append(result, window)
		}
	}
	return result
}

func remainingQuotaHint(quota accountQuota) int {
	values := make([]int, 0, 2)
	for _, window := range quotaReportedWindows(quota) {
		if window.Present {
			values = append(values, int(quotaWindowRemaining(window)))
		}
	}
	if len(values) == 0 {
		if quota.Exhausted {
			return 0
		}
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

func quotaWindowEvidenceActive(window quotaWindow, now time.Time) bool {
	if window.ResetAt == nil {
		return true
	}
	return now.Unix() < *window.ResetAt
}

func quotaExplicitlyBlocksRouting(quota accountQuota, model string, now time.Time) bool {
	if quota.Exhausted {
		return true
	}
	for _, window := range quotaReportedWindows(quota) {
		if window.Present && quotaWindowRemaining(window) <= 0 && quotaWindowEvidenceActive(window, now) {
			return true
		}
	}
	for _, additional := range quota.AdditionalLimits {
		// Unknown/model-specific buckets are display metadata unless the limit id
		// exactly names the requested model. Do not turn a new future meter into
		// an account-wide authorization rule.
		if additional.LimitID != model {
			continue
		}
		if additional.Exhausted {
			return true
		}
		for _, window := range additional.Windows {
			if window.Present && quotaWindowRemaining(window) <= 0 && quotaWindowEvidenceActive(window, now) {
				return true
			}
		}
	}
	return false
}

func quotaTelemetryState(snapshot quotaSnapshot, now time.Time) string {
	if snapshot.QuotaError != nil {
		return "refresh_unavailable"
	}
	observedAt := snapshot.LastSuccessfulRefreshAt
	if observedAt.IsZero() {
		observedAt = snapshot.UsageUpdatedAt
	}
	if observedAt.IsZero() {
		return "not_reported"
	}
	if now.Sub(observedAt) <= quotaTelemetryFreshness {
		return "fresh"
	}
	return "stale"
}

func quotaSnapshotForDisplay(snapshot quotaSnapshot, now time.Time) quotaSnapshot {
	snapshot.Freshness = quotaTelemetryState(snapshot, now)
	if snapshot.Provenance == "" && snapshot.Quota != nil {
		snapshot.Provenance = snapshot.Quota.Provenance
	}
	return snapshot
}

func cloneBool(value *bool) *bool {
	if value == nil {
		return nil
	}
	cloned := *value
	return &cloned
}

func clampFloat(value, min, max float64) float64 {
	if value < min {
		return min
	}
	if value > max {
		return max
	}
	return value
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

func (a *app) startLoginJobLocked(item account) (loginJob, error) {
	return a.startLoginJobWithPolicyLocked(item, false)
}

func (a *app) startPublicRepairJobLocked(item account) (loginJob, error) {
	expectedAccountID := strings.TrimSpace(item.AccountID)
	if item.PendingAuthVerification {
		expectedAccountID = strings.TrimSpace(item.PendingAuthExpectedAccountID)
	}
	// Public repair cannot safely initialize or replace a slot. Without a
	// previously verified upstream account ID, any visitor could bind their own
	// account to the credential directory, so require the owner-only flow.
	if expectedAccountID == "" {
		return loginJob{}, errPublicRepairIdentityMismatch
	}
	return a.startLoginJobWithPolicyLocked(item, true)
}

func (a *app) startLoginJobWithPolicyLocked(item account, publicRepair bool) (loginJob, error) {
	if a.jobs == nil {
		a.jobs = map[string]*loginJob{}
	}
	if a.loginCancels == nil {
		a.loginCancels = map[string]context.CancelFunc{}
	}
	for _, job := range a.jobs {
		if !loginJobActiveStatus(job.Status) {
			continue
		}
		if job.AccountID == item.ID {
			if publicRepair && !job.PublicRepair {
				// Never expose or adopt an owner-originated login job on the
				// unauthenticated page; it may have been started to replace the
				// slot and its device code belongs to the authenticated session.
				return loginJob{}, errAnotherLoginJobInProgress
			}
			slot := a.accountLocked(item.ID)
			if slot != nil && !slot.PendingAuthVerification {
				slot.PendingAuthVerification = true
				slot.PendingAuthExpectedAccountID = strings.TrimSpace(slot.AccountID)
				slot.UpdatedAt = time.Now().UTC()
				if err := a.saveLocked(); err != nil {
					// The config write may have succeeded even when the
					// runtime write failed. Stay blocked in memory too; the
					// active CLI may already be rewriting auth.json.
					return loginJob{}, fmt.Errorf("persist pending auth verification: %w", err)
				}
			}
			return *job, nil
		}
		return loginJob{}, errAnotherLoginJobInProgress
	}
	home := a.accountCodexHome(item.ID)
	jobID := fmt.Sprintf("job-login-%s-%d-%s", item.ID, time.Now().Unix(), randomID())
	now := time.Now().UTC()
	expectedUpstreamAccountID := strings.TrimSpace(item.AccountID)
	if item.PendingAuthVerification {
		// Retrying after a failed job or restart must compare against the identity
		// captured before the first credential rewrite, not metadata that may
		// have been partially saved by that unfinished attempt.
		expectedUpstreamAccountID = strings.TrimSpace(item.PendingAuthExpectedAccountID)
	}
	reauthentication := a.accountHasPriorIdentityLocked(item)
	slot := a.accountLocked(item.ID)
	if slot == nil {
		return loginJob{}, errors.New("account no longer exists")
	}
	if !slot.PendingAuthVerification {
		slot.PendingAuthVerification = true
		slot.PendingAuthExpectedAccountID = expectedUpstreamAccountID
	}
	slot.UpdatedAt = now
	if err := a.saveLocked(); err != nil {
		// Do not roll the gate back in memory: saveLocked may already have
		// committed config.json before runtime.json failed.
		return loginJob{}, fmt.Errorf("persist pending auth verification: %w", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	job := &loginJob{
		ID:               jobID,
		Type:             "account_login",
		Status:           "running",
		AccountID:        item.ID,
		PublicRepair:     publicRepair,
		Reauthentication: reauthentication,
		Message:          "Starting Codex device auth login",
		StartedAt:        now,
		UpdatedAt:        now,
	}
	a.jobs[jobID] = job
	a.loginCancels[jobID] = cancel
	// The upstream account id is durable metadata from the prior successful
	// login. Carry it across the asynchronous job so completion can distinguish
	// same-account credential repair from replacing the slot with another
	// upstream identity.
	go a.runLoginJob(ctx, jobID, item.ID, home, expectedUpstreamAccountID, reauthentication, publicRepair)
	return *job, nil
}

func (a *app) runLoginJob(ctx context.Context, jobID, accountID, codexHome, expectedUpstreamAccountID string, reauthentication, publicRepair bool) {
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
	if ctx.Err() != nil || a.loginJobCancellationRequested(jobID) {
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
	historyReset := false
	finalizeError := ""
	sidecarAccount := account{}
	syncSidecar := false
	a.mu.Lock()
	if job := a.jobs[jobID]; job != nil && !loginJobCancellationRequestedStatus(job.Status) {
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
				if publicRepair && reauthenticationChangedUpstreamIdentity(auth, expectedUpstreamAccountID, reauthentication) {
					// Public callers may repair only the same upstream identity.
					// Fail before mutating metadata, history, or the sidecar; the
					// durable gate remains set so the legitimate user can retry.
					finalizeError = "Authenticated account did not match this credential"
				} else {
					historyReset = a.applyCompletedDeviceAuthLocked(item, auth, expectedUpstreamAccountID, reauthentication, now)
					job.HistoryReset = historyReset
					if err := a.saveLocked(); err != nil {
						finalizeError = "Unable to persist authenticated account state"
					} else {
						refreshQuotaAccountID = accountID
						if a.usesCliproxySidecar(*item) {
							sidecarAccount = *item
							syncSidecar = true
						}
					}
				}
			} else {
				// A successful CLI exit is not enough to inherit identity-scoped
				// state. If the resulting credential cannot be read and its
				// upstream account id cannot be verified, fail closed instead
				// of reporting that history was preserved.
				finalizeError = "Unable to verify the authenticated account"
			}
		}
	}
	a.mu.Unlock()
	if finalizeError != "" {
		a.finishLoginJob(jobID, "failed", verificationURL, userCode, finalizeError)
		return
	}
	if !continueLogin || a.loginJobCancellationRequested(jobID) {
		if a.loginJobCancellationRequested(jobID) {
			a.finishLoginJob(jobID, "cancelled", verificationURL, userCode, "Codex device auth login cancelled")
		}
		return
	}
	if syncSidecar {
		if err := a.syncCliproxyAuth(sidecarAccount, true); err != nil {
			a.finishLoginJob(jobID, "failed", verificationURL, userCode, "Unable to prepare the account gateway")
			return
		}
	}
	if refreshQuotaAccountID != "" {
		var err error
		if publicRepair {
			_, err = a.refreshAccountQuotaForRepair(ctx, refreshQuotaAccountID, expectedUpstreamAccountID)
		} else {
			_, err = a.refreshAccountQuota(ctx, refreshQuotaAccountID)
		}
		if errors.Is(err, errPublicRepairIdentityMismatch) {
			a.finishLoginJob(jobID, "failed", verificationURL, userCode, "Authenticated account did not match this credential")
			return
		}
		if err != nil && ctx.Err() == nil {
			a.logger.Printf("quota refresh after login skipped for %s: %s", refreshQuotaAccountID, err)
		}
	}
	if ctx.Err() != nil || a.loginJobCancellationRequested(jobID) {
		a.finishLoginJob(jobID, "cancelled", verificationURL, userCode, "Codex device auth login cancelled")
		return
	}
	a.mu.Lock()
	item := a.accountLocked(accountID)
	if item == nil {
		finalizeError = "Account no longer exists"
	} else {
		// Quota/subscription metadata can resolve a more specific workspace id
		// than auth.json. Re-check the final durable identity before releasing
		// the routing gate so that later metadata enrichment cannot smuggle old
		// affinity/history across an account boundary.
		finalIdentityReset := reauthenticationChangedUpstreamIdentity(codexAuthInfo{AccountID: item.AccountID}, expectedUpstreamAccountID, reauthentication)
		if publicRepair && finalIdentityReset {
			// A metadata refresh may resolve a more specific workspace ID than
			// auth.json. Public repair still requires an exact final identity;
			// never release the routing gate or convert this slot for a visitor.
			finalizeError = "Authenticated account did not match this credential"
		} else if finalIdentityReset && !historyReset {
			a.clearAccountIdentityScopedStateLocked(accountID)
			historyReset = true
			if job := a.jobs[jobID]; job != nil {
				job.HistoryReset = true
			}
		}
		if finalizeError == "" {
			item.PendingAuthVerification = false
			item.PendingAuthExpectedAccountID = ""
			if activateAfterFinalize {
				item.Enabled = true
				item.InPool = true
				item.PendingPoolActivation = false
			}
			item.UpdatedAt = time.Now().UTC()
			if err := a.saveCompletedAuthVerificationLocked(); err != nil {
				// Keep the in-memory routing gate aligned with the durable pending
				// marker written at job start. A storage failure must not release a
				// credential whose identity transition was not persisted.
				item.PendingAuthVerification = true
				item.PendingAuthExpectedAccountID = expectedUpstreamAccountID
				finalizeError = "Unable to persist completed sign-in repair"
			}
		}
	}
	a.mu.Unlock()
	if finalizeError != "" {
		a.finishLoginJob(jobID, "failed", verificationURL, userCode, finalizeError)
		return
	}
	message := "Codex device auth login completed"
	if historyReset {
		message = "Codex device auth login completed; account identity changed and cache/affinity history was reset"
	}
	a.finishLoginJob(jobID, "completed", verificationURL, userCode, message)
}

func accountHasPriorIdentity(item account) bool {
	return strings.TrimSpace(item.AccountID) != "" ||
		normalizeEmail(item.Email) != "" ||
		cleanOrganizationName(item.OrganizationName) != "" ||
		!item.LastLoginAt.IsZero()
}

func (a *app) accountHasPriorIdentityLocked(item account) bool {
	if accountHasPriorIdentity(item) {
		return true
	}
	for _, values := range []map[string]promptCacheStat{a.state.PromptCache, a.state.PromptCacheBaseline} {
		for _, stat := range values {
			if stat.AccountID == item.ID {
				return true
			}
		}
	}
	for _, binding := range a.state.StickySessions {
		if binding.AccountID == item.ID {
			return true
		}
	}
	for _, binding := range a.state.ResponseBindings {
		if binding.AccountID == item.ID {
			return true
		}
	}
	for _, binding := range a.state.ThreadBindings {
		if binding.AccountID == item.ID {
			return true
		}
	}
	for _, event := range a.state.RoutingCacheEvents {
		if event.AccountID == item.ID || event.FailoverFromAccountID == item.ID {
			return true
		}
	}
	for _, bucket := range a.throughputBuckets {
		if bucket.AccountID == item.ID {
			return true
		}
	}
	return false
}

func reauthenticationChangedUpstreamIdentity(auth codexAuthInfo, expectedUpstreamAccountID string, reauthentication bool) bool {
	if !reauthentication {
		return false
	}
	previousAccountID := strings.TrimSpace(expectedUpstreamAccountID)
	currentAccountID := strings.TrimSpace(auth.AccountID)
	// A repair may inherit identity-scoped metrics and routes only when the
	// durable upstream account id is reproduced exactly. Email and display
	// organization are intentionally not fallback identity keys: one user can
	// access multiple ChatGPT workspaces, so an unverifiable legacy slot must
	// reset once rather than risk crossing an account boundary.
	return previousAccountID == "" || currentAccountID == "" || currentAccountID != previousAccountID
}

func (a *app) applyCompletedDeviceAuthLocked(item *account, auth codexAuthInfo, expectedUpstreamAccountID string, reauthentication bool, now time.Time) bool {
	historyReset := reauthenticationChangedUpstreamIdentity(auth, expectedUpstreamAccountID, reauthentication)
	if historyReset {
		// Metrics and route bindings belong to an upstream identity, not merely
		// to the reusable local credential directory. A different (or
		// unverifiable) account must start clean or the dashboard would
		// attribute the old account's cache/affinity history to the new one and
		// old threads could be sent across an account boundary.
		a.clearAccountIdentityScopedStateLocked(item.ID)
		item.Email = normalizeEmail(auth.Email)
		item.AccountID = strings.TrimSpace(auth.AccountID)
		item.OrganizationName = cleanOrganizationName(auth.OrganizationName)
		item.RawPlanType = cleanRawPlanType(auth.PlanType)
		item.PlanFamily = planFamilyFromRaw(item.RawPlanType)
		item.PlanType = item.PlanFamily
		item.PlanLimit = cleanPlanLimit(auth.PlanLimit)
		item.PlanRank = planRank(item.PlanFamily)
		item.SeatType = ""
		item.SeatTypeRaw = ""
		item.QuotaPolicy = nil
	} else {
		if auth.Email != "" {
			item.Email = normalizeEmail(auth.Email)
		}
		if auth.AccountID != "" {
			item.AccountID = strings.TrimSpace(auth.AccountID)
		}
		if auth.OrganizationName != "" {
			item.OrganizationName = cleanOrganizationName(auth.OrganizationName)
		}
		if auth.PlanType != "" {
			item.RawPlanType = cleanRawPlanType(auth.PlanType)
			item.PlanFamily = planFamilyFromRaw(item.RawPlanType)
			item.PlanType = item.PlanFamily
			item.PlanRank = planRank(item.PlanFamily)
		}
		if auth.PlanLimit != "" {
			item.PlanLimit = cleanPlanLimit(auth.PlanLimit)
		}
	}
	a.clearAccountRuntimeStateLocked(item.ID)
	item.Label = accountDisplayName(*item)
	item.LastLoginAt = now
	item.UpdatedAt = now
	return historyReset
}

func (a *app) accountLoginInProgressLocked(accountID string) bool {
	return a.activeLoginJobForAccountLocked(accountID) != nil
}

func loginJobActiveStatus(status string) bool {
	switch status {
	case "running", "waiting_for_user", "finalizing", "cancelling":
		return true
	default:
		return false
	}
}

func loginJobCancellationRequestedStatus(status string) bool {
	return status == "cancelling" || status == "cancelled"
}

func (a *app) activeLoginJobForAccountLocked(accountID string) *loginJob {
	for _, job := range a.jobs {
		if job.AccountID == accountID && loginJobActiveStatus(job.Status) {
			return job
		}
	}
	return nil
}

func (a *app) latestPublicRepairJobForAccountLocked(accountID string) *loginJob {
	var latest *loginJob
	for _, job := range a.jobs {
		if job.AccountID != accountID || !job.PublicRepair {
			continue
		}
		if latest == nil || job.StartedAt.After(latest.StartedAt) {
			latest = job
		}
	}
	return latest
}

func (a *app) publicRepairJobActiveLocked(accountID string) bool {
	job := a.activeLoginJobForAccountLocked(accountID)
	return job != nil && job.PublicRepair
}

func (a *app) publicRepairAvailableLocked(item account) bool {
	if !isCodexDeviceAuth(item) || !item.Enabled || !item.InPool {
		return false
	}
	expectedAccountID := strings.TrimSpace(item.AccountID)
	if item.PendingAuthVerification {
		expectedAccountID = strings.TrimSpace(item.PendingAuthExpectedAccountID)
	}
	// Public repair must prove continuity with a prior verified identity. Empty
	// or legacy identity metadata requires authenticated owner intervention.
	if expectedAccountID == "" {
		return false
	}
	if job := a.activeLoginJobForAccountLocked(item.ID); job != nil {
		return job.PublicRepair
	}
	if item.PendingAuthVerification {
		return true
	}
	if _, err := a.codexAuth(item); err != nil {
		return true
	}
	return quotaErrorBlocksRouting(a.state.Quotas[item.ID].QuotaError)
}

func publicRepairJob(job loginJob) map[string]any {
	// This is the complete unauthenticated job projection. Keep raw job/account
	// IDs, CLI output, internal errors, and owner-originated login jobs private.
	result := map[string]any{
		"status":           job.Status,
		"reauthentication": job.Reauthentication,
		"historyReset":     job.HistoryReset,
	}
	if job.VerificationURL != "" {
		result["verificationUrl"] = job.VerificationURL
	}
	if job.UserCode != "" {
		result["userCode"] = job.UserCode
	}
	if !job.CodeExpiresAt.IsZero() {
		result["codeExpiresAt"] = job.CodeExpiresAt
	}
	if !job.StartedAt.IsZero() {
		result["startedAt"] = job.StartedAt
	}
	if !job.UpdatedAt.IsZero() {
		result["updatedAt"] = job.UpdatedAt
	}
	if !job.CompletedAt.IsZero() {
		result["completedAt"] = job.CompletedAt
	}
	return result
}

func (a *app) accountAuthVerificationPendingLocked(item account) bool {
	return item.PendingAuthVerification || a.accountLoginInProgressLocked(item.ID)
}

func (a *app) loginJobStatus(jobID string) string {
	a.mu.RLock()
	defer a.mu.RUnlock()
	if job := a.jobs[jobID]; job != nil {
		return job.Status
	}
	return ""
}

func (a *app) loginJobCancellationRequested(jobID string) bool {
	return loginJobCancellationRequestedStatus(a.loginJobStatus(jobID))
}

func (a *app) cancelLoginJob(jobID string) (context.CancelFunc, loginJob, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	job := a.jobs[jobID]
	if job == nil {
		return nil, loginJob{}, errors.New("job not found")
	}
	switch job.Status {
	case "completed", "failed", "cancelled", "cancelling":
		return nil, *job, nil
	}
	now := time.Now().UTC()
	// Keep cancellation active until CommandContext confirms the process has
	// exited. Marking the job terminal here would let a second Codex login start
	// while the first process is still tearing down, violating single-flight.
	job.Status = "cancelling"
	job.Message = "Cancelling Codex device auth login"
	job.Error = ""
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
		if !loginJobActiveStatus(job.Status) {
			continue
		}
		job.Status = "cancelling"
		job.Message = "Cancelling Codex device auth login because the account was removed"
		job.Error = ""
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
	if loginJobCancellationRequestedStatus(job.Status) || job.Status == "completed" || job.Status == "failed" {
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
	if loginJobCancellationRequestedStatus(job.Status) && status != "cancelled" {
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
	// These raw status counts plus the unavailable roll-up are the source of
	// truth for the cards above the account table. Keep standby and duplicate
	// separate: both can be unroutable, but only standby is actually out of the
	// pool. The visible mutually-exclusive cards must always reconcile to total.
	summary := map[string]int{
		"total":          len(a.config.Accounts),
		"ready":          0,
		"low":            0,
		"protected":      0,
		"exhausted":      0,
		"cooldown":       0,
		"standby":        0,
		"duplicate":      0,
		"error":          0,
		"missing_auth":   0,
		"authenticating": 0,
		"disabled":       0,
		"unavailable":    0,
	}
	for _, item := range a.config.Accounts {
		status, _ := a.accountStatusLocked(item, now)
		if _, ok := summary[status]; ok {
			summary[status]++
		} else {
			summary["unavailable"]++
			continue
		}
		switch status {
		case "protected", "exhausted", "error", "missing_auth", "authenticating", "disabled":
			summary["unavailable"]++
		}
	}
	return summary
}
func (a *app) publicDashboardSummaryLocked(now time.Time) map[string]int {
	detailed := a.dashboardSummaryLocked(now)
	// Public mode exposes only operational buckets, not the auth-specific raw
	// breakdown. These six buckets are mutually exclusive and add up to total.
	summary := map[string]int{
		"total":       detailed["total"],
		"ready":       detailed["ready"],
		"low":         detailed["low"] + detailed["protected"] + detailed["exhausted"],
		"cooldown":    detailed["cooldown"],
		"standby":     detailed["standby"],
		"duplicate":   detailed["duplicate"],
		"unavailable": detailed["unavailable"] - detailed["protected"] - detailed["exhausted"],
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

// routingCacheEventViewsLocked is the only browser-facing representation of
// request-level events. It deliberately omits local/upstream account IDs and
// exposes only masked display labels plus domain-separated hashes, so adding the
// diagnostics panel cannot become a side channel for credentials or raw Codex
// thread/cache identifiers.
func (a *app) routingCacheEventViewsLocked(now time.Time) []map[string]any {
	cutoff := now.Add(-routingCacheEventTTL)
	capacity := min(len(a.state.RoutingCacheEvents), routingCacheEventViewLimit)
	result := make([]map[string]any, 0, capacity)
	for index := len(a.state.RoutingCacheEvents) - 1; index >= 0; index-- {
		event := a.state.RoutingCacheEvents[index]
		if event.Timestamp.Before(cutoff) {
			continue
		}
		accountLabel := "Credential"
		if item, accountIndex := a.accountWithIndexLocked(event.AccountID); item != nil {
			accountLabel = publicCredentialDisplayName(*item, accountIndex)
		}
		failoverFromLabel := ""
		if item, accountIndex := a.accountWithIndexLocked(event.FailoverFromAccountID); item != nil {
			failoverFromLabel = publicCredentialDisplayName(*item, accountIndex)
		}
		result = append(result, map[string]any{
			"timestamp": event.Timestamp, "requestIdHash": event.RequestIDHash, "responseIdHash": event.ResponseIDHash,
			"modelId": event.ModelID, "accountLabel": accountLabel, "accountRef": operationalIdentifierHash("account", event.AccountID),
			"agentKind": event.AgentKind, "threadIdHash": event.ThreadIDHash, "lineageRootIdHash": event.LineageRootIDHash,
			"stickyKeyHash": event.StickyKeyHash, "promptCacheKeyHash": event.PromptCacheKeyHash,
			"routingOutcome": event.RoutingOutcome, "routingSource": event.RoutingSource, "parentAffinity": event.ParentAffinity,
			"terminalEvent": event.TerminalEvent, "terminalFailureClass": event.TerminalFailureClass, "terminalErrorCode": event.TerminalErrorCode,
			"failoverFromAccountLabel": failoverFromLabel, "failoverFromAccountRef": operationalIdentifierHash("account", event.FailoverFromAccountID),
			"usageObserved": event.UsageObserved, "inputTokens": event.InputTokens, "cachedTokens": event.CachedTokens,
			"cacheWriteTokens": event.CacheWriteTokens, "uncachedInputTokens": event.UncachedInputTokens,
			"cacheReadRate": event.CacheReadRate, "cacheWriteRate": event.CacheWriteRate, "cacheReuseBalance": event.CacheReuseBalance,
			"cacheHit": event.CacheHit, "coldCacheEligible": event.ColdCacheEligible,
		})
		// Browser APIs need a compact diagnostic view, not the full persisted
		// rolling buffer. Keep this server-side limit even though the UI also
		// slices defensively, otherwise a future client could pull all 500
		// request correlations on every dashboard refresh.
		if len(result) == routingCacheEventViewLimit {
			break
		}
	}
	return result
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

type throughputAggregate struct {
	requests            uint64
	successes           uint64
	failures            uint64
	cancelled           uint64
	streaming           uint64
	usageObserved       uint64
	outputObserved      uint64
	inputTokens         uint64
	cachedTokens        uint64
	outputTokens        uint64
	totalDurationMillis uint64
	durationHistogram   []uint64
}

func (a *app) throughputSnapshotLocked(accountID string, now time.Time) map[string]any {
	result := map[string]any{
		"bucketIntervalSeconds": int(throughputBucketInterval / time.Second),
		"retentionHours":        int(throughputBucketTTL / time.Hour),
	}
	if accountID != "" {
		// Account rows remain a compact recent operational view; the large
		// 48-hour series is pool-wide so the public payload cannot reveal traffic
		// attribution for an individual credential.
		result["windows"] = map[string]any{"5m": a.throughputWindowLocked(accountID, now, 5*time.Minute)}
		return result
	}
	result["activeRequests"] = a.activeProxyRequests
	result["seriesIntervalSeconds"] = int(throughputSeriesInterval / time.Second)
	result["current"] = a.throughputWindowLocked("", now, throughputSeriesInterval)
	result["series"] = a.throughputSeriesLocked("", now)
	return result
}

func (a *app) throughputWindowLocked(accountID string, now time.Time, window time.Duration) map[string]any {
	cutoff := now.Add(-window)
	var aggregate throughputAggregate
	for _, bucket := range a.throughputBuckets {
		if accountID != "" && bucket.AccountID != accountID {
			continue
		}
		if bucket.BucketAt.After(now) || !bucket.BucketAt.Add(throughputBucketInterval).After(cutoff) {
			continue
		}
		aggregate.add(bucket)
	}
	return throughputAggregateProjection(aggregate, window)
}

func (a *app) throughputSeriesLocked(accountID string, now time.Time) []map[string]any {
	end := now.Truncate(throughputSeriesInterval)
	start := end.Add(-throughputBucketTTL)
	cutoff := now.Add(-throughputBucketTTL)
	aggregates := map[int64]*throughputAggregate{}
	for _, bucket := range a.throughputBuckets {
		if accountID != "" && bucket.AccountID != accountID {
			continue
		}
		if bucket.BucketAt.After(now) || !bucket.BucketAt.Add(throughputBucketInterval).After(cutoff) {
			continue
		}
		at := bucket.BucketAt.Truncate(throughputSeriesInterval)
		if at.Before(start) || at.After(end) {
			continue
		}
		key := at.Unix()
		aggregate := aggregates[key]
		if aggregate == nil {
			aggregate = &throughputAggregate{}
			aggregates[key] = aggregate
		}
		aggregate.add(bucket)
	}
	if len(aggregates) == 0 {
		// An empty slice lets the UI distinguish "no process-memory history yet"
		// from a real 48-hour interval in which the provider was running but idle.
		// Do not manufacture a full chart of zeroes before the first observation.
		return []map[string]any{}
	}
	points := make([]map[string]any, 0, int(throughputBucketTTL/throughputSeriesInterval)+1)
	for at := start; !at.After(end); at = at.Add(throughputSeriesInterval) {
		windowStart := at
		windowEnd := at.Add(throughputSeriesInterval)
		if cutoff.After(windowStart) {
			windowStart = cutoff
		}
		if now.Before(windowEnd) {
			windowEnd = now
		}
		// The first and newest points can be partial fixed buckets. Use their
		// actual visible wall time, with a one-minute floor, so boundary rates
		// are neither diluted by a full ten minutes nor amplified by seconds.
		window := windowEnd.Sub(windowStart)
		if window < throughputBucketInterval {
			window = throughputBucketInterval
		}
		if window > throughputSeriesInterval {
			window = throughputSeriesInterval
		}
		aggregate := throughputAggregate{}
		if stored := aggregates[at.Unix()]; stored != nil {
			aggregate = *stored
		}
		// The public history intentionally carries only the two metrics shown on
		// the correlation chart. Keep request, token-flow, and latency aggregates
		// available for the current/account summaries without shipping unused
		// 48-hour series fields back to every browser refresh.
		point := throughputSeriesPointProjection(aggregate, window)
		point["at"] = at
		points = append(points, point)
	}
	return points
}

func throughputSeriesPointProjection(aggregate throughputAggregate, window time.Duration) map[string]any {
	result := map[string]any{"windowSeconds": int(window / time.Second)}
	addThroughputOutputAndCacheProjection(result, aggregate, window)
	return result
}

func throughputAggregateProjection(aggregate throughputAggregate, window time.Duration) map[string]any {
	minutes := window.Minutes()
	if minutes <= 0 {
		minutes = throughputBucketInterval.Minutes()
	}
	result := map[string]any{
		"windowSeconds":              int(window / time.Second),
		"requestCount":               aggregate.requests,
		"successCount":               aggregate.successes,
		"failureCount":               aggregate.failures,
		"cancelledCount":             aggregate.cancelled,
		"streamingRequestCount":      aggregate.streaming,
		"usageObservedRequestCount":  aggregate.usageObserved,
		"outputObservedRequestCount": aggregate.outputObserved,
		"inputTokens":                aggregate.inputTokens,
		"cachedTokens":               aggregate.cachedTokens,
		"outputTokens":               aggregate.outputTokens,
		"requestsPerMinute":          float64(aggregate.requests) / minutes,
		"inputTokensPerMinute":       float64(aggregate.inputTokens) / minutes,
		"cachedTokensPerMinute":      float64(aggregate.cachedTokens) / minutes,
		"outputTokensPerMinute":      float64(aggregate.outputTokens) / minutes,
		"averageLatencyMs":           averageMillis(aggregate.totalDurationMillis, aggregate.requests),
		"p50LatencyMs":               histogramPercentileMillis(aggregate.durationHistogram, 50),
		"p95LatencyMs":               histogramPercentileMillis(aggregate.durationHistogram, 95),
	}
	addThroughputOutputAndCacheProjection(result, aggregate, window)
	if aggregate.requests > 0 {
		result["successRate"] = float64(aggregate.successes) / float64(aggregate.requests)
	} else {
		result["successRate"] = nil
	}
	return result
}

func addThroughputOutputAndCacheProjection(result map[string]any, aggregate throughputAggregate, window time.Duration) {
	if aggregate.outputObserved > 0 {
		// This is actual rolling aggregate throughput: all observed output
		// tokens divided by the wall-clock window. Do not replace it with summed
		// per-request generation time, which under-reports parallel work.
		result["outputTokensPerSecond"] = float64(aggregate.outputTokens) / window.Seconds()
	} else {
		result["outputTokensPerSecond"] = nil
	}
	if aggregate.inputTokens > 0 {
		cached := aggregate.cachedTokens
		if cached > aggregate.inputTokens {
			cached = aggregate.inputTokens
		}
		result["cacheHitRate"] = float64(cached) / float64(aggregate.inputTokens)
	} else {
		result["cacheHitRate"] = nil
	}
}

func (aggregate *throughputAggregate) add(bucket throughputBucket) {
	aggregate.requests += bucket.RequestCount
	aggregate.successes += bucket.SuccessCount
	aggregate.failures += bucket.FailureCount
	aggregate.cancelled += bucket.CancelledCount
	aggregate.streaming += bucket.StreamingRequestCount
	aggregate.usageObserved += bucket.UsageObservedRequestCount
	aggregate.outputObserved += bucket.OutputObservedRequestCount
	aggregate.inputTokens += bucket.InputTokens
	aggregate.cachedTokens += bucket.CachedTokens
	aggregate.outputTokens += bucket.OutputTokens
	aggregate.totalDurationMillis += bucket.TotalDurationMillis
	aggregate.durationHistogram = addHistograms(aggregate.durationHistogram, bucket.DurationHistogram)
}

func addHistograms(total, values []uint64) []uint64 {
	if len(values) == 0 {
		return total
	}
	if len(total) < len(values) {
		resized := make([]uint64, len(values))
		copy(resized, total)
		total = resized
	}
	for index, value := range values {
		total[index] += value
	}
	return total
}

func averageMillis(total, count uint64) any {
	if count == 0 {
		return nil
	}
	return float64(total) / float64(count)
}

func histogramPercentileMillis(histogram []uint64, percentile uint64) any {
	var total uint64
	for _, count := range histogram {
		total += count
	}
	if total == 0 {
		return nil
	}
	target := (total*percentile + 99) / 100
	var seen uint64
	for index, count := range histogram {
		seen += count
		if seen < target {
			continue
		}
		if index < len(throughputLatencyBoundsMillis) {
			return throughputLatencyBoundsMillis[index]
		}
		return throughputLatencyBoundsMillis[len(throughputLatencyBoundsMillis)-1]
	}
	return nil
}

// promptCacheWindowFilteredLocked computes the since-baseline deltas. An empty
// accountID aggregates every account.
func (a *app) promptCacheWindowFilteredLocked(accountID string, resetAt time.Time) map[string]any {
	var input, cached, requests, usageObserved, cacheWrites, cacheWriteInput, cacheWriteObserved, cacheHits, cacheEligible, cold uint64
	var parentAffinityHits, parentAffinityFallbacks, lineageFailovers, routingFailovers uint64
	agents := map[string]map[string]uint64{
		"main": {
			"inputTokens": 0, "cachedTokens": 0, "requestCount": 0, "usageObservedRequestCount": 0,
			"cacheWriteTokens": 0, "cacheWriteInputTokens": 0, "cacheWriteObservedRequestCount": 0,
			"cacheHitRequestCount": 0, "cacheEligibleRequestCount": 0, "coldRequestCount": 0,
			"parentAffinityHitCount": 0, "parentAffinityFallbackCount": 0, "lineageFailoverCount": 0, "routingFailoverCount": 0,
		},
		"subagent": {
			"inputTokens": 0, "cachedTokens": 0, "requestCount": 0, "usageObservedRequestCount": 0,
			"cacheWriteTokens": 0, "cacheWriteInputTokens": 0, "cacheWriteObservedRequestCount": 0,
			"cacheHitRequestCount": 0, "cacheEligibleRequestCount": 0, "coldRequestCount": 0,
			"parentAffinityHitCount": 0, "parentAffinityFallbackCount": 0, "lineageFailoverCount": 0, "routingFailoverCount": 0,
		},
	}
	for key, stat := range a.state.PromptCache {
		if accountID != "" && stat.AccountID != accountID {
			continue
		}
		base := a.state.PromptCacheBaseline[key]
		inputDelta := subSat(stat.InputTokens, base.InputTokens)
		cachedDelta := subSat(stat.CachedTokens, base.CachedTokens)
		requestDelta := subSat(stat.RequestCount, base.RequestCount)
		usageObservedDelta := subSat(stat.UsageObservedRequestCount, base.UsageObservedRequestCount)
		cacheWriteDelta := subSat(stat.CacheWriteTokens, base.CacheWriteTokens)
		cacheWriteInputDelta := subSat(stat.CacheWriteInputTokens, base.CacheWriteInputTokens)
		cacheWriteObservedDelta := subSat(stat.CacheWriteObservedRequestCount, base.CacheWriteObservedRequestCount)
		cacheHitDelta := subSat(stat.CacheHitRequestCount, base.CacheHitRequestCount)
		cacheEligibleDelta := subSat(stat.CacheEligibleRequestCount, base.CacheEligibleRequestCount)
		coldDelta := subSat(stat.ColdRequestCount, base.ColdRequestCount)
		input += inputDelta
		cached += cachedDelta
		requests += requestDelta
		usageObserved += usageObservedDelta
		cacheWrites += cacheWriteDelta
		cacheWriteInput += cacheWriteInputDelta
		cacheWriteObserved += cacheWriteObservedDelta
		cacheHits += cacheHitDelta
		cacheEligible += cacheEligibleDelta
		cold += coldDelta
		parentAffinityHits += subSat(stat.ParentAffinityHitCount, base.ParentAffinityHitCount)
		parentAffinityFallbacks += subSat(stat.ParentAffinityFallbackCount, base.ParentAffinityFallbackCount)
		lineageFailovers += subSat(stat.LineageFailoverCount, base.LineageFailoverCount)
		routingFailovers += subSat(stat.RoutingFailoverCount, base.RoutingFailoverCount)
		agentKind := stat.AgentKind
		if agentKind != "subagent" {
			agentKind = "main"
		}
		agents[agentKind]["inputTokens"] += inputDelta
		agents[agentKind]["cachedTokens"] += cachedDelta
		agents[agentKind]["requestCount"] += requestDelta
		agents[agentKind]["usageObservedRequestCount"] += usageObservedDelta
		agents[agentKind]["cacheWriteTokens"] += cacheWriteDelta
		agents[agentKind]["cacheWriteInputTokens"] += cacheWriteInputDelta
		agents[agentKind]["cacheWriteObservedRequestCount"] += cacheWriteObservedDelta
		agents[agentKind]["cacheHitRequestCount"] += cacheHitDelta
		agents[agentKind]["cacheEligibleRequestCount"] += cacheEligibleDelta
		agents[agentKind]["coldRequestCount"] += coldDelta
		agents[agentKind]["parentAffinityHitCount"] += subSat(stat.ParentAffinityHitCount, base.ParentAffinityHitCount)
		agents[agentKind]["parentAffinityFallbackCount"] += subSat(stat.ParentAffinityFallbackCount, base.ParentAffinityFallbackCount)
		agents[agentKind]["lineageFailoverCount"] += subSat(stat.LineageFailoverCount, base.LineageFailoverCount)
		agents[agentKind]["routingFailoverCount"] += subSat(stat.RoutingFailoverCount, base.RoutingFailoverCount)
	}
	return map[string]any{
		"inputTokens": input, "cachedTokens": cached, "requestCount": requests, "usageObservedRequestCount": usageObserved,
		"cacheWriteTokens": cacheWrites, "cacheWriteInputTokens": cacheWriteInput, "cacheWriteObservedRequestCount": cacheWriteObserved,
		"cacheHitRequestCount": cacheHits, "cacheEligibleRequestCount": cacheEligible, "coldRequestCount": cold,
		"main": agents["main"], "subagent": agents["subagent"],
		"parentAffinityHitCount": parentAffinityHits, "parentAffinityFallbackCount": parentAffinityFallbacks,
		"lineageFailoverCount": lineageFailovers, "routingFailoverCount": routingFailovers, "resetAt": resetAt,
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
	quota := quotaSnapshotForDisplay(a.state.Quotas[item.ID], now)
	cacheInput, cacheCached, cacheRequests := a.promptCacheStatsForAccountLocked(item.ID)
	result := map[string]any{"accountId": item.ID, "available": status == "ready" || status == "low", "status": status, "statusReason": reason, "cooldowns": cooldowns, "lastSuccessAt": health.LastSuccessAt, "lastFailureAt": health.LastFailureAt, "lastFailureReason": health.LastFailureReason, "consecutiveFailure": health.ConsecutiveFailure, "active": accountActiveLocked(health, now), "activeRouteCount": a.activeRouteCountLocked(item.ID, now), "cacheInputTokens": cacheInput, "cacheCachedTokens": cacheCached, "cacheRequestCount": cacheRequests, "cacheWindow": a.promptCacheWindowForAccountLocked(item.ID), "throughput": a.throughputSnapshotLocked(item.ID, now), "remainingQuota": item.RemainingQuota, "quota": quota.Quota, "usageUpdatedAt": quota.UsageUpdatedAt, "lastSuccessfulRefreshAt": quota.LastSuccessfulRefreshAt, "quotaFreshness": quota.Freshness, "quotaProvenance": quota.Provenance, "quotaError": quota.QuotaError, "quotaProtection": a.quotaProtectionStatusLocked(item, quota, now), "quotaMetering": quotaMeteringKind(item)}
	if job := a.activeLoginJobForAccountLocked(item.ID); job != nil {
		// This endpoint is management-only. Returning the active job lets a page
		// reload recover the device URL/code instead of orphaning a live login.
		result["loginJob"] = *job
	}
	return result
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
	quota := quotaSnapshotForDisplay(a.state.Quotas[item.ID], now)
	displayItem := item
	if quota.OrganizationName != "" {
		displayItem.OrganizationName = quota.OrganizationName
	}
	if quota.PlanType != "" {
		displayItem.PlanType = quota.PlanType
		displayItem.PlanFamily = quota.PlanFamily
		displayItem.RawPlanType = quota.RawPlanType
		displayItem.PlanRank = planRank(quota.PlanType)
	}
	if quota.PlanLimit != "" {
		displayItem.PlanLimit = quota.PlanLimit
	}
	displayItem.SeatType = quota.SeatType
	displayItem.SeatTypeRaw = quota.SeatTypeRaw
	displayItem.QuotaPolicy = append([]string(nil), quota.QuotaPolicy...)
	remainingQuota := displayItem.RemainingQuota
	if remainingQuota == nil && quota.Quota != nil {
		remaining := remainingQuotaHint(*quota.Quota)
		remainingQuota = &remaining
	}
	metadata := credentialMetadata(displayItem)
	return map[string]any{
		"label":                   currentAccountDisplayName(displayItem, index),
		"displayName":             currentAccountDisplayName(displayItem, index),
		"credentialMetadata":      metadata,
		"email":                   metadata["email"],
		"organizationName":        metadata["organizationName"],
		"planType":                metadata["planType"],
		"planLimit":               metadata["planLimit"],
		"planDisplayName":         metadata["planDisplayName"],
		"planRank":                metadata["planRank"],
		"status":                  status,
		"statusReason":            reason,
		"available":               status == "ready" || status == "low",
		"remainingQuota":          remainingQuota,
		"quota":                   quota.Quota,
		"usageUpdatedAt":          quota.UsageUpdatedAt,
		"lastSuccessfulRefreshAt": quota.LastSuccessfulRefreshAt,
		"quotaFreshness":          quota.Freshness,
		"quotaProvenance":         quota.Provenance,
		"quotaError":              quota.QuotaError,
		"quotaProtection":         a.quotaProtectionStatusLocked(item, quota, now),
		"quotaMetering":           quotaMeteringKind(item),
	}
}

func (a *app) accountStatusLocked(item account, now time.Time) (string, string) {
	if a.accountLoginInProgressLocked(item.ID) {
		return "authenticating", "Sign-in repair is in progress"
	}
	if item.PendingAuthVerification {
		return "missing_auth", "Sign-in repair must be completed before routing resumes"
	}
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
	quotaSnapshot := a.state.Quotas[item.ID]
	// A proven credential failure is more direct and actionable than duplicate
	// identity. Keep it ahead of duplicate classification so an in-pool slot that
	// needs Repair is counted as Unavailable rather than falsely inflating the
	// Out-of-pool/Duplicate cards. Healthy sibling copies still show Duplicate.
	//
	// Transient quota polling failures remain diagnostic history rather than
	// availability gates; quotaErrorBlocksRouting only accepts errors that prove
	// the credential itself cannot route requests.
	if quotaErrorBlocksRouting(quotaSnapshot.QuotaError) {
		reason := "Quota refresh failed"
		if quotaSnapshot.QuotaError.Code != "" {
			reason += ": " + quotaSnapshot.QuotaError.Code
		}
		return "error", reason
	}
	// Check duplicate identity before cooldown/quota so the operator sees the
	// structural reason this slot is not routable. The primary slot owns the
	// upstream identity; runtime cooldown and quota are evaluated on that slot.
	// Credential failures are intentionally handled above because Repair is more
	// urgent than the otherwise healthy duplicate relationship.
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
	if quotaSnapshot.Quota != nil {
		if quotaExplicitlyBlocksRouting(*quotaSnapshot.Quota, "", now) {
			reason := "Quota exhausted"
			if quotaSnapshot.Quota.ExhaustionReason != "" {
				reason += ": " + quotaSnapshot.Quota.ExhaustionReason
			}
			return "exhausted", reason
		}
		if protection := a.quotaProtectionStatusLocked(item, quotaSnapshot, now); protection.Blocked {
			return "protected", protection.Message
		}
		remaining := remainingQuotaHint(*quotaSnapshot.Quota)
		if remaining <= 20 {
			return "low", "Quota window is at or below 20%"
		}
	}
	if item.RemainingQuota != nil {
		if *item.RemainingQuota <= 0 {
			return "exhausted", "Remaining quota is exhausted"
		}
		if *item.RemainingQuota <= 20 {
			return "low", "Remaining quota is at or below 20%"
		}
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
	return map[string]any{"id": item.ID, "label": displayName, "displayName": displayName, "ownerNote": item.OwnerNote, "credentialMetadata": metadata, "email": metadata["email"], "organizationName": metadata["organizationName"], "rawPlanType": metadata["rawPlanType"], "planFamily": metadata["planFamily"], "planType": metadata["planType"], "planLimit": metadata["planLimit"], "seatType": metadata["seatType"], "seatTypeRaw": metadata["seatTypeRaw"], "seatTypeDisplay": metadata["seatTypeDisplay"], "quotaPolicy": metadata["quotaPolicy"], "planDisplayName": metadata["planDisplayName"], "planRank": metadata["planRank"], "authType": item.AuthType, "enabled": item.Enabled, "inPool": item.InPool, "priority": item.Priority, "remainingQuota": item.RemainingQuota, "quotaProtectionEnabled": item.QuotaProtectionEnabled, "quotaProtectionThreshold": item.QuotaProtectionThreshold, "quotaMetering": quotaMeteringKind(item), "allowedModels": item.AllowedModels, "excludedModels": item.ExcludedModels, "wireApi": item.WireAPI, "hasUpstreamApiKey": item.UpstreamAPIKey != "", "lastLoginAt": item.LastLoginAt}
}

func (a *app) publicDashboardAccountLocked(item account, index int, now time.Time) map[string]any {
	status, _ := a.accountStatusLocked(item, now)
	quota := quotaSnapshotForDisplay(a.state.Quotas[item.ID], now)
	displayItem := item
	if quota.OrganizationName != "" {
		displayItem.OrganizationName = quota.OrganizationName
	}
	if quota.PlanType != "" {
		displayItem.PlanType = quota.PlanType
		displayItem.PlanFamily = quota.PlanFamily
		displayItem.RawPlanType = quota.RawPlanType
		displayItem.PlanRank = planRank(quota.PlanType)
	}
	if quota.PlanLimit != "" {
		displayItem.PlanLimit = quota.PlanLimit
	}
	displayItem.SeatType = quota.SeatType
	displayItem.SeatTypeRaw = quota.SeatTypeRaw
	displayItem.QuotaPolicy = append([]string(nil), quota.QuotaPolicy...)
	statusTone, statusLabel := publicDashboardStatus(status)
	remainingQuota := displayItem.RemainingQuota
	if remainingQuota == nil && quota.Quota != nil {
		remaining := remainingQuotaHint(*quota.Quota)
		remainingQuota = &remaining
	}
	cacheInput, cacheCached, cacheRequests := a.promptCacheStatsForAccountLocked(item.ID)
	repairAvailable := a.publicRepairAvailableLocked(item)
	return map[string]any{
		"displayName":             publicDashboardAccountLabel(displayItem, index),
		"detail":                  publicDashboardAccountDetail(displayItem),
		"ownerNote":               item.OwnerNote,
		"statusTone":              statusTone,
		"statusLabel":             statusLabel,
		"poolLabel":               publicPoolLabel(item),
		"poolRef":                 a.publicAccountRefLocked(item.ID),
		"poolAction":              publicPoolAction(item, repairAvailable),
		"poolActionLabel":         publicPoolActionLabel(item, repairAvailable, a.publicRepairJobActiveLocked(item.ID)),
		"remainingQuota":          remainingQuota,
		"quota":                   quota.Quota,
		"quotaUnavailable":        quota.QuotaError != nil,
		"quotaFreshness":          quota.Freshness,
		"lastSuccessfulRefreshAt": quota.LastSuccessfulRefreshAt,
		"quotaMetering":           quotaMeteringKind(item),
		"active":                  accountActiveLocked(a.state.Health[item.ID], now),
		"cacheInputTokens":        cacheInput,
		"cacheCachedTokens":       cacheCached,
		"cacheRequestCount":       cacheRequests,
		"cacheWindow":             a.promptCacheWindowForAccountLocked(item.ID),
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
	case "protected":
		return "low", "Protected"
	case "exhausted":
		return "low", "Exhausted"
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
	if item.PendingAuthVerification {
		return "Verification pending"
	}
	if !item.Enabled {
		return "Unavailable"
	}
	if !item.InPool {
		return "Out of pool"
	}
	return "In pool"
}

func publicPoolAction(item account, repairAvailable bool) string {
	if repairAvailable {
		return "repair"
	}
	if item.PendingAuthVerification {
		return ""
	}
	if item.InPool {
		return "pool-remove"
	}
	return "pool-add"
}

func publicPoolActionLabel(item account, repairAvailable, repairActive bool) string {
	if repairAvailable {
		if repairActive {
			return "Continue repair"
		}
		return "Repair"
	}
	if item.PendingAuthVerification {
		return ""
	}
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

func normalizedOwnerNote(value string) string {
	// Notes are rendered on an unauthenticated page. Collapse whitespace and
	// remove control characters at the storage boundary so every UI/API caller
	// receives the same compact plain-text value.
	var safe strings.Builder
	for _, r := range value {
		if unicode.IsControl(r) && !unicode.IsSpace(r) {
			continue
		}
		safe.WriteRune(r)
	}
	return strings.Join(strings.Fields(safe.String()), " ")
}

func cleanOwnerNote(value string) string {
	value = normalizedOwnerNote(value)
	runes := []rune(value)
	if len(runes) > maxOwnerNoteRunes {
		value = string(runes[:maxOwnerNoteRunes])
	}
	return value
}

func validateOwnerNote(value string) (string, error) {
	for _, r := range value {
		if unicode.IsControl(r) && !unicode.IsSpace(r) {
			return "", errors.New("account note must contain plain text only")
		}
	}
	value = normalizedOwnerNote(value)
	if len([]rune(value)) > maxOwnerNoteRunes {
		return "", fmt.Errorf("account note must be %d characters or fewer", maxOwnerNoteRunes)
	}
	return value, nil
}

func effectiveOrganizationName(item account) string {
	return cleanOrganizationName(item.OrganizationName)
}

func credentialMetadata(item account) map[string]any {
	planFamily := effectivePlanFamily(item)
	seatType := cleanMetadataToken(item.SeatType)
	seatDisplay := "Not reported"
	if seatType == "standard" {
		seatDisplay = "Standard"
	} else if seatType == "premium" {
		seatDisplay = "Premium"
	} else if raw := cleanMetadataToken(item.SeatTypeRaw); raw != "" {
		seatDisplay = raw
	}
	return map[string]any{
		"email":            maskedPublicEmail(item.Email),
		"organizationName": publicOrganizationName(effectiveOrganizationName(item)),
		"rawPlanType":      cleanRawPlanType(item.RawPlanType),
		"planFamily":       planFamily,
		"planType":         planFamily,
		"planLimit":        cleanPlanLimit(item.PlanLimit),
		"planDisplayName":  accountPlanDisplayName(item, false),
		"planRank":         item.PlanRank,
		"seatType":         seatType,
		"seatTypeRaw":      cleanMetadataToken(item.SeatTypeRaw),
		"seatTypeDisplay":  seatDisplay,
		"quotaPolicy":      append([]string(nil), item.QuotaPolicy...),
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
	value = strings.ToLower(strings.TrimSpace(value))
	switch value {
	case "5x", "10x", "20x":
		return value
	default:
		return ""
	}
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
	case "free", "go", "plus", "pro", "pro_lite", "team", "business", "enterprise", "edu":
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

func cleanMetadataText(value string, maxRunes int) string {
	var builder strings.Builder
	for _, character := range strings.TrimSpace(value) {
		if unicode.IsControl(character) {
			continue
		}
		builder.WriteRune(character)
	}
	value = strings.Join(strings.Fields(builder.String()), " ")
	runes := []rune(value)
	if maxRunes > 0 && len(runes) > maxRunes {
		value = string(runes[:maxRunes])
	}
	return value
}

func cleanMetadataToken(value string) string {
	value = strings.ToLower(strings.TrimSpace(value))
	if value == "" || len(value) > 80 {
		return ""
	}
	for _, character := range value {
		if !(character >= 'a' && character <= 'z') &&
			!(character >= '0' && character <= '9') &&
			character != '_' && character != '-' && character != '.' {
			return ""
		}
	}
	return value
}

func cleanRawPlanType(value string) string {
	return cleanMetadataToken(value)
}

func planFamilyFromRaw(value string) string {
	switch cleanRawPlanType(value) {
	case "free":
		return "free"
	case "go":
		return "go"
	case "plus", "chatgptplus", "chatgpt_plus":
		return "plus"
	case "pro", "chatgptpro", "chatgpt_pro", "chatgptproplan":
		return "pro"
	case "prolite":
		return "pro_lite"
	case "team", "chatgptteam", "chatgpt_team", "chatgptteamplan", "self_serve_business_usage_based":
		return "business"
	case "business", "enterprise_cbp_usage_based", "enterprise", "hc":
		return "enterprise"
	case "education", "edu":
		return "edu"
	default:
		return "unknown"
	}
}

func normalizeAccountPlanMetadata(item *account) {
	item.RawPlanType = cleanRawPlanType(item.RawPlanType)
	if item.RawPlanType != "" {
		item.PlanFamily = planFamilyFromRaw(item.RawPlanType)
	} else if strings.TrimSpace(item.PlanFamily) != "" {
		item.PlanFamily = normalizePlanType(item.PlanFamily)
	} else {
		// Legacy config stored only the already-normalized planType. Do not apply
		// raw-wire remapping here or an old Business family would become
		// Enterprise merely because the new rawPlanType field was absent.
		item.PlanFamily = normalizePlanType(item.PlanType)
	}
	item.PlanType = item.PlanFamily
	switch cleanMetadataToken(item.SeatType) {
	case "standard", "premium":
		item.SeatType = cleanMetadataToken(item.SeatType)
	default:
		item.SeatType = ""
	}
	item.SeatTypeRaw = cleanMetadataToken(item.SeatTypeRaw)
	policies := make([]string, 0, len(item.QuotaPolicy))
	for _, policy := range item.QuotaPolicy {
		if value := cleanMetadataToken(policy); value != "" {
			policies = append(policies, value)
		}
	}
	item.QuotaPolicy = policies
}

func effectivePlanFamily(item account) string {
	if family := strings.TrimSpace(item.PlanFamily); family != "" {
		return normalizePlanType(family)
	}
	return normalizePlanType(item.PlanType)
}

func planRank(plan string) int {
	switch normalizePlanType(plan) {
	case "enterprise":
		return 500
	case "team", "business":
		return 400
	case "pro":
		return 300
	case "pro_lite":
		return 250
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
	case "pro_lite":
		return "Pro Lite"
	case "go":
		return "Go"
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
	plan := effectivePlanFamily(item)
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

func combinedListenAddressFromEnv() string {
	if address := strings.TrimSpace(os.Getenv("CODEX_POOL_ADDR")); address != "" {
		return address
	}
	// CODEX_POOL_PUBLIC_ADDR was the provider-only listener before the HTTP
	// surfaces were merged. Preserve it as a compatibility alias so an existing
	// deployment does not unexpectedly move ports during upgrade.
	return envOr("CODEX_POOL_PUBLIC_ADDR", listenAddressDefault)
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
