package main

import (
	"fmt"
	"math/rand/v2"
	"testing"
	"time"
)

// Simulates three hours of the background poll over ten accounts: no two
// polls come together, one account's polls are 4 to 6 minutes apart and not
// all alike, and the first round is spread out instead of a burst.
func TestQuotaPollScheduleHasNoFixedBeat(t *testing.T) {
	schedule := newQuotaPollSchedule(rand.New(rand.NewPCG(1, 2)).Float64)
	ids := make([]string, 10)
	for i := range ids {
		ids[i] = fmt.Sprintf("acct-%d", i)
	}
	start := time.Date(2026, 10, 21, 0, 0, 0, 0, time.UTC)
	const pollTakes = time.Second
	polls := map[string][]time.Time{}
	var last time.Time
	for now := start; now.Before(start.Add(3 * time.Hour)); {
		if id, ok := schedule.due(now, ids); ok {
			if !last.IsZero() && now.Sub(last) < quotaPollPauseMin {
				t.Fatalf("two polls %s apart", now.Sub(last))
			}
			last = now
			polls[id] = append(polls[id], now)
			now = now.Add(pollTakes)
			schedule.polled(id, now)
		}
		now = now.Add(schedule.pause())
	}
	firstRound := quotaPollStartSpread + time.Duration(len(ids))*(quotaPollPauseMax+pollTakes)
	for _, id := range ids {
		times := polls[id]
		if len(times) < 25 {
			t.Fatalf("%s polled %d times in three hours", id, len(times))
		}
		if first := times[0].Sub(start); first > firstRound {
			t.Fatalf("%s first polled %s after the start", id, first)
		}
		gaps := map[time.Duration]bool{}
		for i := 1; i < len(times); i++ {
			gap := times[i].Sub(times[i-1])
			low := quotaRefreshInterval - quotaRefreshJitter + pollTakes
			high := quotaRefreshMaxInterval + pollTakes + time.Duration(len(ids))*(quotaPollPauseMax+pollTakes)
			if gap < low || gap > high {
				t.Fatalf("%s polls %s apart, outside [%s, %s]", id, gap, low, high)
			}
			gaps[gap.Truncate(time.Second)] = true
		}
		if len(gaps) < len(times)/2 {
			t.Fatalf("%s polls on a beat: %d distinct gaps in %d", id, len(gaps), len(times)-1)
		}
	}
	if len(polls) != len(ids) {
		t.Fatalf("%d of %d accounts polled", len(polls), len(ids))
	}
}

func TestQuotaPollScheduleFollowsTheAccountList(t *testing.T) {
	schedule := newQuotaPollSchedule(func() float64 { return 0 })
	now := time.Date(2026, 10, 21, 0, 0, 0, 0, time.UTC)
	if id, ok := schedule.due(now, []string{"a"}); !ok || id != "a" {
		t.Fatalf("a new account is not due: %q %v", id, ok)
	}
	schedule.polled("a", now)
	if _, ok := schedule.due(now.Add(time.Minute), []string{"a"}); ok {
		t.Fatal("an account was due again a minute after its poll")
	}
	if _, ok := schedule.due(now, []string{"b"}); !ok {
		t.Fatal("an added account was not scheduled")
	}
	if _, kept := schedule.next["a"]; kept {
		t.Fatal("a removed account kept its schedule")
	}
}
