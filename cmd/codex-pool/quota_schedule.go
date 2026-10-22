package main

import (
	"context"
	"math/rand/v2"
	"time"
)

// The background quota poll does not run like a clock, and asks about an idle
// account far less often than about one in use.
//
// It once asked upstream about every account in one burst, within the same
// second, every five minutes exactly. From one address, several accounts swept
// together on a fixed beat is what an automated client looks like, and it ties
// the accounts to each other for anyone watching. Each account now has its own
// schedule: its next poll lands a random 4 to 6 minutes after its last one
// finished, the first round after a start is spread over quotaPollStartSpread,
// and at most one account is asked at a time, with a random pause before the
// next one due. Do not go back to a shared ticker or a sweep: that brings the
// burst back.
//
// That cadence is for an account in use, one that served a request within
// quotaPollActiveWindow. An idle account is asked every 20 to 30 minutes
// instead, and is due at the in-use cadence again as soon as a request lands
// on it, so the first turn after a quiet spell still sees a fresh quota. A
// native Codex reads usage only while it is open (every 60 seconds, more often
// near exhaustion, timed from the end of the last read: tui
// app/rate_limit_refresh.rs poll_deadline, chatwidget/rate_limits.rs
// rate_limit_refresh_interval) and not at all while nobody uses it; polling
// idle accounts at the in-use rate around the clock was most of the pool's own
// load on upstream. Only when a poll is sent is the pool's own; what is sent
// is still the native Codex request of sendCodexOwnRequest.
const (
	quotaRefreshJitter          = time.Minute
	quotaRefreshMaxInterval     = quotaRefreshInterval + quotaRefreshJitter
	quotaIdleRefreshMinInterval = 20 * time.Minute
	quotaIdleRefreshMaxInterval = 30 * time.Minute
	quotaPollActiveWindow       = 30 * time.Minute
	quotaPollStartSpread        = 2 * time.Minute
	quotaPollPauseMin           = 3 * time.Second
	quotaPollPauseMax           = 12 * time.Second
)

// quotaPollPlan is one account's schedule. Both gaps are drawn when a poll
// finishes, so which one applies can change with the account's use without
// redrawing it.
type quotaPollPlan struct {
	first     time.Time
	last      time.Time
	activeGap time.Duration
	idleGap   time.Duration
	// by, when set, brings the next poll forward to decide a waiting prime
	// (codex_prime.go, quota prime grace).
	by time.Time
}

type quotaPollSchedule struct {
	plans  map[string]quotaPollPlan
	random func() float64 // uniform in [0, 1)
}

func newQuotaPollSchedule(random func() float64) *quotaPollSchedule {
	return &quotaPollSchedule{plans: map[string]quotaPollPlan{}, random: random}
}

func (s *quotaPollSchedule) between(low, high time.Duration) time.Duration {
	return low + time.Duration(s.random()*float64(high-low))
}

func (s *quotaPollSchedule) nextAt(plan quotaPollPlan, active bool) time.Time {
	next := plan.first
	if !plan.last.IsZero() {
		next = plan.last.Add(plan.idleGap)
		if active {
			next = plan.last.Add(plan.activeGap)
		}
	}
	if !plan.by.IsZero() && plan.by.Before(next) {
		return plan.by
	}
	return next
}

// due names the account whose poll is most overdue at now; active reports
// whether an account is in use. An account seen for the first time gets its
// first poll within quotaPollStartSpread; one no longer listed is forgotten.
func (s *quotaPollSchedule) due(now time.Time, accountIDs []string, active func(string) bool) (string, bool) {
	listed := make(map[string]bool, len(accountIDs))
	for _, id := range accountIDs {
		listed[id] = true
		if _, ok := s.plans[id]; !ok {
			s.plans[id] = quotaPollPlan{first: now.Add(s.between(0, quotaPollStartSpread))}
		}
	}
	for id := range s.plans {
		if !listed[id] {
			delete(s.plans, id)
		}
	}
	due := ""
	var dueAt time.Time
	for _, id := range accountIDs {
		at := s.nextAt(s.plans[id], active(id))
		if at.After(now) {
			continue
		}
		if due == "" || at.Before(dueAt) {
			due, dueAt = id, at
		}
	}
	return due, due != ""
}

// polled schedules the account's next poll from when this one finished.
func (s *quotaPollSchedule) polled(accountID string, finished time.Time) {
	s.plans[accountID] = quotaPollPlan{
		last:      finished,
		activeGap: s.between(quotaRefreshInterval-quotaRefreshJitter, quotaRefreshMaxInterval),
		idleGap:   s.between(quotaIdleRefreshMinInterval, quotaIdleRefreshMaxInterval),
	}
}

// pollBy makes the account due no later than at.
func (s *quotaPollSchedule) pollBy(accountID string, at time.Time) {
	if plan, ok := s.plans[accountID]; ok {
		plan.by = at
		s.plans[accountID] = plan
	}
}

func (s *quotaPollSchedule) pause() time.Duration {
	return s.between(quotaPollPauseMin, quotaPollPauseMax)
}

// noteAccountActivity records that a request is going to the account, which
// keeps its quota poll at the in-use cadence.
func (a *app) noteAccountActivity(accountID string) {
	a.accountActivityMu.Lock()
	defer a.accountActivityMu.Unlock()
	if a.accountActivity == nil {
		a.accountActivity = map[string]time.Time{}
	}
	a.accountActivity[accountID] = time.Now()
}

func (a *app) accountInUse(accountID string, now time.Time) bool {
	a.accountActivityMu.Lock()
	defer a.accountActivityMu.Unlock()
	last, ok := a.accountActivity[accountID]
	return ok && now.Sub(last) < quotaPollActiveWindow
}

func (a *app) startQuotaRefresher(ctx context.Context) {
	go func() {
		schedule := newQuotaPollSchedule(rand.Float64)
		for {
			accounts := a.quotaPollAccounts()
			ids := make([]string, 0, len(accounts))
			for _, item := range accounts {
				ids = append(ids, item.ID)
			}
			now := time.Now()
			inUse := func(id string) bool { return a.accountInUse(id, now) }
			if id, ok := schedule.due(now, ids, inUse); ok {
				for _, item := range accounts {
					if item.ID == id {
						a.pollAccountQuota(ctx, item)
						break
					}
				}
				schedule.polled(id, time.Now())
				if at, waiting := a.primeGraceDue(id); waiting {
					schedule.pollBy(id, at)
				}
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(schedule.pause()):
			}
		}
	}()
}
