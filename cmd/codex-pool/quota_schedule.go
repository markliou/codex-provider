package main

import (
	"context"
	"math/rand/v2"
	"time"
)

// The background quota poll does not run like a clock.
//
// It once asked upstream about every account in one burst, within the same
// second, every five minutes exactly. From one address, several accounts swept
// together on a fixed beat is what an automated client looks like, and it ties
// the accounts to each other for anyone watching. Each account now has its own
// schedule: its next poll lands a random 4 to 6 minutes after its last one
// finished, the first round after a start is spread over quotaPollStartSpread,
// and at most one account is asked at a time, with a random pause before the
// next one due. Do not go back to a shared ticker or a sweep: that brings the
// burst back. The mean stays quotaRefreshInterval, and quotaTelemetryFreshness
// and quotaWindowAnchorSlack assume polls at most quotaRefreshMaxInterval
// apart; keep them above it if the jitter grows.
//
// Only when a poll is sent is the pool's own; what is sent is still the native
// Codex request of sendCodexOwnRequest. A native Codex TUI reads usage every
// 60 seconds (more often near exhaustion), timed from the end of the last read
// (tui app/rate_limit_refresh.rs poll_deadline, chatwidget/rate_limits.rs
// rate_limit_refresh_interval). The pool asks each account less often than
// that and, like Codex, times the next poll from when the last one finished,
// so a slow or failed poll cannot make the next one come early.
const (
	quotaRefreshJitter      = time.Minute
	quotaRefreshMaxInterval = quotaRefreshInterval + quotaRefreshJitter
	quotaPollStartSpread    = 2 * time.Minute
	quotaPollPauseMin       = 3 * time.Second
	quotaPollPauseMax       = 12 * time.Second
)

type quotaPollSchedule struct {
	next   map[string]time.Time
	random func() float64 // uniform in [0, 1)
}

func newQuotaPollSchedule(random func() float64) *quotaPollSchedule {
	return &quotaPollSchedule{next: map[string]time.Time{}, random: random}
}

func (s *quotaPollSchedule) between(low, high time.Duration) time.Duration {
	return low + time.Duration(s.random()*float64(high-low))
}

// due names the account whose poll is most overdue at now. An account seen
// for the first time gets its first poll within quotaPollStartSpread; one no
// longer listed is forgotten.
func (s *quotaPollSchedule) due(now time.Time, accountIDs []string) (string, bool) {
	listed := make(map[string]bool, len(accountIDs))
	for _, id := range accountIDs {
		listed[id] = true
		if _, ok := s.next[id]; !ok {
			s.next[id] = now.Add(s.between(0, quotaPollStartSpread))
		}
	}
	for id := range s.next {
		if !listed[id] {
			delete(s.next, id)
		}
	}
	due := ""
	var dueAt time.Time
	for _, id := range accountIDs {
		at := s.next[id]
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
	s.next[accountID] = finished.Add(s.between(quotaRefreshInterval-quotaRefreshJitter, quotaRefreshMaxInterval))
}

func (s *quotaPollSchedule) pause() time.Duration {
	return s.between(quotaPollPauseMin, quotaPollPauseMax)
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
			if id, ok := schedule.due(time.Now(), ids); ok {
				for _, item := range accounts {
					if item.ID == id {
						a.pollAccountQuota(ctx, item)
						break
					}
				}
				schedule.polled(id, time.Now())
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(schedule.pause()):
			}
		}
	}()
}
