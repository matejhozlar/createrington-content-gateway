use std::{collections::HashMap, sync::Arc};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::{Budget, DayCounts, MinuteCounts, redis::parse_day_field, timestamp, utc_date};
use crate::{
    app::AppState,
    auth::Pool,
    clock::{DAY_MS, MINUTE_MS},
    error::ApiError,
};

const DEFAULT_SNAPSHOT_DAYS: u32 = 14;

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
    pub(super) fn and(self, other: Self) -> Self {
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
    pub(super) history_days: u32,
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
    pub(super) source: CountSource,
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

impl Budget {
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
                        let field = pool.to_string();
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
                                upstream = self.upstream.name(),
                                %field,
                                "Redis holds a daily count the gateway does not keep, the readout leaves it out"
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

fn iso(ms: u64) -> String {
    timestamp(ms).to_string()
}
