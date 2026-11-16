use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use redis::{ParsingError, Script, Value, aio::ConnectionManager};

use super::{Budget, Counts, DayCounts, MAX_HISTORY_DAYS, Verdict, readout::utc_date};
use crate::{
    auth::Pool,
    clock::{Clock, DAY_MS, MINUTE_MS},
    redis_store::RedisStore,
};

const MINUTE_KEY_TTL_MS: u64 = 2 * MINUTE_MS;
const DAYS_TRIMMED: u64 = 31;

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

pub(super) fn admit_script() -> String {
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

pub(super) struct Shared {
    pub(super) store: Arc<RedisStore>,
    namespace: &'static str,
    admit: Script,
    pub(super) hold: Script,
    pub(super) writing_days: tokio::sync::Mutex<()>,
    trimmed_on: Mutex<Option<String>>,
}

pub(super) struct Stored {
    pub(super) minute: HashMap<String, u64>,
    pub(super) held_until_ms: u64,
    pub(super) days: Vec<HashMap<String, u64>>,
}

impl Budget {
    pub(super) async fn admit_in_redis(
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
                .map(|(pool, counts)| (pool.to_string(), counts.used))
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
            .arg(pool.to_string())
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
                upstream = self.upstream.name(),
                code,
                "the budget script answered an unknown verdict, memory decides instead"
            );
            return None;
        };
        Some((verdict, held_until_ms))
    }

    pub(super) async fn read_redis(
        &self,
        redis: &Shared,
        now: u64,
        dates: &[String],
    ) -> Option<Stored> {
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
                    upstream = self.upstream.name(),
                    %error,
                    "Redis answered the budget readout with replies the gateway cannot read, the readout uses memory"
                );
                None
            }
        }
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
    pub(super) fn new(store: Arc<RedisStore>, namespace: &'static str) -> Self {
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

    pub(super) fn hold_key(&self) -> String {
        self.key("held-until")
    }

    fn day_key(&self, date: &str) -> String {
        self.key(&format!("day:{date}"))
    }

    pub(super) async fn write_days(
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

fn day_field(pool: &Pool, name: &str) -> String {
    format!("{pool}/{name}")
}

pub(super) fn parse_day_field(field: &str) -> Option<(Pool, &str)> {
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
