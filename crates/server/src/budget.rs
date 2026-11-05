use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use jiff::{Timestamp, fmt::rfc2822};
use redis::{Script, Value, aio::ConnectionManager};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::{app::AppState, error::ApiError, redis_store::RedisStore};

const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 24 * 60 * MINUTE_MS;
const HOLD_WHEN_NOT_TOLD_MS: u64 = 30_000;
const SHORTEST_HOLD_MS: u64 = 1_000;
const LONGEST_HOLD_MS: u64 = 10 * MINUTE_MS;
const MINUTE_KEY_TTL_MS: u64 = 2 * MINUTE_MS;
const DAYS_TRIMMED: u64 = 31;
/// Days of daily counts kept unless `CONTENT_GATEWAY_BUDGET_HISTORY_DAYS` says otherwise.
pub const DEFAULT_HISTORY_DAYS: u32 = 90;
/// The most days of daily counts that can be kept.
pub const MAX_HISTORY_DAYS: u32 = 3660;
/// Days `/v1/budget` answers when not asked for a number.
pub const DEFAULT_SNAPSHOT_DAYS: u32 = 14;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;
const ADMITTED: i64 = 0;
const TOO_LARGE: i64 = 1;
const HELD: i64 = 2;
const SPENT: i64 = 3;

const ADMIT_SCRIPT: &str = r"
local calls = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local known = tonumber(ARGV[4])
local stored = tonumber(redis.call('GET', KEYS[2]) or '0') or 0
if known > stored and known > now then
  redis.call('SET', KEYS[2], ARGV[4], 'PX', known - now)
end
local held = math.max(known, stored)
local used = tonumber(redis.call('HGET', KEYS[1], 'used') or '0') or 0
local verdict = 0
if calls > limit then
  verdict = 1
elseif held > now then
  verdict = 2
elseif used + calls > limit then
  verdict = 3
end
local count = 'used'
if verdict ~= 0 then count = 'withheld' end
redis.call('HINCRBY', KEYS[1], count, calls)
redis.call('HINCRBY', KEYS[1], count .. ':' .. ARGV[5], calls)
redis.call('PEXPIRE', KEYS[1], ARGV[6])
return {verdict, held}
";

const HOLD_SCRIPT: &str = r"
local held = tonumber(redis.call('GET', KEYS[1]) or '0') or 0
if tonumber(ARGV[1]) > held then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
end
return 0
";

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

/// Counts and caps the calls every consumer makes on one upstream, in memory and in Redis when configured.
pub struct Budget {
    limit_per_minute: u32,
    history_days: u32,
    name: &'static str,
    pools: Vec<PoolKey>,
    clock: Arc<dyn Clock>,
    memory: Arc<Mutex<Counts>>,
    redis: Option<Arc<Shared>>,
}

struct Shared {
    store: Arc<RedisStore>,
    namespace: &'static str,
    admit: Script,
    hold: Script,
    writing_days: tokio::sync::Mutex<()>,
    trimmed_on: Mutex<Option<String>>,
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

/// What one pool did on one UTC day.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct DayCounts {
    /// Calls that reached CurseForge.
    pub calls: u64,
    /// Requests answered without a call: from the cache, or by joining a call already in flight.
    pub hits: u64,
    /// Calls the budget refused.
    pub withheld: u64,
    /// Calls CurseForge answered 403.
    pub forbidden: u64,
    /// Calls CurseForge answered 429.
    pub throttled: u64,
}

/// Where a readout's counts come from: `redis` when they are the stored ones, `memory` when no Redis is configured,
/// `unavailable` when Redis did not answer, so the minute is this process's own count and the days hold only what
/// is not written to Redis yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Redis,
    Memory,
    Unavailable,
}

/// What `/v1/budget` answers: one budget per upstream, under one history and one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSnapshot {
    /// UTC days of daily counts the gateway keeps, the most `days` can cover.
    pub history_days: u32,
    pub source: Source,
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
    pub history_days: u32,
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
    pub source: Source,
    /// Every pool of every consumer, this minute.
    pub pools: Vec<PoolSnapshot>,
    /// The UTC days asked for, today first; a day without a single count is left out.
    pub days: Vec<DaySnapshot>,
}

/// The query of `/v1/budget`.
#[derive(Debug, Default, Deserialize, IntoParams)]
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
    pub fn new<C, P>(
        limit_per_minute: u32,
        pools: impl IntoIterator<Item = (C, P)>,
        clock: Arc<dyn Clock>,
        redis: Option<Arc<RedisStore>>,
    ) -> Self
    where
        C: Into<Arc<str>>,
        P: Into<Arc<str>>,
    {
        Self {
            limit_per_minute,
            history_days: DEFAULT_HISTORY_DAYS,
            name: "CurseForge",
            pools: pools
                .into_iter()
                .map(|(consumer, pool)| (consumer.into(), pool.into()))
                .collect(),
            clock,
            memory: Arc::default(),
            redis: redis.map(|store| Arc::new(Shared::new(store, ""))),
        }
    }

    /// Keeps `days` UTC days of daily counts instead of the default, 1 up.
    pub fn keeping_days(mut self, days: u32) -> Self {
        self.history_days = days.max(1);
        self
    }

    /// Makes this the budget of another upstream, with its Redis keys under `budget:{namespace}:`.
    pub fn named(mut self, namespace: &'static str, name: &'static str) -> Self {
        self.name = name;
        self.redis = self
            .redis
            .take()
            .map(|redis| Arc::new(Shared::new(redis.store.clone(), namespace)));
        self
    }

    /// Reserves `calls` upstream calls for one request, all or none.
    pub async fn admit(&self, consumer: &str, pool: &str, calls: u32) -> Result<(), Refusal> {
        if calls == 0 {
            return Ok(());
        }
        let now = self.clock.now_ms();
        let key = self.key(consumer, pool);
        let shared = match &self.redis {
            Some(redis) => self.admit_in_redis(redis, &key, calls, now).await,
            None => None,
        };
        let verdict = match shared {
            Some(verdict) => verdict,
            None => self.admit_in_memory(&key, calls, now),
        };
        if verdict.is_err() {
            self.lock(now).day(now, key, self.history_days).withheld += u64::from(calls);
            self.write_days_soon();
        }
        verdict
    }

    /// Records one call CurseForge answered with `status`, or `None` when it gave no answer. A 429 starts the hold.
    pub async fn record(
        &self,
        consumer: &str,
        pool: &str,
        status: Option<u16>,
        retry_after: Option<&str>,
    ) {
        let now = self.clock.now_ms();
        let key = self.key(consumer, pool);
        let hold = {
            let mut state = self.lock(now);
            let day = state.day(now, key, self.history_days);
            day.calls += 1;
            match status {
                Some(HTTP_FORBIDDEN) => {
                    day.forbidden += 1;
                    None
                }
                Some(HTTP_TOO_MANY_REQUESTS) => {
                    day.throttled += 1;
                    let wait = hold_ms(retry_after, now);
                    state.held_until_ms = state.held_until_ms.max(now + wait);
                    Some((now + wait, wait))
                }
                _ => None,
            }
        };
        if let (Some(redis), Some((until, wait))) = (&self.redis, hold) {
            let mut invocation = redis.hold.key(redis.store.key(&redis.key("held-until")));
            invocation.arg(until).arg(wait);
            redis
                .store
                .run(|mut connection| async move {
                    invocation.invoke_async::<()>(&mut connection).await
                })
                .await;
        }
        self.write_days_soon();
    }

    /// Counts one request answered without a call to CurseForge: from the cache, or by joining a call already made.
    pub fn record_hit(&self, consumer: &str, pool: &str) {
        let now = self.clock.now_ms();
        let key = self.key(consumer, pool);
        self.lock(now).day(now, key, self.history_days).hits += 1;
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
                self.name
            );
        }
    }

    /// The budget with the last 14 UTC days, at most the days kept.
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
            None => (Source::Memory, None),
            Some(redis) => match self.read_redis(redis, now, &dates).await {
                Some(stored) => (Source::Redis, Some(stored)),
                None => (Source::Unavailable, None),
            },
        };

        let mut state = self.lock(now);
        if let Some(stored) = &stored {
            state.held_until_ms = state.held_until_ms.max(stored.held_until_ms);
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
                    .map(|key| {
                        let field = pool_field(key);
                        pool_snapshot(
                            key,
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
                    .map(|key| {
                        pool_snapshot(key, state.pools.get(key).copied().unwrap_or_default())
                    })
                    .collect();
                (state.used, state.withheld, pools)
            }
        };

        let days = dates
            .iter()
            .enumerate()
            .filter_map(|(index, date)| {
                let mut counts: HashMap<PoolKey, DayCounts> = HashMap::new();
                if let Some(fields) = stored.as_ref().and_then(|stored| stored.days.get(index)) {
                    for (field, count) in fields {
                        let Some((consumer, pool, name)) = parse_day_field(field) else {
                            continue;
                        };
                        counts
                            .entry(self.key(consumer, pool))
                            .or_default()
                            .add(name, *count);
                    }
                }
                for (key, unwritten) in state.days.get(date).into_iter().flatten() {
                    for (name, count) in unwritten.named() {
                        counts.entry(key.clone()).or_default().add(name, count);
                    }
                }
                let mut pools: Vec<DayPoolSnapshot> = counts
                    .into_iter()
                    .filter(|(_, counts)| !counts.is_empty())
                    .map(|(key, counts)| DayPoolSnapshot {
                        consumer: key.0.to_string(),
                        pool: key.1.to_string(),
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
            resets_at: iso(((now / MINUTE_MS) + 1) * MINUTE_MS),
            held_until: (state.held_until_ms > now).then(|| iso(state.held_until_ms)),
            source,
            pools,
            days,
        }
    }

    async fn admit_in_redis(
        &self,
        redis: &Shared,
        key: &PoolKey,
        calls: u32,
        now: u64,
    ) -> Option<Result<(), Refusal>> {
        let minute = now / MINUTE_MS;
        let held_in_memory = self.lock(now).held_until_ms;
        let mut invocation = redis
            .admit
            .key(redis.store.key(&redis.key(&format!("minute:{minute}"))));
        invocation
            .key(redis.store.key(&redis.key("held-until")))
            .arg(calls)
            .arg(self.limit_per_minute)
            .arg(now)
            .arg(held_in_memory)
            .arg(pool_field(key))
            .arg(MINUTE_KEY_TTL_MS);
        let (verdict, held_until) = redis
            .store
            .run(|mut connection| async move {
                invocation.invoke_async::<(i64, i64)>(&mut connection).await
            })
            .await?;
        let held_until = u64::try_from(held_until).unwrap_or(0);
        match verdict {
            ADMITTED => Some(Ok(())),
            TOO_LARGE => Some(Err(Refusal::TooLarge {
                calls,
                limit_per_minute: self.limit_per_minute,
            })),
            HELD => {
                let mut state = self.lock(now);
                state.held_until_ms = state.held_until_ms.max(held_until);
                Some(Err(Refusal::Hold {
                    retry_after_ms: held_until.saturating_sub(now),
                }))
            }
            SPENT => Some(Err(Refusal::Spent {
                retry_after_ms: (minute + 1) * MINUTE_MS - now,
            })),
            _ => None,
        }
    }

    fn admit_in_memory(&self, key: &PoolKey, calls: u32, now: u64) -> Result<(), Refusal> {
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

        let minute = state.pools.entry(key.clone()).or_default();
        if let Some(refusal) = refusal {
            minute.withheld = minute.withheld.saturating_add(calls);
            state.withheld = state.withheld.saturating_add(calls);
            return Err(refusal);
        }
        minute.used = minute.used.saturating_add(calls);
        state.used += calls;
        Ok(())
    }

    async fn read_redis(&self, redis: &Shared, now: u64, dates: &[String]) -> Option<Stored> {
        let mut pipeline = redis::pipe();
        pipeline
            .hgetall(
                redis
                    .store
                    .key(&redis.key(&format!("minute:{}", now / MINUTE_MS))),
            )
            .get(redis.store.key(&redis.key("held-until")));
        for date in dates {
            pipeline.hgetall(redis.store.key(&redis.day_key(date)));
        }
        let replies = redis
            .store
            .run(|mut connection| async move {
                pipeline.query_async::<Vec<Value>>(&mut connection).await
            })
            .await?;

        let mut replies = replies.into_iter();
        let minute = redis::from_redis_value(replies.next()?).ok()?;
        let held_until_ms = redis::from_redis_value::<Option<u64>>(replies.next()?)
            .ok()?
            .unwrap_or(0);
        let days = replies
            .map(|reply| redis::from_redis_value(reply).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(Stored {
            minute,
            held_until_ms,
            days,
        })
    }

    fn write_days_soon(&self) {
        if let Some(redis) = &self.redis {
            let (redis, memory, clock) = (redis.clone(), self.memory.clone(), self.clock.clone());
            let history_days = self.history_days;
            tokio::spawn(async move {
                redis
                    .write_days(&memory, &*clock, history_days, false)
                    .await;
            });
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
        let mut state = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
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

impl Shared {
    async fn write_days(
        &self,
        memory: &Mutex<Counts>,
        clock: &dyn Clock,
        history_days: u32,
        even_if_paused: bool,
    ) -> u64 {
        let _writing = self.writing_days.lock().await;
        let unwritten: Vec<(String, PoolKey, DayCounts)> = {
            let state = memory.lock().unwrap_or_else(PoisonError::into_inner);
            state
                .days
                .iter()
                .flat_map(|(date, pools)| {
                    pools
                        .iter()
                        .map(|(key, counts)| (date.clone(), key.clone(), *counts))
                })
                .collect()
        };
        if unwritten.is_empty() {
            return 0;
        }

        let mut pipeline = redis::pipe();
        pipeline.atomic();
        for (date, key, counts) in &unwritten {
            let day_key = self.store.key(&self.day_key(date));
            for (name, count) in counts.named() {
                if count > 0 {
                    pipeline
                        .hincr(&day_key, day_field(key, name), count)
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
                .map(|days_ago| {
                    self.store
                        .key(&self.day_key(&utc_date(now.saturating_sub(days_ago * DAY_MS))))
                })
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
        for (date, key, counts) in unwritten {
            let Some(pools) = state.days.get_mut(&date) else {
                continue;
            };
            if let Some(left) = pools.get_mut(&key) {
                left.subtract(&counts);
                if left.is_empty() {
                    pools.remove(&key);
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
    fn day(&mut self, now: u64, key: PoolKey, history_days: u32) -> &mut DayCounts {
        let date = utc_date(now);
        if !self.days.contains_key(&date) {
            let oldest = utc_date(now.saturating_sub((u64::from(history_days) - 1) * DAY_MS));
            self.days.retain(|kept, _| *kept >= oldest);
        }
        self.days.entry(date).or_default().entry(key).or_default()
    }
}

impl DayCounts {
    fn named(&self) -> [(&'static str, u64); 5] {
        [
            ("calls", self.calls),
            ("hits", self.hits),
            ("withheld", self.withheld),
            ("forbidden", self.forbidden),
            ("throttled", self.throttled),
        ]
    }

    fn add(&mut self, name: &str, count: u64) {
        let field = match name {
            "calls" => &mut self.calls,
            "hits" => &mut self.hits,
            "withheld" => &mut self.withheld,
            "forbidden" => &mut self.forbidden,
            "throttled" => &mut self.throttled,
            _ => return,
        };
        *field = field.saturating_add(count);
    }

    fn subtract(&mut self, written: &Self) {
        self.calls = self.calls.saturating_sub(written.calls);
        self.hits = self.hits.saturating_sub(written.hits);
        self.withheld = self.withheld.saturating_sub(written.withheld);
        self.forbidden = self.forbidden.saturating_sub(written.forbidden);
        self.throttled = self.throttled.saturating_sub(written.throttled);
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
    let curseforge = state.budget.history(days).await;
    let modrinth = state.modrinth_budget.history(days).await;
    Ok(Json(BudgetSnapshot {
        history_days: curseforge.history_days,
        source: curseforge.source,
        curseforge,
        modrinth,
    }))
}

fn pool_snapshot(key: &PoolKey, counts: MinuteCounts) -> PoolSnapshot {
    PoolSnapshot {
        consumer: key.0.to_string(),
        pool: key.1.to_string(),
        used: counts.used,
        withheld: counts.withheld,
    }
}

impl Shared {
    fn new(store: Arc<RedisStore>, namespace: &'static str) -> Self {
        Self {
            store,
            namespace,
            admit: Script::new(ADMIT_SCRIPT),
            hold: Script::new(HOLD_SCRIPT),
            writing_days: tokio::sync::Mutex::new(()),
            trimmed_on: Mutex::new(None),
        }
    }

    fn key(&self, tail: &str) -> String {
        if self.namespace.is_empty() {
            format!("budget:{tail}")
        } else {
            format!("budget:{}:{tail}", self.namespace)
        }
    }

    fn day_key(&self, date: &str) -> String {
        self.key(&format!("day:{date}"))
    }
}

fn pool_field(key: &PoolKey) -> String {
    format!("{}/{}", key.0, key.1)
}

fn day_field(key: &PoolKey, name: &str) -> String {
    format!("{}/{}/{name}", key.0, key.1)
}

fn parse_day_field(field: &str) -> Option<(&str, &str, &str)> {
    let (pool_key, name) = field.rsplit_once('/')?;
    let (consumer, pool) = pool_key.split_once('/')?;
    Some((consumer, pool, name))
}

/// How long a 429 with this `Retry-After` holds every call off: 30 s without one, 1 s to 10 min.
pub fn hold_ms(retry_after: Option<&str>, now: u64) -> u64 {
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
pub(crate) struct TestClock(std::sync::atomic::AtomicU64);

#[cfg(test)]
impl TestClock {
    pub(crate) fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(std::sync::atomic::AtomicU64::new(ms)))
    }

    pub(crate) fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const NOV_3_2026_20H: u64 = 1_793_736_000_000;
    const TEST_REDIS_ENV: &str = "CONTENT_GATEWAY_TEST_REDIS_URL";
    const POOLS: [(&str, &str); 3] = [("app", "launcher"), ("app", "app"), ("sandbox", "jobs")];

    fn test_redis_url() -> Option<String> {
        std::env::var(TEST_REDIS_ENV)
            .ok()
            .filter(|url| !url.trim().is_empty())
    }

    fn unique_prefix() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            "content-gateway-test:{}:{}:{}",
            std::process::id(),
            SystemClock.now_ms(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        )
    }

    async fn forget(url: &str, prefix: &str) {
        let mut connection = redis::Client::open(url)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(format!("{prefix}:*"))
            .query_async(&mut connection)
            .await
            .unwrap();
        if !keys.is_empty() {
            redis::cmd("DEL")
                .arg(keys)
                .query_async::<()>(&mut connection)
                .await
                .unwrap();
        }
    }

    struct Backends {
        redis: Option<(String, String)>,
        budgets: Vec<(Source, Budget)>,
    }

    impl Backends {
        fn new(limit: u32, clock: &Arc<TestClock>) -> Self {
            Self::keeping_days(limit, clock, DEFAULT_HISTORY_DAYS)
        }

        fn keeping_days(limit: u32, clock: &Arc<TestClock>, days: u32) -> Self {
            let mut budgets = vec![(
                Source::Memory,
                Budget::new(limit, POOLS, clock.clone(), None).keeping_days(days),
            )];
            let redis = test_redis_url().map(|url| (url, unique_prefix()));
            if let Some((url, prefix)) = &redis {
                let store = RedisStore::open(url, prefix.clone()).unwrap();
                budgets.push((
                    Source::Redis,
                    Budget::new(limit, POOLS, clock.clone(), Some(Arc::new(store)))
                        .keeping_days(days),
                ));
            }
            Self { redis, budgets }
        }

        async fn forget(self) {
            if let Some((url, prefix)) = &self.redis {
                for (_, budget) in &self.budgets {
                    budget.write_days_at_exit().await;
                }
                forget(url, prefix).await;
            }
        }
    }

    #[tokio::test]
    async fn admits_calls_up_to_the_limit_across_all_pools() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(5, &clock);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit("app", "launcher", 3).await,
                Ok(()),
                "{source:?}"
            );
            assert_eq!(
                budget.admit("sandbox", "jobs", 2).await,
                Ok(()),
                "{source:?}"
            );
            assert_eq!(
                budget.admit("app", "app", 1).await,
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
            assert_eq!(budget.admit("app", "app", 4).await, Ok(()), "{source:?}");
            assert!(budget.admit("app", "app", 2).await.is_err(), "{source:?}");
            assert_eq!(budget.admit("app", "app", 1).await, Ok(()), "{source:?}");
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
                budget.admit("sandbox", "jobs", 6).await,
                Err(Refusal::TooLarge {
                    calls: 6,
                    limit_per_minute: 5
                }),
                "{source:?}"
            );
            assert_eq!(
                budget.admit("sandbox", "jobs", 5).await,
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
            budget.admit("app", "app", 4).await.unwrap();
            budget.admit("app", "app", 3).await.unwrap_err();

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
            budget.admit("app", "app", 1).await.unwrap();
            assert_eq!(budget.admit("app", "app", 0).await, Ok(()), "{source:?}");
            assert_eq!(budget.snapshot().await.withheld, 0, "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn the_count_starts_over_every_minute() {
        let clock = TestClock::at(NOV_3_2026_20H + 59_000);
        let backends = Backends::new(1, &clock);
        for (source, budget) in &backends.budgets {
            budget.admit("app", "app", 1).await.unwrap();
            assert_eq!(
                budget.admit("app", "app", 1).await,
                Err(Refusal::Spent {
                    retry_after_ms: 1_000
                }),
                "{source:?}"
            );
        }
        clock.advance(1_000);
        for (source, budget) in &backends.budgets {
            assert_eq!(budget.admit("app", "app", 1).await, Ok(()), "{source:?}");
        }
        backends.forget().await;
    }

    #[tokio::test]
    async fn a_429_holds_every_pool_off() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let backends = Backends::new(300, &clock);
        for (source, budget) in &backends.budgets {
            budget
                .record("sandbox", "jobs", Some(429), Some("120"))
                .await;
            assert_eq!(
                budget.admit("app", "launcher", 1).await,
                Err(Refusal::Hold {
                    retry_after_ms: 120_000
                }),
                "{source:?}"
            );
        }
        clock.advance(120_000);
        for (source, budget) in &backends.budgets {
            assert_eq!(
                budget.admit("app", "launcher", 1).await,
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
            budget.record("app", "app", Some(429), Some("300")).await;
            budget.record("app", "app", Some(429), Some("5")).await;
            assert_eq!(
                budget.admit("app", "app", 1).await,
                Err(Refusal::Hold {
                    retry_after_ms: 300_000
                }),
                "{source:?}"
            );
        }
        backends.forget().await;
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
            budget.record("app", "launcher", Some(200), None).await;
            budget.record("app", "launcher", Some(403), None).await;
            budget.record("app", "launcher", None, None).await;
            budget.admit("app", "launcher", 1).await.unwrap();
            budget.admit("app", "launcher", 1).await.unwrap_err();
        }
        clock.advance(DAY_MS);
        for (source, budget) in &backends.budgets {
            budget.record("sandbox", "jobs", Some(429), None).await;

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
            budget.record_hit("app", "launcher");
            budget.record_hit("app", "launcher");
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
                budget.record("app", "app", Some(200), None).await;
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
        let budget = Budget::new(300, POOLS, clock.clone(), None).keeping_days(3);
        for _ in 0..5 {
            budget.record("app", "app", Some(200), None).await;
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
            budget.admit("app", "launcher", 2).await.unwrap();
            budget
                .record("app", "launcher", Some(429), Some("30"))
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
    async fn redis_keeps_the_minute_the_hold_and_the_days_across_a_restart() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                5,
                POOLS,
                clock.clone(),
                Some(Arc::new(RedisStore::open(&url, prefix.clone()).unwrap())),
            )
        };

        let before = open();
        before.admit("app", "launcher", 3).await.unwrap();
        before.record("app", "launcher", Some(200), None).await;
        before
            .record("app", "launcher", Some(429), Some("60"))
            .await;
        before.write_days_at_exit().await;
        drop(before);

        let after = open();
        let snapshot = after.snapshot().await;
        assert_eq!(snapshot.source, Source::Redis);
        assert_eq!(snapshot.used, 3);
        assert_eq!(snapshot.pools[0].used, 3);
        assert_eq!(snapshot.held_until.as_deref(), Some("2026-11-03T20:01:00Z"));
        assert_eq!(snapshot.days[0].pools[0].counts.calls, 2);
        assert_eq!(snapshot.days[0].pools[0].counts.throttled, 1);
        assert_eq!(
            after.admit("sandbox", "jobs", 1).await,
            Err(Refusal::Hold {
                retry_after_ms: 60_000
            })
        );

        clock.advance(60_000);
        assert_eq!(after.admit("sandbox", "jobs", 5).await, Ok(()));
        forget(&url, &prefix).await;
    }

    #[tokio::test]
    async fn a_hold_redis_missed_is_stored_by_the_next_admit() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                5,
                POOLS,
                clock.clone(),
                Some(Arc::new(RedisStore::open(&url, prefix.clone()).unwrap())),
            )
        };

        let first = open();
        first.lock(NOV_3_2026_20H).held_until_ms = NOV_3_2026_20H + 60_000;
        assert!(first.admit("app", "app", 1).await.is_err());

        let second = open();
        assert_eq!(
            second.admit("sandbox", "jobs", 1).await,
            Err(Refusal::Hold {
                retry_after_ms: 60_000
            })
        );
        forget(&url, &prefix).await;
    }

    #[tokio::test]
    async fn the_exit_write_tries_redis_even_while_it_is_skipped() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = RedisStore::open(&url, prefix.clone()).unwrap();
        store.skip_for_a_while();
        let before = Budget::new(5, POOLS, clock.clone(), Some(Arc::new(store)));
        before.record("app", "app", Some(200), None).await;
        before.write_days_at_exit().await;
        drop(before);

        let after = Budget::new(
            5,
            POOLS,
            clock,
            Some(Arc::new(RedisStore::open(&url, prefix.clone()).unwrap())),
        );
        let snapshot = after.snapshot().await;
        assert_eq!(snapshot.source, Source::Redis);
        assert_eq!(snapshot.days[0].pools[0].counts.calls, 1);
        forget(&url, &prefix).await;
    }

    #[tokio::test]
    async fn daily_counts_never_expire_and_days_past_the_kept_are_deleted() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = Arc::new(RedisStore::open(&url, prefix.clone()).unwrap());
        let budget = Budget::new(300, POOLS, clock.clone(), Some(store.clone())).keeping_days(14);
        for _ in 0..20 {
            budget.record("app", "app", Some(200), None).await;
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
        forget(&url, &prefix).await;
    }

    #[tokio::test]
    async fn days_past_a_lowered_history_are_deleted_on_the_first_write() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = Arc::new(RedisStore::open(&url, prefix.clone()).unwrap());
        let long = Budget::new(300, POOLS, clock.clone(), Some(store.clone())).keeping_days(90);
        for _ in 0..80 {
            long.record("app", "app", Some(200), None).await;
            long.write_days_at_exit().await;
            clock.advance(DAY_MS);
        }
        assert_eq!(day_keys(&store).await.len(), 80);

        let short = Budget::new(300, POOLS, clock.clone(), Some(store.clone())).keeping_days(14);
        short.record("app", "app", Some(200), None).await;
        short.write_days_at_exit().await;
        let days = day_keys(&store).await;
        assert_eq!(days.len(), 14, "{days:?}");
        assert!(days[0].ends_with("2027-01-09"), "{days:?}");
        assert_eq!(short.history(90).await.days.len(), 14);
        forget(&url, &prefix).await;
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
    async fn a_named_budget_keeps_its_own_keys() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = Arc::new(RedisStore::open(&url, prefix.clone()).unwrap());
        let curseforge = Budget::new(5, POOLS, clock.clone(), Some(store.clone()));
        let shared = Budget::new(5, POOLS, clock.clone(), Some(store.clone()));
        let modrinth = shared.named("modrinth", "Modrinth");
        curseforge.admit("app", "app", 5).await.unwrap();
        curseforge.record("app", "app", Some(200), None).await;
        assert_eq!(modrinth.admit("app", "app", 5).await, Ok(()));
        modrinth.record("app", "app", Some(429), Some("30")).await;
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
        forget(&url, &prefix).await;
    }

    #[tokio::test]
    async fn two_budgets_on_one_redis_share_the_minute() {
        let Some(url) = test_redis_url() else {
            eprintln!("{TEST_REDIS_ENV} is not set, skipped");
            return;
        };
        let prefix = unique_prefix();
        let clock = TestClock::at(NOV_3_2026_20H);
        let open = || {
            Budget::new(
                5,
                POOLS,
                clock.clone(),
                Some(Arc::new(RedisStore::open(&url, prefix.clone()).unwrap())),
            )
        };
        let (first, second) = (open(), open());
        first.admit("app", "app", 3).await.unwrap();
        assert_eq!(
            second.admit("sandbox", "jobs", 3).await,
            Err(Refusal::Spent {
                retry_after_ms: 60_000
            })
        );
        assert_eq!(second.admit("sandbox", "jobs", 2).await, Ok(()));
        forget(&url, &prefix).await;
    }

    #[tokio::test]
    async fn falls_back_to_memory_when_redis_does_not_answer() {
        let clock = TestClock::at(NOV_3_2026_20H);
        let store = RedisStore::open("redis://127.0.0.1:1", unique_prefix()).unwrap();
        let budget = Budget::new(2, POOLS, clock.clone(), Some(Arc::new(store)));

        assert_eq!(budget.admit("app", "app", 2).await, Ok(()));
        assert!(budget.admit("app", "app", 1).await.is_err());
        budget.record("app", "app", Some(429), Some("10")).await;
        clock.advance(60_000);
        assert_eq!(budget.admit("app", "app", 1).await, Ok(()));

        let snapshot = budget.snapshot().await;
        assert_eq!(snapshot.source, Source::Unavailable);
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
