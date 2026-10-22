package main

import (
	"fmt"
	"math/rand/v2"
	"testing"
	"time"
)

// Simulates three hours of the background poll over ten accounts in use: no
// two polls come together, one account's polls are 4 to 6 minutes apart and
// not all alike, and the first round is spread out instead of a burst.
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
		if id, ok := schedule.due(now, ids, func(string) bool { return true }); ok {
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
	inUse := func(string) bool { return true }
	if id, ok := schedule.due(now, []string{"a"}, inUse); !ok || id != "a" {
		t.Fatalf("a new account is not due: %q %v", id, ok)
	}
	schedule.polled("a", now)
	if _, ok := schedule.due(now.Add(time.Minute), []string{"a"}, inUse); ok {
		t.Fatal("an account was due again a minute after its poll")
	}
	if _, ok := schedule.due(now, []string{"b"}, inUse); !ok {
		t.Fatal("an added account was not scheduled")
	}
	if _, kept := schedule.plans["a"]; kept {
		t.Fatal("a removed account kept its schedule")
	}
}

// An idle account is asked every 20 to 30 minutes, and is due at the in-use
// cadence the moment a request lands on it.
func TestQuotaPollScheduleAsksIdleAccountsLess(t *testing.T) {
	schedule := newQuotaPollSchedule(rand.New(rand.NewPCG(3, 4)).Float64)
	ids := []string{"idle", "busy"}
	busy := map[string]bool{"busy": true}
	active := func(id string) bool { return busy[id] }
	start := time.Date(2026, 10, 21, 0, 0, 0, 0, time.UTC)
	polls := map[string][]time.Time{}
	for now := start; now.Before(start.Add(4 * time.Hour)); {
		if id, ok := schedule.due(now, ids, active); ok {
			polls[id] = append(polls[id], now)
			now = now.Add(time.Second)
			schedule.polled(id, now)
		}
		now = now.Add(schedule.pause())
	}
	slack := 2 * (quotaPollPauseMax + time.Second)
	for i := 1; i < len(polls["idle"]); i++ {
		if gap := polls["idle"][i].Sub(polls["idle"][i-1]); gap < quotaIdleRefreshMinInterval || gap > quotaIdleRefreshMaxInterval+slack {
			t.Fatalf("idle account polled %s apart", gap)
		}
	}
	if len(polls["busy"]) < 3*len(polls["idle"]) {
		t.Fatalf("an account in use was not asked more often: %d busy, %d idle", len(polls["busy"]), len(polls["idle"]))
	}
	// Use resumes: the idle account, last asked over six minutes ago, is due.
	last := polls["idle"][len(polls["idle"])-1]
	busy["idle"] = true
	now := last.Add(quotaRefreshMaxInterval + 2*time.Second)
	if id, ok := schedule.due(now, []string{"idle"}, active); !ok || id != "idle" {
		t.Fatal("an account back in use waited for its idle poll")
	}
}

func TestAccountActivitySetsTheCadence(t *testing.T) {
	a := testApp(t, nil)
	now := time.Now()
	if a.accountInUse("acct", now) {
		t.Fatal("an account nothing went to is in use")
	}
	a.noteAccountActivity("acct")
	if !a.accountInUse("acct", time.Now()) {
		t.Fatal("a request did not put the account in use")
	}
	if a.accountInUse("acct", time.Now().Add(quotaPollActiveWindow+time.Second)) {
		t.Fatal("the account stayed in use past the active window")
	}
}
