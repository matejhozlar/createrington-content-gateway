use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{Json, extract::State};
use jiff::{Timestamp, fmt::rfc2822};
use serde::Serialize;

use crate::app::AppState;

const MINUTE_MS: u64 = 60_000;
const HOLD_WHEN_NOT_TOLD_MS: u64 = 30_000;
const SHORTEST_HOLD_MS: u64 = 1_000;
const LONGEST_HOLD_MS: u64 = 10 * MINUTE_MS;
const DAYS_KEPT: usize = 14;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

/// Milliseconds since the Unix epoch. Swapped for a fixed clock in tests.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

/// Why calls were not let through: the minute's budget is spent, CurseForge answered 429 and the hold has not ended,
/// or the request needs more calls than a whole minute allows, which no retry can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Spent { retry_after_ms: u64 },
    Hold { retry_after_ms: u64 },
    TooLarge { calls: u32, limit_per_minute: u32 },
}

/// Counts and caps the calls every consumer makes on the CurseForge key.
///
/// One ceiling per minute covers all pools. A call is admitted before it goes
/// upstream and recorded once CurseForge answered; a 429 holds every pool off
/// for as long as its `Retry-After` asks (30 s without one, 1 s to 10 min).
/// Minute counts per pool and daily counts for the last 14 UTC days are kept
/// in memory, which is correct for the single gateway process.
pub struct Budget {
    limit_per_minute: u32,
    pools: Vec<PoolKey>,
    clock: Arc<dyn Clock>,
    state: Mutex<Counts>,
}

type PoolKey = (Arc<str>, Arc<str>);

#[derive(Default)]
struct Counts {
    minute: u64,
    used: u32,
    withheld: u32,
    pools: HashMap<PoolKey, MinuteCounts>,
    held_until_ms: u64,
    days: BTreeMap<String, HashMap<PoolKey, DayCounts>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MinuteCounts {
    used: u32,
    withheld: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DayCounts {
    pub calls: u64,
    pub withheld: u64,
    pub forbidden: u64,
    pub throttled: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSnapshot {
    pub limit_per_minute: u32,
    pub used: u32,
    pub withheld: u32,
    pub resets_at: String,
    pub held_until: Option<String>,
    pub source: &'static str,
    pub pools: Vec<PoolSnapshot>,
    pub days: Vec<DaySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolSnapshot {
    pub consumer: String,
    pub pool: String,
    pub used: u32,
    pub withheld: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaySnapshot {
    pub date: String,
    pub pools: Vec<DayPoolSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DayPoolSnapshot {
    pub consumer: String,
    pub pool: String,
    #[serde(flatten)]
    pub counts: DayCounts,
}

impl Budget {
    pub fn new<C, P>(
        limit_per_minute: u32,
        pools: impl IntoIterator<Item = (C, P)>,
        clock: Arc<dyn Clock>,
    ) -> Self
    where
        C: Into<Arc<str>>,
        P: Into<Arc<str>>,
    {
        Self {
            limit_per_minute,
            pools: pools
                .into_iter()
                .map(|(consumer, pool)| (consumer.into(), pool.into()))
                .collect(),
            clock,
            state: Mutex::new(Counts::default()),
        }
    }

    /// Reserves `calls` upstream calls for one request, all or none. Zero calls (everything cached) always passes.
    /// Refused calls are counted as withheld, call by call.
    pub fn admit(&self, consumer: &str, pool: &str, calls: u32) -> Result<(), Refusal> {
        if calls == 0 {
            return Ok(());
        }
        let now = self.clock.now_ms();
        let key = self.key(consumer, pool);
        let mut state = self.lock(now);

        let refusal = if calls > self.limit_per_minute {
            Some(Refusal::TooLarge {
                calls,
                limit_per_minute: self.limit_per_minute,
            })
        } else if state.held_until_ms > now {
            Some(Refusal::Hold {
                retry_after_ms: state.held_until_ms - now,
            })
        } else if state.used.saturating_add(calls) > self.limit_per_minute {
            Some(Refusal::Spent {
                retry_after_ms: (state.minute + 1) * MINUTE_MS - now,
            })
        } else {
            None
        };

        if let Some(refusal) = refusal {
            state.withheld = state.withheld.saturating_add(calls);
            let minute = state.pools.entry(key.clone()).or_default();
            minute.withheld = minute.withheld.saturating_add(calls);
            state.day(now, key).withheld += u64::from(calls);
            return Err(refusal);
        }

        state.used += calls;
        let minute = state.pools.entry(key).or_default();
        minute.used = minute.used.saturating_add(calls);
        Ok(())
    }

    /// Records one call CurseForge answered with `status`, or `None` when it gave no answer. A 429 starts the hold.
    pub fn record(
        &self,
        consumer: &str,
        pool: &str,
        status: Option<u16>,
        retry_after: Option<&str>,
    ) {
        let now = self.clock.now_ms();
        let key = self.key(consumer, pool);
        let mut state = self.lock(now);
        let day = state.day(now, key);
        day.calls += 1;
        match status {
            Some(HTTP_FORBIDDEN) => day.forbidden += 1,
            Some(HTTP_TOO_MANY_REQUESTS) => {
                day.throttled += 1;
                let until = now + hold_ms(retry_after, now);
                state.held_until_ms = state.held_until_ms.max(until);
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        let now = self.clock.now_ms();
        let state = self.lock(now);
        let pools = self
            .pools
            .iter()
            .map(|key| {
                let counts = state.pools.get(key).copied().unwrap_or_default();
                PoolSnapshot {
                    consumer: key.0.to_string(),
                    pool: key.1.to_string(),
                    used: counts.used,
                    withheld: counts.withheld,
                }
            })
            .collect();
        let days = state
            .days
            .iter()
            .rev()
            .map(|(date, pools)| {
                let mut pools: Vec<DayPoolSnapshot> = pools
                    .iter()
                    .map(|(key, counts)| DayPoolSnapshot {
                        consumer: key.0.to_string(),
                        pool: key.1.to_string(),
                        counts: *counts,
                    })
                    .collect();
                pools.sort_by(|a, b| (&a.consumer, &a.pool).cmp(&(&b.consumer, &b.pool)));
                DaySnapshot {
                    date: date.clone(),
                    pools,
                }
            })
            .collect();

        BudgetSnapshot {
            limit_per_minute: self.limit_per_minute,
            used: state.used,
            withheld: state.withheld,
            resets_at: iso(((now / MINUTE_MS) + 1) * MINUTE_MS),
            held_until: (state.held_until_ms > now).then(|| iso(state.held_until_ms)),
            source: "memory",
            pools,
            days,
        }
    }

    fn key(&self, consumer: &str, pool: &str) -> PoolKey {
        self.pools
            .iter()
            .find(|(known_consumer, known_pool)| {
                known_consumer.as_ref() == consumer && known_pool.as_ref() == pool
            })
            .cloned()
            .unwrap_or_else(|| (consumer.into(), pool.into()))
    }

    fn lock(&self, now: u64) -> MutexGuard<'_, Counts> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let minute = now / MINUTE_MS;
        if state.minute != minute {
            state.minute = minute;
            state.used = 0;
            state.withheld = 0;
            state.pools.clear();
        }
        state
    }
}

impl Counts {
    fn day(&mut self, now: u64, key: PoolKey) -> &mut DayCounts {
        let date = utc_date(now);
        if !self.days.contains_key(&date) {
            self.days.insert(date.clone(), HashMap::new());
            while self.days.len() > DAYS_KEPT {
                self.days.pop_first();
            }
        }
        self.days.entry(date).or_default().entry(key).or_default()
    }
}

pub async fn readout(State(state): State<Arc<AppState>>) -> Json<BudgetSnapshot> {
    Json(state.budget.snapshot())
}

fn hold_ms(retry_after: Option<&str>, now: u64) -> u64 {
    let wait = retry_after
        .map(str::trim)
        .and_then(|header| {
            if !header.is_empty() && header.bytes().all(|byte| byte.is_ascii_digit()) {
                Some(
                    header
                        .parse::<u64>()
                        .map_or(u64::MAX, |seconds| seconds.saturating_mul(1000)),
                )
            } else {
                rfc2822::DateTimeParser::new()
                    .parse_timestamp(header)
                    .ok()
                    .map(|at| {
                        u64::try_from(at.as_millisecond())
                            .unwrap_or(0)
                            .saturating_sub(now)
                    })
            }
        })
        .unwrap_or(HOLD_WHEN_NOT_TOLD_MS);
    wait.clamp(SHORTEST_HOLD_MS, LONGEST_HOLD_MS)
}

fn timestamp(ms: u64) -> Timestamp {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

fn iso(ms: u64) -> String {
    timestamp(ms).to_string()
}

fn utc_date(ms: u64) -> String {
    timestamp(ms).strftime("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const NOV_3_2026_20H: u64 = 1_793_736_000_000;

    struct TestClock(AtomicU64);

    impl TestClock {
        fn at(ms: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(ms)))
        }

        fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn budget(limit: u32, clock: Arc<TestClock>) -> Budget {
        Budget::new(
            limit,
            [("app", "launcher"), ("app", "app"), ("sandbox", "jobs")],
            clock,
        )
    }

    #[test]
    fn admits_calls_up_to_the_limit_across_all_pools() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let budget = budget(5, clock);
        assert_eq!(budget.admit("app", "launcher", 3), Ok(()));
        assert_eq!(budget.admit("sandbox", "jobs", 2), Ok(()));
        assert_eq!(
            budget.admit("app", "app", 1),
            Err(Refusal::Spent {
                retry_after_ms: 60_000
            })
        );
    }

    #[test]
    fn a_batch_is_admitted_all_or_nothing() {
        let budget = budget(5, TestClock::at(NOV_3_2026_20H));
        assert_eq!(budget.admit("app", "app", 4), Ok(()));
        assert!(budget.admit("app", "app", 2).is_err());
        assert_eq!(budget.admit("app", "app", 1), Ok(()));
        assert_eq!(budget.snapshot().used, 5);
    }

    #[test]
    fn a_request_larger_than_a_whole_minute_is_refused_for_good() {
        let budget = budget(5, TestClock::at(NOV_3_2026_20H));
        assert_eq!(
            budget.admit("sandbox", "jobs", 6),
            Err(Refusal::TooLarge {
                calls: 6,
                limit_per_minute: 5
            })
        );
        assert_eq!(budget.admit("sandbox", "jobs", 5), Ok(()));
    }

    #[test]
    fn withheld_counts_calls_like_used_does() {
        let budget = budget(5, TestClock::at(NOV_3_2026_20H));
        budget.admit("app", "app", 4).unwrap();
        budget.admit("app", "app", 3).unwrap_err();

        let snapshot = budget.snapshot();
        assert_eq!((snapshot.used, snapshot.withheld), (4, 3));
        assert_eq!(snapshot.pools[1].withheld, 3);
        assert_eq!(snapshot.days[0].pools[0].counts.withheld, 3);
    }

    #[test]
    fn nothing_to_fetch_always_passes() {
        let budget = budget(1, TestClock::at(NOV_3_2026_20H));
        budget.admit("app", "app", 1).unwrap();
        assert_eq!(budget.admit("app", "app", 0), Ok(()));
        assert_eq!(budget.snapshot().withheld, 0);
    }

    #[test]
    fn the_count_starts_over_every_minute() {
        let clock = TestClock::at(NOV_3_2026_20H + 59_000);
        let budget = budget(1, clock.clone());
        budget.admit("app", "app", 1).unwrap();
        assert_eq!(
            budget.admit("app", "app", 1),
            Err(Refusal::Spent {
                retry_after_ms: 1_000
            })
        );
        clock.advance(1_000);
        assert_eq!(budget.admit("app", "app", 1), Ok(()));
    }

    #[test]
    fn a_429_holds_every_pool_off() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let budget = budget(300, clock.clone());
        budget.record("sandbox", "jobs", Some(429), Some("120"));
        assert_eq!(
            budget.admit("app", "launcher", 1),
            Err(Refusal::Hold {
                retry_after_ms: 120_000
            })
        );
        clock.advance(120_000);
        assert_eq!(budget.admit("app", "launcher", 1), Ok(()));
    }

    #[test]
    fn a_shorter_hold_does_not_end_a_longer_one() {
        let budget = budget(300, TestClock::at(NOV_3_2026_20H));
        budget.record("app", "app", Some(429), Some("300"));
        budget.record("app", "app", Some(429), Some("5"));
        assert_eq!(
            budget.admit("app", "app", 1),
            Err(Refusal::Hold {
                retry_after_ms: 300_000
            })
        );
    }

    #[test]
    fn reads_retry_after_as_seconds_or_a_date_and_clamps_it() {
        let now = NOV_3_2026_20H;
        assert_eq!(hold_ms(None, now), 30_000);
        assert_eq!(hold_ms(Some("not a date"), now), 30_000);
        assert_eq!(hold_ms(Some(" 42 "), now), 42_000);
        assert_eq!(hold_ms(Some("0"), now), 1_000);
        assert_eq!(hold_ms(Some("86400"), now), 600_000);
        assert_eq!(hold_ms(Some("99999999999999999999999"), now), 600_000);
        assert_eq!(hold_ms(Some("Tue, 03 Nov 2026 20:01:30 GMT"), now), 90_000);
        assert_eq!(hold_ms(Some("Tue, 03 Nov 2026 19:00:00 GMT"), now), 1_000);
    }

    #[test]
    fn counts_answers_per_pool_and_day() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let budget = budget(1, clock.clone());
        budget.record("app", "launcher", Some(200), None);
        budget.record("app", "launcher", Some(403), None);
        budget.record("app", "launcher", None, None);
        budget.admit("app", "launcher", 1).unwrap();
        budget.admit("app", "launcher", 1).unwrap_err();
        clock.advance(24 * 60 * MINUTE_MS);
        budget.record("sandbox", "jobs", Some(429), None);

        let snapshot = budget.snapshot();
        let dates: Vec<&str> = snapshot.days.iter().map(|day| day.date.as_str()).collect();
        assert_eq!(dates, ["2026-11-04", "2026-11-03"]);
        assert_eq!(
            snapshot.days[1].pools,
            [DayPoolSnapshot {
                consumer: "app".to_owned(),
                pool: "launcher".to_owned(),
                counts: DayCounts {
                    calls: 3,
                    withheld: 1,
                    forbidden: 1,
                    throttled: 0,
                },
            }]
        );
        assert_eq!(snapshot.days[0].pools[0].counts.throttled, 1);
    }

    #[test]
    fn keeps_fourteen_days() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let budget = budget(300, clock.clone());
        for _ in 0..20 {
            budget.record("app", "app", Some(200), None);
            clock.advance(24 * 60 * MINUTE_MS);
        }
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.days.len(), 14);
        assert_eq!(snapshot.days[0].date, "2026-11-22");
    }

    #[test]
    fn the_snapshot_lists_every_configured_pool_and_the_hold() {
        let clock = TestClock::at(NOV_3_2026_20H + 15_000);
        let budget = budget(300, clock);
        budget.admit("app", "launcher", 2).unwrap();
        budget.record("app", "launcher", Some(429), Some("30"));

        let snapshot = budget.snapshot();
        assert_eq!(snapshot.limit_per_minute, 300);
        assert_eq!(snapshot.used, 2);
        assert_eq!(snapshot.resets_at, "2026-11-03T20:01:00Z");
        assert_eq!(snapshot.held_until.as_deref(), Some("2026-11-03T20:00:45Z"));
        assert_eq!(snapshot.source, "memory");
        let pools: Vec<(&str, &str, u32)> = snapshot
            .pools
            .iter()
            .map(|pool| (pool.consumer.as_str(), pool.pool.as_str(), pool.used))
            .collect();
        assert_eq!(
            pools,
            [
                ("app", "launcher", 2),
                ("app", "app", 0),
                ("sandbox", "jobs", 0)
            ]
        );
    }
}
