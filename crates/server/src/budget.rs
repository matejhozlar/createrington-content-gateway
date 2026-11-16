use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use jiff::{Timestamp, fmt::rfc2822};
use redis::{ParsingError, Script, Value, aio::ConnectionManager};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use utoipa::{IntoParams, ToSchema};

use crate::{
    app::AppState,
    auth::Pool,
    clock::{Clock, DAY_MS, MINUTE_MS},
    error::ApiError,
    redis_store::RedisStore,
    upstream,
};

const HOLD_WHEN_NOT_TOLD_MS: u64 = 30_000;
const SHORTEST_HOLD_MS: u64 = 1_000;
const LONGEST_HOLD_MS: u64 = 10 * MINUTE_MS;
const MINUTE_KEY_TTL_MS: u64 = 2 * MINUTE_MS;
const DAYS_TRIMMED: u64 = 31;
/// Days of daily counts kept unless `CONTENT_GATEWAY_BUDGET_HISTORY_DAYS` says otherwise.
pub const DEFAULT_HISTORY_DAYS: u32 = 90;
/// The most days of daily counts that can be kept.
pub const MAX_HISTORY_DAYS: u32 = 3660;
const DEFAULT_SNAPSHOT_DAYS: u32 = 14;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

const KEEP_LATER_HOLD: &str = r"
local function keep_later_hold(key, until_arg, now)
  local stored = tonumber(redis.call('GET', key) or '0') or 0
  local later = tonumber(until_arg)
  if later > stored and later > now then
    redis.call('SET', key, until_arg, 'PX', later - now)
    return later
  end
  return stored
end
";

fn admit_script() -> String {
    format!(
        r"{KEEP_LATER_HOLD}
local calls = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local held = keep_later_hold(KEYS[2], ARGV[4], now)
local used = tonumber(redis.call('HGET', KEYS[1], 'used') or '0') or 0
local counted = tonumber(ARGV[5])
if counted > used then
  redis.call('HSET', KEYS[1], 'used', counted)
  used = counted
end
for i = 8, #ARGV, 2 do
  local field = 'used:' .. ARGV[i]
  local pool_used = tonumber(redis.call('HGET', KEYS[1], field) or '0') or 0
  if tonumber(ARGV[i + 1]) > pool_used then
    redis.call('HSET', KEYS[1], field, ARGV[i + 1])
  end
end
local verdict = {code_admitted}
if calls > limit then
  verdict = {code_too_large}
elseif held > now then
  verdict = {code_held}
elseif used + calls > limit then
  verdict = {code_spent}
end
local count = 'used'
if verdict ~= {code_admitted} then count = 'withheld' end
redis.call('HINCRBY', KEYS[1], count, calls)
redis.call('HINCRBY', KEYS[1], count .. ':' .. ARGV[6], calls)
redis.call('PEXPIRE', KEYS[1], ARGV[7])
return {{verdict, held}}
",
        code_admitted = Verdict::Admitted.code(),
        code_too_large = Verdict::TooLarge.code(),
        code_held = Verdict::Held.code(),
        code_spent = Verdict::Spent.code(),
    )
}

fn hold_script() -> String {
    format!("{KEEP_LATER_HOLD}\nreturn keep_later_hold(KEYS[1], ARGV[1], tonumber(ARGV[2]))\n")
}

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

struct Shared {
    store: Arc<RedisStore>,
    namespace: &'static str,
    admit: Script,
    hold: Script,
    writing_days: tokio::sync::Mutex<()>,
    trimmed_on: Mutex<Option<String>>,
}

#[derive(Default)]
struct Counts {
    minute: u64,
    used: u32,
    withheld: u32,
    pools: HashMap<Pool, MinuteCounts>,
    held_until_ms: u64,
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

/// Where a readout's counts come from: `redis` when they are the stored ones, `memory` when no Redis is configured,
/// `unavailable` when Redis did not answer, so the minute is the calls this process let through or refused itself
/// and the days hold only what is not written to Redis yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(as = Source)]
pub enum CountSource {
    Redis,
    Memory,
    Unavailable,
}

impl CountSource {
    fn and(self, other: Self) -> Self {
        if other == Self::Unavailable {
            other
        } else {
            self
        }
    }
}

/// What `/v1/budget` answers: one budget per upstream, under one history and one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSnapshot {
    /// UTC days of daily counts the gateway keeps, the most `days` can cover.
    pub history_days: u32,
    pub source: CountSource,
    /// The budget of calls on the CurseForge key.
    pub curseforge: UpstreamBudget,
    /// The budget of calls to Modrinth, which rate-limits the gateway's address.
    pub modrinth: UpstreamBudget,
}

/// One upstream's budget: this minute over all pools and per pool, the 429 hold, and the last UTC days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamBudget {
    /// Calls the minute allows over all pools.
    pub limit_per_minute: u32,
    #[serde(skip)]
    #[schema(ignore)]
    history_days: u32,
    /// Calls admitted this minute.
    pub used: u32,
    /// Calls refused this minute.
    pub withheld: u32,
    /// When this minute's counts start over, ISO 8601.
    #[schema(format = DateTime)]
    pub resets_at: String,
    /// Until when the upstream's 429 holds every call off, ISO 8601, or null while no hold is on.
    #[schema(format = DateTime)]
    pub held_until: Option<String>,
    #[serde(skip)]
    #[schema(ignore)]
    source: CountSource,
    /// Every pool of every consumer, this minute.
    pub pools: Vec<PoolSnapshot>,
    /// The UTC days asked for, today first; a day without a single count is left out.
    pub days: Vec<DaySnapshot>,
}

/// The query of `/v1/budget`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryQuery {
    /// UTC days to answer, today included, 1 up; 14 when left out, and more than the days kept answers the days kept.
    #[param(minimum = 1)]
    pub days: Option<u32>,
}

/// One pool's calls this minute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PoolSnapshot {
    pub consumer: String,
    pub pool: String,
    /// Calls admitted this minute.
    pub used: u32,
    /// Calls refused this minute.
    pub withheld: u32,
}

/// One UTC day's counts for every pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DaySnapshot {
    /// The UTC day, `YYYY-MM-DD`.
    #[schema(format = Date)]
    pub date: String,
    pub pools: Vec<DayPoolSnapshot>,
}

/// One pool's counts on one UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DayPoolSnapshot {
    pub consumer: String,
    pub pool: String,
    #[serde(flatten)]
    pub counts: DayCounts,
}

struct Stored {
    minute: HashMap<String, u64>,
    held_until_ms: u64,
    days: Vec<HashMap<String, u64>>,
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
        let (verdict, held_until_ms) = {
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
            (verdict, state.held_until_ms)
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
            Verdict::Spent => Refusal::Spent {
                retry_after_ms: (now / MINUTE_MS + 1) * MINUTE_MS - now,
            },
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
        Some(Refusal::Hold {
            retry_after_ms: held_until_ms.saturating_sub(now),
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
                "Redis did not take the last daily counts of the {} budget, {lost} of them are lost",
                self.upstream.name()
            );
        }
    }

    #[cfg(test)]
    pub async fn snapshot(&self) -> UpstreamBudget {
        self.history(DEFAULT_SNAPSHOT_DAYS).await
    }

    /// The budget with the last `days` UTC days, today included, at most the days kept.
    pub async fn history(&self, days: u32) -> UpstreamBudget {
        let now = self.clock.now_ms();
        let dates: Vec<String> = (0..u64::from(days.clamp(1, self.history_days)))
            .map(|days_ago| utc_date(now.saturating_sub(days_ago * DAY_MS)))
            .collect();
        let _writing = match &self.redis {
            Some(redis) => Some(redis.writing_days.lock().await),
            None => None,
        };
        let (source, stored) = match &self.redis {
            None => (CountSource::Memory, None),
            Some(redis) => match self.read_redis(redis, now, &dates).await {
                Some(stored) => (CountSource::Redis, Some(stored)),
                None => (CountSource::Unavailable, None),
            },
        };

        let mut state = self.lock(now);
        if let Some(stored) = &stored {
            state.keep_later_hold(stored.held_until_ms);
        }
        let (used, withheld, pools) = match &stored {
            Some(stored) => {
                let count = |field: &str| {
                    u32::try_from(stored.minute.get(field).copied().unwrap_or(0))
                        .unwrap_or(u32::MAX)
                };
                let pools = self
                    .pools
                    .iter()
                    .map(|pool| {
                        let field = pool_field(pool);
                        pool_snapshot(
                            pool,
                            MinuteCounts {
                                used: count(&format!("used:{field}")),
                                withheld: count(&format!("withheld:{field}")),
                            },
                        )
                    })
                    .collect();
                (count("used"), count("withheld"), pools)
            }
            None => {
                let pools = self
                    .pools
                    .iter()
                    .map(|pool| {
                        pool_snapshot(pool, state.pools.get(pool).copied().unwrap_or_default())
                    })
                    .collect();
                (state.used, state.withheld, pools)
            }
        };

        let days = dates
            .iter()
            .enumerate()
            .filter_map(|(index, date)| {
                let mut counts: HashMap<Pool, DayCounts> = HashMap::new();
                if let Some(fields) = stored.as_ref().and_then(|stored| stored.days.get(index)) {
                    for (field, count) in fields {
                        let known = parse_day_field(field).is_some_and(|(pool, name)| {
                            counts.entry(pool).or_default().add(name, *count)
                        });
                        if !known {
                            tracing::warn!(
                                "Redis holds the daily count {field} of the {} budget, which is not one the gateway \
                                 keeps, so the readout leaves it out",
                                self.upstream.name()
                            );
                        }
                    }
                }
                for (pool, unwritten) in state.days.get(date).into_iter().flatten() {
                    counts.entry(pool.clone()).or_default().add_all(*unwritten);
                }
                let mut pools: Vec<DayPoolSnapshot> = counts
                    .into_iter()
                    .filter(|(_, counts)| !counts.is_empty())
                    .map(|(pool, counts)| DayPoolSnapshot {
                        consumer: pool.consumer.to_string(),
                        pool: pool.name.to_string(),
                        counts,
                    })
                    .collect();
                pools.sort_by(|a, b| (&a.consumer, &a.pool).cmp(&(&b.consumer, &b.pool)));
                (!pools.is_empty()).then(|| DaySnapshot {
                    date: date.clone(),
                    pools,
                })
            })
            .collect();

        UpstreamBudget {
            limit_per_minute: self.limit_per_minute,
            history_days: self.history_days,
            used,
            withheld,
            resets_at: iso((now / MINUTE_MS + 1) * MINUTE_MS),
            held_until: (state.held_until_ms > now).then(|| iso(state.held_until_ms)),
            source,
            pools,
            days,
        }
    }

    async fn admit_in_redis(
        &self,
        redis: &Shared,
        pool: &Pool,
        calls: u32,
        now: u64,
    ) -> Option<(Verdict, u64)> {
        let (counted, held_until_ms, pools_counted) = {
            let state = self.lock(now);
            let pools_counted: Vec<(String, u32)> = state
                .pools
                .iter()
                .filter(|(_, counts)| counts.used > 0)
                .map(|(pool, counts)| (pool_field(pool), counts.used))
                .collect();
            (state.used, state.held_until_ms, pools_counted)
        };
        let mut invocation = redis.admit.key(redis.minute_key(now));
        invocation
            .key(redis.hold_key())
            .arg(calls)
            .arg(self.limit_per_minute)
            .arg(now)
            .arg(held_until_ms)
            .arg(counted)
            .arg(pool_field(pool))
            .arg(MINUTE_KEY_TTL_MS);
        for (field, used) in pools_counted {
            invocation.arg(field).arg(used);
        }
        let (code, held_until_ms) = redis
            .store
            .run(|mut connection| async move {
                invocation.invoke_async::<(i64, u64)>(&mut connection).await
            })
            .await?;
        let Some(verdict) = Verdict::of_code(code) else {
            tracing::warn!(
                "the budget script of {} answered the unknown verdict {code}, memory decides instead",
                self.upstream.name()
            );
            return None;
        };
        Some((verdict, held_until_ms))
    }

    async fn read_redis(&self, redis: &Shared, now: u64, dates: &[String]) -> Option<Stored> {
        let mut pipeline = redis::pipe();
        pipeline
            .hgetall(redis.minute_key(now))
            .get(redis.hold_key());
        for date in dates {
            pipeline.hgetall(redis.day_key(date));
        }
        let replies = redis
            .store
            .run(|mut connection| async move {
                pipeline.query_async::<Vec<Value>>(&mut connection).await
            })
            .await?;
        match Stored::read(replies) {
            Ok(stored) => Some(stored),
            Err(error) => {
                tracing::warn!(
                    "Redis answered the readout of the {} budget with replies it cannot read, the readout uses \
                     memory: {error}",
                    self.upstream.name()
                );
                None
            }
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
            state.pools.clear();
        }
        state
    }
}

impl Stored {
    fn read(replies: Vec<Value>) -> Result<Self, ParsingError> {
        let mut replies = replies.into_iter();
        let mut next = || {
            replies
                .next()
                .ok_or_else(|| ParsingError::from("a reply is missing"))
        };
        let minute = redis::from_redis_value(next()?)?;
        let held_until_ms = redis::from_redis_value::<Option<u64>>(next()?)?.unwrap_or(0);
        let days = replies
            .map(redis::from_redis_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            minute,
            held_until_ms,
            days,
        })
    }
}

impl Shared {
    fn new(store: Arc<RedisStore>, namespace: &'static str) -> Self {
        Self {
            store,
            namespace,
            admit: Script::new(&admit_script()),
            hold: Script::new(&hold_script()),
            writing_days: tokio::sync::Mutex::new(()),
            trimmed_on: Mutex::new(None),
        }
    }

    fn key(&self, tail: &str) -> String {
        if self.namespace.is_empty() {
            self.store.key(&format!("budget:{tail}"))
        } else {
            self.store.key(&format!("budget:{}:{tail}", self.namespace))
        }
    }

    fn minute_key(&self, now: u64) -> String {
        self.key(&format!("minute:{}", now / MINUTE_MS))
    }

    fn hold_key(&self) -> String {
        self.key("held-until")
    }

    fn day_key(&self, date: &str) -> String {
        self.key(&format!("day:{date}"))
    }

    async fn write_days(
        &self,
        memory: &Mutex<Counts>,
        clock: &dyn Clock,
        history_days: u32,
        even_if_paused: bool,
    ) -> u64 {
        let _writing = self.writing_days.lock().await;
        let unwritten: Vec<(String, Pool, DayCounts)> = {
            let state = memory.lock().unwrap_or_else(PoisonError::into_inner);
            state
                .days
                .iter()
                .flat_map(|(date, pools)| {
                    pools
                        .iter()
                        .map(|(pool, counts)| (date.clone(), pool.clone(), *counts))
                })
                .collect()
        };
        if unwritten.is_empty() {
            return 0;
        }

        let mut pipeline = redis::pipe();
        pipeline.atomic();
        for (date, pool, counts) in &unwritten {
            let day_key = self.day_key(date);
            for (name, count) in counts.named() {
                if count > 0 {
                    pipeline
                        .hincr(&day_key, day_field(pool, name), count)
                        .ignore();
                }
            }
        }
        let now = clock.now_ms();
        let today = utc_date(now);
        let trimmed_on = self
            .trimmed_on
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let trim = trimmed_on.as_ref() != Some(&today);
        if trim {
            let kept = u64::from(history_days);
            let reach = if trimmed_on.is_none() {
                u64::from(MAX_HISTORY_DAYS)
            } else {
                DAYS_TRIMMED
            };
            let gone: Vec<String> = (kept..kept + reach)
                .map(|days_ago| self.day_key(&utc_date(now.saturating_sub(days_ago * DAY_MS))))
                .collect();
            pipeline.del(gone).ignore();
        }
        let write = |mut connection: ConnectionManager| async move {
            pipeline.query_async::<()>(&mut connection).await
        };
        let written = if even_if_paused {
            self.store.run_even_if_paused(write).await
        } else {
            self.store.run(write).await
        };
        if written.is_none() {
            return unwritten
                .iter()
                .flat_map(|(_, _, counts)| counts.named())
                .map(|(_, count)| count)
                .sum();
        }

        if trim {
            *self
                .trimmed_on
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(today);
        }
        let mut state = memory.lock().unwrap_or_else(PoisonError::into_inner);
        for (date, pool, counts) in unwritten {
            let Some(pools) = state.days.get_mut(&date) else {
                continue;
            };
            if let Some(left) = pools.get_mut(&pool) {
                left.subtract(counts);
                if left.is_empty() {
                    pools.remove(&pool);
                }
            }
            if pools.is_empty() {
                state.days.remove(&date);
            }
        }
        0
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

/// Each upstream's calls this minute per pool, its 429 hold, and its daily counts of the last UTC days.
#[utoipa::path(
    get,
    path = "/v1/budget",
    tag = "gateway",
    security(("bearer" = [])),
    params(HistoryQuery),
    responses(
        (status = 200, description = "The budget as it stands", body = BudgetSnapshot),
        (status = 400, description = "`days` is not a whole number from 1 up", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
    )
)]
pub async fn readout(
    State(state): State<Arc<AppState>>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<BudgetSnapshot>, ApiError> {
    let days = match query {
        Ok(Query(HistoryQuery { days: Some(0) })) | Err(_) => return Err(ApiError::invalid_days()),
        Ok(Query(HistoryQuery { days })) => days.unwrap_or(DEFAULT_SNAPSHOT_DAYS),
    };
    let curseforge = state.curseforge_budget.history(days).await;
    let modrinth = state.modrinth_budget.history(days).await;
    Ok(Json(BudgetSnapshot {
        history_days: curseforge.history_days,
        source: curseforge.source.and(modrinth.source),
        curseforge,
        modrinth,
    }))
}

fn pool_snapshot(pool: &Pool, counts: MinuteCounts) -> PoolSnapshot {
    PoolSnapshot {
        consumer: pool.consumer.to_string(),
        pool: pool.name.to_string(),
        used: counts.used,
        withheld: counts.withheld,
    }
}

fn pool_field(pool: &Pool) -> String {
    format!("{}/{}", pool.consumer, pool.name)
}

fn day_field(pool: &Pool, name: &str) -> String {
    format!("{}/{name}", pool_field(pool))
}

fn parse_day_field(field: &str) -> Option<(Pool, &str)> {
    let (pool, name) = field.rsplit_once('/')?;
    let (consumer, pool) = pool.split_once('/')?;
    Some((
        Pool {
            consumer: consumer.into(),
            name: pool.into(),
        },
        name,
    ))
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
    use super::*;
    use crate::{
        clock::TestClock,
        test_support::{NOV_3_2026_20H, TestRedis},
    };

    fn pool(consumer: &str, name: &str) -> Pool {
        Pool {
            consumer: consumer.into(),
            name: name.into(),
        }
    }

    fn pools() -> Vec<Pool> {
        [("app", "launcher"), ("app", "app"), ("sandbox", "jobs")]
            .into_iter()
            .map(|(consumer, name)| pool(consumer, name))
            .collect()
    }

    struct Backends {
        redis: Option<TestRedis>,
        budgets: Vec<(CountSource, Budget)>,
    }

    impl Backends {
        fn new(limit: u32, clock: &Arc<TestClock>) -> Self {
            Self::keeping_days(limit, clock, DEFAULT_HISTORY_DAYS)
        }

        fn keeping_days(limit: u32, clock: &Arc<TestClock>, days: u32) -> Self {
            let mut budgets = vec![(
                CountSource::Memory,
                Budget::new(
                    upstream::Source::CurseForge,
                    limit,
                    pools(),
                    clock.clone(),
                    None,
                )
                .keeping_days(days),
            )];
            let redis = TestRedis::from_env();
            if let Some(redis) = &redis {
                budgets.push((
                    CountSource::Redis,
                    Budget::new(
                        upstream::Source::CurseForge,
                        limit,
                        pools(),
                        clock.clone(),
                        Some(redis.open()),
                    )
                    .keeping_days(days),
                ));
            }
            Self { redis, budgets }
        }

        async fn forget(self) {
            if let Some(redis) = &self.redis {
                for (_, budget) in &self.budgets {
                    budget.write_days_at_exit().await;
                }
                redis.forget().await;
            }
        }
    }

    #[tokio::test]
    async fn admits_calls_up_to_the_limit_across_all_pools() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(5, &clock);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit(&pool("app", "launcher"), 3).await,
                Ok(()),
                "{source:?}"
            );
            assert_eq!(
                budget.admit(&pool("sandbox", "jobs"), 2).await,
                Ok(()),
                "{source:?}"
            );
            assert_eq!(
                budget.admit(&pool("app", "app"), 1).await,
                Err(Refusal::Spent {
                    retry_after_ms: 60_000
                }),
                "{source:?}"
            );
            assert_eq!(budget.snapshot().await.source, *source);
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn a_batch_is_admitted_all_or_nothing() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(5, &clock);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit(&pool("app", "app"), 4).await,
                Ok(()),
                "{source:?}"
            );
            assert!(
                budget.admit(&pool("app", "app"), 2).await.is_err(),
                "{source:?}"
            );
            assert_eq!(
                budget.admit(&pool("app", "app"), 1).await,
                Ok(()),
                "{source:?}"
            );
            assert_eq!(budget.snapshot().await.used, 5, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn a_request_larger_than_a_whole_minute_is_refused_for_good() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(5, &clock);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit(&pool("sandbox", "jobs"), 6).await,
                Err(Refusal::TooLarge {
                    calls: 6,
                    limit_per_minute: 5
                }),
                "{source:?}"
            );
            assert_eq!(
                budget.admit(&pool("sandbox", "jobs"), 5).await,
                Ok(()),
                "{source:?}"
            );
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn withheld_counts_calls_like_used_does() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(5, &clock);
        for (source, budget) in &backends.budgets {
            budget.admit(&pool("app", "app"), 4).await.unwrap();
            budget.admit(&pool("app", "app"), 3).await.unwrap_err();

            let snapshot = budget.snapshot().await;
            assert_eq!((snapshot.used, snapshot.withheld), (4, 3), "{source:?}");
            assert_eq!(snapshot.pools[1].withheld, 3, "{source:?}");
            assert_eq!(snapshot.days[0].pools[0].counts.withheld, 3, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn nothing_to_fetch_always_passes() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(1, &clock);
        for (source, budget) in &backends.budgets {
            budget.admit(&pool("app", "app"), 1).await.unwrap();
            assert_eq!(
                budget.admit(&pool("app", "app"), 0).await,
                Ok(()),
                "{source:?}"
            );
            assert_eq!(budget.snapshot().await.withheld, 0, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn the_count_starts_over_every_minute() {
        let clock = TestClock::at(NOV_3_2026_20H + 59_000);
        let backends = Backends::new(1, &clock);
        for (source, budget) in &backends.budgets {
            budget.admit(&pool("app", "app"), 1).await.unwrap();
            assert_eq!(
                budget.admit(&pool("app", "app"), 1).await,
                Err(Refusal::Spent {
                    retry_after_ms: 1_000
                }),
                "{source:?}"
            );
        }
        clock.advance(1_000);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit(&pool("app", "app"), 1).await,
                Ok(()),
                "{source:?}"
            );
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn a_late_count_from_the_last_minute_leaves_this_minute_alone() {
        let clock = TestClock::at(NOV_3_2026_20H + MINUTE_MS);
        let budget = Budget::new(
            upstream::Source::CurseForge,
            5,
            pools(),
            clock.clone(),
            None,
        );
        budget.admit(&pool("app", "app"), 3).await.unwrap();
        budget
            .lock(NOV_3_2026_20H)
            .count(&pool("app", "app"), NOV_3_2026_20H, 2, true);

        let snapshot = budget.snapshot().await;
        assert_eq!((snapshot.used, snapshot.pools[1].used), (3, 3));
        assert_eq!(
            budget.admit(&pool("app", "app"), 3).await,
            Err(Refusal::Spent {
                retry_after_ms: 60_000
            })
        );
    }

    #[tokio::test]
    async fn a_429_holds_every_pool_off() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(300, &clock);
        for (source, budget) in &backends.budgets {
            budget
                .record(&pool("sandbox", "jobs"), Some(429), Some("120"))
                .await;
            assert_eq!(
                budget.admit(&pool("app", "launcher"), 1).await,
                Err(Refusal::Hold {
                    retry_after_ms: 120_000
                }),
                "{source:?}"
            );
        }
        clock.advance(120_000);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit(&pool("app", "launcher"), 1).await,
                Ok(()),
                "{source:?}"
            );
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn a_shorter_hold_does_not_end_a_longer_one() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(300, &clock);
        for (source, budget) in &backends.budgets {
            budget
                .record(&pool("app", "app"), Some(429), Some("300"))
                .await;
            assert_eq!(
                budget
                    .record(&pool("app", "app"), Some(429), Some("5"))
                    .await,
                Some(Refusal::Hold {
                    retry_after_ms: 300_000
                }),
                "{source:?}"
            );
            assert_eq!(
                budget.admit(&pool("app", "app"), 1).await,
                Err(Refusal::Hold {
                    retry_after_ms: 300_000
                }),
                "{source:?}"
            );
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn only_a_429_starts_a_hold() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(300, &clock);
        for (source, budget) in &backends.budgets {
            for status in [Some(200), Some(403), Some(404), None] {
                assert_eq!(
                    budget.record(&pool("app", "app"), status, Some("60")).await,
                    None,
                    "{source:?} {status:?}"
                );
            }
            assert_eq!(
                budget
                    .record(&pool("app", "app"), Some(429), Some("60"))
                    .await,
                Some(Refusal::Hold {
                    retry_after_ms: 60_000
                }),
                "{source:?}"
            );
        }
        backends.forget().await;
    }

    #[test]
    fn the_readout_is_unavailable_when_either_budget_could_not_read_redis() {
        use CountSource::{Memory, Redis, Unavailable};
        assert_eq!(Redis.and(Redis), Redis);
        assert_eq!(Memory.and(Memory), Memory);
        assert_eq!(Redis.and(Unavailable), Unavailable);
        assert_eq!(Unavailable.and(Redis), Unavailable);
    }

    #[test]
    fn the_script_decides_with_the_same_verdict_codes() {
        let script = admit_script();
        for verdict in Verdict::ALL {
            assert_eq!(Verdict::of_code(verdict.code()), Some(verdict));
            assert!(
                script.contains(&format!("verdict = {}", verdict.code())),
                "{verdict:?}"
            );
        }
        assert_eq!(Verdict::of_code(4), None);
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

    #[tokio::test]
    async fn counts_answers_per_pool_and_day() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(1, &clock);
        for (_, budget) in &backends.budgets {
            budget
                .record(&pool("app", "launcher"), Some(200), None)
                .await;
            budget
                .record(&pool("app", "launcher"), Some(403), None)
                .await;
            budget.record(&pool("app", "launcher"), None, None).await;
            budget.admit(&pool("app", "launcher"), 1).await.unwrap();
            budget.admit(&pool("app", "launcher"), 1).await.unwrap_err();
        }
        clock.advance(DAY_MS);
        for (source, budget) in &backends.budgets {
            budget
                .record(&pool("sandbox", "jobs"), Some(429), None)
                .await;

            let snapshot = budget.snapshot().await;
            let dates: Vec<&str> = snapshot.days.iter().map(|day| day.date.as_str()).collect();
            assert_eq!(dates, ["2026-11-04", "2026-11-03"], "{source:?}");
            assert_eq!(
                snapshot.days[1].pools,
                [DayPoolSnapshot {
                    consumer: "app".to_owned(),
                    pool: "launcher".to_owned(),
                    counts: DayCounts {
                        calls: 3,
                        hits: 0,
                        withheld: 1,
                        forbidden: 1,
                        throttled: 0,
                    },
                }],
                "{source:?}"
            );
            assert_eq!(snapshot.days[0].pools[0].counts.throttled, 1, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn counts_cache_hits_per_pool_and_day() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(300, &clock);
        for (source, budget) in &backends.budgets {
            budget.record_hit(&pool("app", "launcher"));
            budget.record_hit(&pool("app", "launcher"));
            let snapshot = budget.snapshot().await;
            assert_eq!(snapshot.used, 0, "{source:?}");
            assert_eq!(snapshot.days[0].pools[0].counts.hits, 2, "{source:?}");
            assert_eq!(snapshot.days[0].pools[0].counts.calls, 0, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn answers_fourteen_days_unless_asked_for_more_up_to_the_days_kept() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::keeping_days(300, &clock, 20);
        for _ in 0..25 {
            clock.advance(DAY_MS);
            for (_, budget) in &backends.budgets {
                budget.record(&pool("app", "app"), Some(200), None).await;
            }
        }
        for (source, budget) in &backends.budgets {
            let snapshot = budget.snapshot().await;
            assert_eq!(snapshot.history_days, 20, "{source:?}");
            assert_eq!(snapshot.days.len(), 14, "{source:?}");
            assert_eq!(snapshot.days[0].date, "2026-11-28", "{source:?}");
            assert_eq!(snapshot.days[13].date, "2026-11-15", "{source:?}");

            let history = budget.history(90).await;
            assert_eq!(history.days.len(), 20, "{source:?}");
            assert_eq!(history.days[19].date, "2026-11-09", "{source:?}");

            assert_eq!(budget.history(0).await.days.len(), 1, "{source:?}");
            assert_eq!(budget.history(3).await.days.len(), 3, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn keeps_the_configured_days_in_memory() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let budget = Budget::new(
            upstream::Source::CurseForge,
            300,
            pools(),
            clock.clone(),
            None,
        )
        .keeping_days(3);
        for _ in 0..5 {
            budget.record(&pool("app", "app"), Some(200), None).await;
            clock.advance(DAY_MS);
        }
        let history = budget.history(90).await;
        assert_eq!(history.history_days, 3);
        let dates: Vec<&str> = history.days.iter().map(|day| day.date.as_str()).collect();
        assert_eq!(dates, ["2026-11-07", "2026-11-06"]);
    }

    #[tokio::test]
    async fn the_snapshot_lists_every_configured_pool_and_the_hold() {
        let clock = TestClock::at(NOV_3_2026_20H + 15_000);
        let backends = Backends::new(300, &clock);
        for (source, budget) in &backends.budgets {
            budget.admit(&pool("app", "launcher"), 2).await.unwrap();
            budget
                .record(&pool("app", "launcher"), Some(429), Some("30"))
                .await;

            let snapshot = budget.snapshot().await;
            assert_eq!(snapshot.limit_per_minute, 300);
            assert_eq!(snapshot.used, 2, "{source:?}");
            assert_eq!(snapshot.resets_at, "2026-11-03T20:01:00Z");
            assert_eq!(
                snapshot.held_until.as_deref(),
                Some("2026-11-03T20:00:45Z"),
                "{source:?}"
            );
            assert_eq!(snapshot.source, *source);
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
                ],
                "{source:?}"
            );
        }
        backends.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn redis_keeps_the_minute_the_hold_and_the_days_across_a_restart() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                upstream::Source::CurseForge,
                5,
                pools(),
                clock.clone(),
                Some(redis.open()),
            )
        };

        let before = open();
        before.admit(&pool("app", "launcher"), 3).await.unwrap();
        before
            .record(&pool("app", "launcher"), Some(200), None)
            .await;
        before
            .record(&pool("app", "launcher"), Some(429), Some("60"))
            .await;
        before.write_days_at_exit().await;
        drop(before);

        let after = open();
        let snapshot = after.snapshot().await;
        assert_eq!(snapshot.source, CountSource::Redis);
        assert_eq!(snapshot.used, 3);
        assert_eq!(snapshot.pools[0].used, 3);
        assert_eq!(snapshot.held_until.as_deref(), Some("2026-11-03T20:01:00Z"));
        assert_eq!(snapshot.days[0].pools[0].counts.calls, 2);
        assert_eq!(snapshot.days[0].pools[0].counts.throttled, 1);
        assert_eq!(
            after.admit(&pool("sandbox", "jobs"), 1).await,
            Err(Refusal::Hold {
                retry_after_ms: 60_000
            })
        );

        clock.advance(60_000);
        assert_eq!(after.admit(&pool("sandbox", "jobs"), 5).await, Ok(()));
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn a_hold_redis_missed_is_stored_by_the_next_admit() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                upstream::Source::CurseForge,
                5,
                pools(),
                clock.clone(),
                Some(redis.open()),
            )
        };

        let first = open();
        first.lock(NOV_3_2026_20H).held_until_ms = NOV_3_2026_20H + 60_000;
        assert!(first.admit(&pool("app", "app"), 1).await.is_err());

        let second = open();
        assert_eq!(
            second.admit(&pool("sandbox", "jobs"), 1).await,
            Err(Refusal::Hold {
                retry_after_ms: 60_000
            })
        );
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn the_exit_write_tries_redis_even_while_it_is_skipped() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = redis.open();
        let failed: Option<()> = store
            .run(|_| async { Err(std::io::Error::other("refused").into()) })
            .await;
        assert_eq!(failed, None);
        let before = Budget::new(
            upstream::Source::CurseForge,
            5,
            pools(),
            clock.clone(),
            Some(store),
        );
        before.record(&pool("app", "app"), Some(200), None).await;
        before.write_days_at_exit().await;
        drop(before);

        let after = Budget::new(
            upstream::Source::CurseForge,
            5,
            pools(),
            clock,
            Some(redis.open()),
        );
        let snapshot = after.snapshot().await;
        assert_eq!(snapshot.source, CountSource::Redis);
        assert_eq!(snapshot.days[0].pools[0].counts.calls, 1);
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn daily_counts_never_expire_and_days_past_the_kept_are_deleted() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = redis.open();
        let budget = Budget::new(
            upstream::Source::CurseForge,
            300,
            pools(),
            clock.clone(),
            Some(store.clone()),
        )
        .keeping_days(14);
        for _ in 0..20 {
            budget.record(&pool("app", "app"), Some(200), None).await;
            budget.write_days_at_exit().await;
            clock.advance(DAY_MS);
        }

        let pattern = store.key("budget:day:*");
        let mut days = store
            .run(|mut connection| async move {
                redis::cmd("KEYS")
                    .arg(pattern)
                    .query_async::<Vec<String>>(&mut connection)
                    .await
            })
            .await
            .unwrap();
        days.sort();
        assert_eq!(days.len(), 14);
        assert!(days[0].ends_with("2026-11-09"), "{days:?}");
        let newest = days[13].clone();
        let ttl = store
            .run(|mut connection| async move {
                redis::cmd("PTTL")
                    .arg(newest)
                    .query_async::<i64>(&mut connection)
                    .await
            })
            .await;
        assert_eq!(ttl, Some(-1));
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn days_past_a_lowered_history_are_deleted_on_the_first_write() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = redis.open();
        let long = Budget::new(
            upstream::Source::CurseForge,
            300,
            pools(),
            clock.clone(),
            Some(store.clone()),
        )
        .keeping_days(90);
        for _ in 0..80 {
            long.record(&pool("app", "app"), Some(200), None).await;
            long.write_days_at_exit().await;
            clock.advance(DAY_MS);
        }
        assert_eq!(day_keys(&store).await.len(), 80);

        let short = Budget::new(
            upstream::Source::CurseForge,
            300,
            pools(),
            clock.clone(),
            Some(store.clone()),
        )
        .keeping_days(14);
        short.record(&pool("app", "app"), Some(200), None).await;
        short.write_days_at_exit().await;
        let days = day_keys(&store).await;
        assert_eq!(days.len(), 14, "{days:?}");
        assert!(days[0].ends_with("2027-01-09"), "{days:?}");
        assert_eq!(short.history(90).await.days.len(), 14);
        redis.forget().await;
    }

    async fn day_keys(store: &RedisStore) -> Vec<String> {
        keys_matching(store, "budget:day:*").await
    }

    async fn keys_matching(store: &RedisStore, pattern: &str) -> Vec<String> {
        let pattern = store.key(pattern);
        let mut days = store
            .run(|mut connection| async move {
                redis::cmd("KEYS")
                    .arg(pattern)
                    .query_async::<Vec<String>>(&mut connection)
                    .await
            })
            .await
            .unwrap();
        days.sort();
        days
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn a_modrinth_budget_keeps_its_own_keys() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = redis.open();
        let curseforge = Budget::new(
            upstream::Source::CurseForge,
            5,
            pools(),
            clock.clone(),
            Some(store.clone()),
        );
        let modrinth = Budget::new(
            upstream::Source::Modrinth,
            5,
            pools(),
            clock.clone(),
            Some(store.clone()),
        );
        curseforge.admit(&pool("app", "app"), 5).await.unwrap();
        curseforge
            .record(&pool("app", "app"), Some(200), None)
            .await;
        assert_eq!(modrinth.admit(&pool("app", "app"), 5).await, Ok(()));
        modrinth
            .record(&pool("app", "app"), Some(429), Some("30"))
            .await;
        curseforge.write_days_at_exit().await;
        modrinth.write_days_at_exit().await;
        assert_eq!(day_keys(&store).await.len(), 1);
        let named = keys_matching(&store, "budget:modrinth:*").await;
        assert_eq!(named.len(), 3, "{named:?}");
        assert_eq!(curseforge.snapshot().await.held_until, None);
        assert_eq!(
            curseforge.snapshot().await.days[0].pools[0]
                .counts
                .throttled,
            0
        );
        let modrinth = modrinth.snapshot().await;
        assert!(modrinth.held_until.is_some());
        assert_eq!(modrinth.days[0].pools[0].counts.throttled, 1);
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn two_budgets_on_one_redis_share_the_minute() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                upstream::Source::CurseForge,
                5,
                pools(),
                clock.clone(),
                Some(redis.open()),
            )
        };
        let (first, second) = (open(), open());
        first.admit(&pool("app", "app"), 3).await.unwrap();
        assert_eq!(
            second.admit(&pool("sandbox", "jobs"), 3).await,
            Err(Refusal::Spent {
                retry_after_ms: 60_000
            })
        );
        assert_eq!(second.admit(&pool("sandbox", "jobs"), 2).await, Ok(()));
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn a_redis_flap_does_not_let_more_than_the_limit_through() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = redis.open_on(clock.clone());
        let budget = Budget::new(
            upstream::Source::CurseForge,
            5,
            pools(),
            clock.clone(),
            Some(store.clone()),
        );
        budget.admit(&pool("app", "app"), 3).await.unwrap();

        let failed: Option<()> = store
            .run(|_| async { Err(std::io::Error::other("refused").into()) })
            .await;
        assert_eq!(failed, None);
        assert_eq!(
            budget.admit(&pool("app", "app"), 3).await,
            Err(Refusal::Spent {
                retry_after_ms: 60_000
            })
        );
        budget.admit(&pool("app", "launcher"), 2).await.unwrap();

        clock.advance(5_000);
        assert_eq!(
            budget.admit(&pool("app", "app"), 1).await,
            Err(Refusal::Spent {
                retry_after_ms: 55_000
            })
        );
        let snapshot = budget.snapshot().await;
        assert_eq!(snapshot.source, CountSource::Redis);
        let pools: Vec<u32> = snapshot.pools.iter().map(|pool| pool.used).collect();
        assert_eq!((snapshot.used, pools), (5, vec![2, 3, 0]));
        redis.forget().await;
    }

    #[tokio::test]
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn counts_reach_redis_without_waiting_for_the_exit() {
        let redis = TestRedis::required();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                upstream::Source::CurseForge,
                5,
                pools(),
                clock.clone(),
                Some(redis.open()),
            )
        };
        let (counting, reading) = (open(), open());
        for _ in 0..50 {
            counting.record_hit(&pool("app", "app"));
        }
        let mut hits = 0;
        for _ in 0..5_000 {
            hits = reading
                .snapshot()
                .await
                .days
                .first()
                .map_or(0, |day| day.pools[0].counts.hits);
            if hits == 50 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(hits, 50);
        redis.forget().await;
    }

    #[tokio::test]
    async fn falls_back_to_memory_when_redis_does_not_answer() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = Arc::new(
            RedisStore::open("redis://127.0.0.1:1", "content-gateway-test", clock.clone()).unwrap(),
        );
        let budget = Budget::new(
            upstream::Source::CurseForge,
            2,
            pools(),
            clock.clone(),
            Some(store),
        );

        assert_eq!(budget.admit(&pool("app", "app"), 2).await, Ok(()));
        assert!(budget.admit(&pool("app", "app"), 1).await.is_err());
        budget
            .record(&pool("app", "app"), Some(429), Some("10"))
            .await;
        clock.advance(60_000);
        assert_eq!(budget.admit(&pool("app", "app"), 1).await, Ok(()));

        let snapshot = budget.snapshot().await;
        assert_eq!(snapshot.source, CountSource::Unavailable);
        assert_eq!(snapshot.used, 1);
        assert_eq!(
            snapshot.days[0].pools[0].counts,
            DayCounts {
                calls: 1,
                hits: 0,
                withheld: 1,
                forbidden: 0,
                throttled: 1,
            }
        );
    }
}
