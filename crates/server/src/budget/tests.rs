use super::{
    readout::{CountSource, DayPoolSnapshot},
    redis::admit_script,
    *,
};
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
            ::redis::cmd("KEYS")
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
            ::redis::cmd("PTTL")
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
            ::redis::cmd("KEYS")
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
