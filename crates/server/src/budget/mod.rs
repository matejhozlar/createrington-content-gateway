pub mod readout;
mod redis;

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
};

use jiff::{Timestamp, fmt::rfc2822};
use serde::Serialize;
use tokio::sync::mpsc;
use utoipa::ToSchema;

use self::redis::Shared;
use crate::{
    auth::Pool,
    clock::{Clock, DAY_MS, MINUTE_MS},
    redis_store::RedisStore,
    upstream,
};

const HOLD_WHEN_NOT_TOLD_MS: u64 = 30_000;
const SHORTEST_HOLD_MS: u64 = 1_000;
const LONGEST_HOLD_MS: u64 = 10 * MINUTE_MS;
/// Days of daily counts kept unless `CONTENT_GATEWAY_BUDGET_HISTORY_DAYS` says otherwise.
pub const DEFAULT_HISTORY_DAYS: u32 = 90;
/// The most days of daily counts that can be kept.
pub const MAX_HISTORY_DAYS: u32 = 3660;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

/// Why calls were not let through: the minute's budget is spent, the upstream answered 429 and the hold has not ended,
/// or the request needs more calls than a whole minute allows, which no retry can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the budget of this minute is spent, the next call can go out in {retry_after_ms} ms")]
    Spent { retry_after_ms: u64 },
    #[error("the upstream answered 429, every call waits {retry_after_ms} ms")]
    Hold { retry_after_ms: u64 },
    #[error("{calls} calls are more than the {limit_per_minute} a minute allows")]
    TooLarge { calls: u32, limit_per_minute: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Admitted,
    TooLarge,
    Held,
    Spent,
}

impl Verdict {
    const ALL: [Self; 4] = [Self::Admitted, Self::TooLarge, Self::Held, Self::Spent];

    const fn code(self) -> i64 {
        self as i64
    }

    fn of_code(code: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|verdict| verdict.code() == code)
    }

    fn reached(calls: u32, limit: u32, held_until_ms: u64, used: u32, now: u64) -> Self {
        if calls > limit {
            Self::TooLarge
        } else if held_until_ms > now {
            Self::Held
        } else if used.saturating_add(calls) > limit {
            Self::Spent
        } else {
            Self::Admitted
        }
    }
}

/// Counts and caps the calls every consumer makes on one upstream, in memory and in Redis when configured.
pub struct Budget {
    limit_per_minute: u32,
    history_days: u32,
    upstream: upstream::Source,
    pools: Vec<Pool>,
    clock: Arc<dyn Clock>,
    memory: Arc<Mutex<Counts>>,
    redis: Option<Arc<Shared>>,
    flush: OnceLock<mpsc::Sender<()>>,
}

#[derive(Default)]
struct Counts {
    minute: u64,
    used: u32,
    withheld: u32,
    pools: HashMap<Pool, MinuteCounts>,
    held_until_ms: u64,
    told_spent: bool,
    days: BTreeMap<String, HashMap<Pool, DayCounts>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MinuteCounts {
    used: u32,
    withheld: u32,
}

/// What one pool did on one UTC day.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct DayCounts {
    /// Calls that reached the upstream.
    pub calls: u64,
    /// Requests answered without a call: from the cache, or by joining a call already in flight.
    pub hits: u64,
    /// Calls the budget refused.
    pub withheld: u64,
    /// Calls the upstream answered 403.
    pub forbidden: u64,
    /// Calls the upstream answered 429.
    pub throttled: u64,
}

impl Budget {
    /// The budget of `upstream`, with its Redis keys under the upstream's namespace.
    pub fn new(
        upstream: upstream::Source,
        limit_per_minute: u32,
        pools: impl IntoIterator<Item = Pool>,
        clock: Arc<dyn Clock>,
        redis: Option<Arc<RedisStore>>,
    ) -> Self {
        Self {
            limit_per_minute,
            history_days: DEFAULT_HISTORY_DAYS,
            upstream,
            pools: pools.into_iter().collect(),
            clock,
            memory: Arc::default(),
            redis: redis.map(|store| Arc::new(Shared::new(store, upstream.namespace()))),
            flush: OnceLock::new(),
        }
    }

    /// Keeps `days` UTC days of daily counts instead of the default, 1 up.
    pub fn keeping_days(mut self, days: u32) -> Self {
        self.history_days = days.max(1);
        self
    }

    /// Reserves `calls` upstream calls for one request, all or none.
    pub async fn admit(&self, pool: &Pool, calls: u32) -> Result<(), Refusal> {
        if calls == 0 {
            return Ok(());
        }
        let now = self.clock.now_ms();
        let decided = match &self.redis {
            Some(redis) => self.admit_in_redis(redis, pool, calls, now).await,
            None => None,
        };
        let (verdict, held_until_ms, first_spent) = {
            let mut state = self.lock(now);
            let verdict = match decided {
                Some((verdict, held_until_ms)) => {
                    state.keep_later_hold(held_until_ms);
                    verdict
                }
                None => Verdict::reached(
                    calls,
                    self.limit_per_minute,
                    state.held_until_ms,
                    state.used,
                    now,
                ),
            };
            state.count(pool, now, calls, verdict == Verdict::Admitted);
            if verdict != Verdict::Admitted {
                state.day(now, pool, self.history_days).withheld += u64::from(calls);
            }
            let first_spent =
                verdict == Verdict::Spent && !std::mem::replace(&mut state.told_spent, true);
            (verdict, state.held_until_ms, first_spent)
        };
        let refusal = match verdict {
            Verdict::Admitted => return Ok(()),
            Verdict::TooLarge => Refusal::TooLarge {
                calls,
                limit_per_minute: self.limit_per_minute,
            },
            Verdict::Held => Refusal::Hold {
                retry_after_ms: held_until_ms.saturating_sub(now),
            },
            Verdict::Spent => {
                let wait_ms = (now / MINUTE_MS + 1) * MINUTE_MS - now;
                if first_spent {
                    tracing::warn!(
                        upstream = self.upstream.name(),
                        %pool,
                        limit = self.limit_per_minute,
                        wait_ms,
                        "the budget of this minute is spent, calls wait for the next minute"
                    );
                }
                Refusal::Spent {
                    retry_after_ms: wait_ms,
                }
            }
        };
        self.write_days_soon();
        Err(refusal)
    }

    /// Records one call the upstream answered with `status`, or `None` when it gave no answer. A 429 starts a hold,
    /// which comes back as the refusal every call meets until it ends.
    pub async fn record(
        &self,
        pool: &Pool,
        status: Option<u16>,
        retry_after: Option<&str>,
    ) -> Option<Refusal> {
        let now = self.clock.now_ms();
        let started = {
            let mut state = self.lock(now);
            let day = state.day(now, pool, self.history_days);
            day.calls += 1;
            match status {
                Some(HTTP_FORBIDDEN) => {
                    day.forbidden += 1;
                    None
                }
                Some(HTTP_TOO_MANY_REQUESTS) => {
                    day.throttled += 1;
                    Some(state.keep_later_hold(now + hold_ms(retry_after, now)))
                }
                _ => None,
            }
        };
        self.write_days_soon();
        let mut held_until_ms = started?;
        if let Some(redis) = &self.redis {
            let mut invocation = redis.hold.key(redis.hold_key());
            invocation.arg(held_until_ms).arg(now);
            let stored = redis
                .store
                .run(|mut connection| async move {
                    invocation.invoke_async::<u64>(&mut connection).await
                })
                .await;
            if let Some(stored) = stored {
                held_until_ms = self.lock(now).keep_later_hold(stored);
            }
        }
        let wait_ms = held_until_ms.saturating_sub(now);
        tracing::warn!(
            upstream = self.upstream.name(),
            %pool,
            wait_ms,
            "the upstream answered 429, every call waits until the hold ends"
        );
        Some(Refusal::Hold {
            retry_after_ms: wait_ms,
        })
    }

    /// Counts one request answered without a call upstream: from the cache, or by joining a call already made.
    pub fn record_hit(&self, pool: &Pool) {
        let now = self.clock.now_ms();
        self.lock(now).day(now, pool, self.history_days).hits += 1;
        self.write_days_soon();
    }

    /// Writes the daily counts that are not in Redis yet, for shutdown.
    pub async fn write_days_at_exit(&self) {
        let Some(redis) = &self.redis else {
            return;
        };
        let lost = redis
            .write_days(&self.memory, &*self.clock, self.history_days, true)
            .await;
        if lost > 0 {
            tracing::warn!(
                upstream = self.upstream.name(),
                lost,
                "Redis did not take the last daily counts, they are lost"
            );
        }
    }

    fn write_days_soon(&self) {
        let Some(redis) = &self.redis else {
            return;
        };
        let flush = self.flush.get_or_init(|| {
            let (flush, mut asked) = mpsc::channel(1);
            let (redis, memory, clock) = (redis.clone(), self.memory.clone(), self.clock.clone());
            let history_days = self.history_days;
            tokio::spawn(async move {
                while asked.recv().await.is_some() {
                    redis
                        .write_days(&memory, &*clock, history_days, false)
                        .await;
                }
            });
            flush
        });
        let _ = flush.try_send(());
    }

    fn lock(&self, now: u64) -> MutexGuard<'_, Counts> {
        let mut state = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
        let minute = now / MINUTE_MS;
        if minute > state.minute {
            state.minute = minute;
            state.used = 0;
            state.withheld = 0;
            state.told_spent = false;
            state.pools.clear();
        }
        state
    }
}

impl Counts {
    fn day(&mut self, now: u64, pool: &Pool, history_days: u32) -> &mut DayCounts {
        let date = utc_date(now);
        if !self.days.contains_key(&date) {
            let oldest = utc_date(now.saturating_sub((u64::from(history_days) - 1) * DAY_MS));
            self.days.retain(|kept, _| *kept >= oldest);
        }
        self.days
            .entry(date)
            .or_default()
            .entry(pool.clone())
            .or_default()
    }

    fn count(&mut self, pool: &Pool, now: u64, calls: u32, admitted: bool) {
        if now / MINUTE_MS < self.minute {
            return;
        }
        let minute = self.pools.entry(pool.clone()).or_default();
        let (pool_count, count) = if admitted {
            (&mut minute.used, &mut self.used)
        } else {
            (&mut minute.withheld, &mut self.withheld)
        };
        *pool_count = pool_count.saturating_add(calls);
        *count = count.saturating_add(calls);
    }

    fn keep_later_hold(&mut self, until_ms: u64) -> u64 {
        self.held_until_ms = self.held_until_ms.max(until_ms);
        self.held_until_ms
    }
}

impl DayCounts {
    fn fields(&mut self) -> [(&'static str, &mut u64); 5] {
        [
            ("calls", &mut self.calls),
            ("hits", &mut self.hits),
            ("withheld", &mut self.withheld),
            ("forbidden", &mut self.forbidden),
            ("throttled", &mut self.throttled),
        ]
    }

    fn named(mut self) -> [(&'static str, u64); 5] {
        self.fields().map(|(name, count)| (name, *count))
    }

    fn add(&mut self, name: &str, count: u64) -> bool {
        let Some((_, field)) = self.fields().into_iter().find(|(known, _)| *known == name) else {
            return false;
        };
        *field = field.saturating_add(count);
        true
    }

    fn add_all(&mut self, other: Self) {
        for ((_, count), (_, added)) in self.fields().into_iter().zip(other.named()) {
            *count = count.saturating_add(added);
        }
    }

    fn subtract(&mut self, written: Self) {
        for ((_, count), (_, taken)) in self.fields().into_iter().zip(written.named()) {
            *count = count.saturating_sub(taken);
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
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

fn utc_date(ms: u64) -> String {
    timestamp(ms).strftime("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests;
