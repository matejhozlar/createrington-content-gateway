use std::collections::HashMap;

use super::*;

const TOKEN: &str = "test-token-0123456789abcdef0123456789";
const OTHER_TOKEN: &str = "test-token-fedcba9876543210fedcba9876";
const API_KEY: &str = "test-curseforge-key";

fn lookup_in(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let env: HashMap<String, String> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    move |name| env.get(name).cloned()
}

fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let mut pairs = pairs.to_vec();
    if !pairs.iter().any(|(name, _)| *name == API_KEY_ENV) {
        pairs.push((API_KEY_ENV, API_KEY));
    }
    Config::from_lookup(lookup_in(&pairs))
}

fn config_with_token(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let mut pairs = pairs.to_vec();
    pairs.push((TOKEN_ENV, TOKEN));
    config_from(&pairs)
}

fn consumers_from(json: &str) -> Result<Config, ConfigError> {
    config_from(&[(CONSUMERS_ENV, json)])
}

#[test]
fn listens_on_loopback_by_default() {
    let config = config_with_token(&[]).unwrap();
    assert_eq!(config.listen, "127.0.0.1:5005".parse().unwrap());
}

#[test]
fn reads_host_and_port() {
    let config = config_with_token(&[(HOST_ENV, "0.0.0.0"), (PORT_ENV, "8080")]).unwrap();
    assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
}

#[test]
fn rejects_a_host_that_is_not_an_ip_address() {
    assert_eq!(
        config_with_token(&[(HOST_ENV, "localhost")]),
        Err(ConfigError::Host {
            value: "localhost".to_owned(),
        })
    );
}

#[test]
fn rejects_port_zero_and_ports_out_of_range() {
    for value in ["0", "65536", "http"] {
        assert_eq!(
            config_with_token(&[(PORT_ENV, value)]),
            Err(ConfigError::Port {
                value: value.to_owned(),
            })
        );
    }
}

#[test]
fn allows_300_calls_a_minute_by_default() {
    assert_eq!(config_with_token(&[]).unwrap().budget_per_minute, 300);
}

#[test]
fn reads_the_budget_and_rejects_zero() {
    assert_eq!(
        config_with_token(&[(BUDGET_ENV, "1000")])
            .unwrap()
            .budget_per_minute,
        1000
    );
    for value in ["0", "-1", "lots"] {
        assert_eq!(
            config_with_token(&[(BUDGET_ENV, value)]),
            Err(ConfigError::Budget {
                name: BUDGET_ENV,
                value: value.to_owned(),
            })
        );
    }
}

#[test]
fn allows_modrinth_300_calls_a_minute_unless_told_otherwise() {
    assert_eq!(config_with_token(&[]).unwrap().modrinth_per_minute, 300);
    assert_eq!(
        config_with_token(&[(MODRINTH_BUDGET_ENV, "100")])
            .unwrap()
            .modrinth_per_minute,
        100
    );
    assert_eq!(
        config_with_token(&[(MODRINTH_BUDGET_ENV, "0")]),
        Err(ConfigError::Budget {
            name: MODRINTH_BUDGET_ENV,
            value: "0".to_owned(),
        })
    );
}

#[test]
fn keeps_ninety_days_of_counts_unless_told_otherwise() {
    assert_eq!(config_with_token(&[]).unwrap().budget_history_days, 90);
    assert_eq!(
        config_with_token(&[(HISTORY_DAYS_ENV, "365")])
            .unwrap()
            .budget_history_days,
        365
    );
    for value in ["0", "-1", "3661", "forever"] {
        assert_eq!(
            config_with_token(&[(HISTORY_DAYS_ENV, value)]),
            Err(ConfigError::HistoryDays {
                value: value.to_owned(),
            })
        );
    }
}

#[test]
fn says_who_it_is_unless_told_what_to_say() {
    assert_eq!(
        config_with_token(&[]).unwrap().user_agent,
        DEFAULT_USER_AGENT
    );
    assert_eq!(
        config_with_token(&[(USER_AGENT_ENV, " my-app/1.0 (https://example.com) ")])
            .unwrap()
            .user_agent,
        "my-app/1.0 (https://example.com)"
    );
    assert_eq!(
        config_with_token(&[(USER_AGENT_ENV, "my-app/1.0\nX: y")]),
        Err(ConfigError::UserAgent {
            name: USER_AGENT_ENV,
            value: "my-app/1.0\nX: y".to_owned(),
        })
    );
}

#[test]
fn takes_the_modrinth_name_when_the_general_one_is_unset() {
    assert_eq!(
        config_with_token(&[(MODRINTH_USER_AGENT_ENV, "old-name/1.0")])
            .unwrap()
            .user_agent,
        "old-name/1.0"
    );
    assert_eq!(
        config_with_token(&[
            (USER_AGENT_ENV, "new-name/1.0"),
            (MODRINTH_USER_AGENT_ENV, "old-name/1.0"),
        ])
        .unwrap()
        .user_agent,
        "new-name/1.0"
    );
    assert_eq!(
        config_with_token(&[
            (USER_AGENT_ENV, ""),
            (MODRINTH_USER_AGENT_ENV, "old-name/1.0")
        ])
        .unwrap()
        .user_agent,
        "old-name/1.0"
    );
    assert_eq!(
        config_with_token(&[(MODRINTH_USER_AGENT_ENV, "old\tname")]),
        Err(ConfigError::UserAgent {
            name: MODRINTH_USER_AGENT_ENV,
            value: "old\tname".to_owned(),
        })
    );
}

#[test]
fn redis_is_optional() {
    assert_eq!(config_with_token(&[]).unwrap().redis_url, None);
    assert_eq!(
        config_with_token(&[(REDIS_ENV, "")]).unwrap().redis_url,
        None
    );
}

#[test]
fn reads_a_redis_url_and_keeps_its_password_out_of_debug_output() {
    let url = "redis://:hunter2-redis-password@127.0.0.1:6379/2";
    let config = config_with_token(&[(REDIS_ENV, url)]).unwrap();
    assert_eq!(config.redis_url.as_ref().map(RedisUrl::as_str), Some(url));
    assert!(!format!("{config:?}").contains("hunter2"));
}

#[test]
fn rejects_a_redis_url_that_does_not_parse_without_echoing_it() {
    for value in [
        "127.0.0.1:6379",
        "http://127.0.0.1:6379",
        "rediss://:secret@127.0.0.1:6379",
    ] {
        let error = config_with_token(&[(REDIS_ENV, value)]).unwrap_err();
        assert_eq!(error, ConfigError::RedisUrl, "{value}");
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn needs_a_curseforge_api_key_and_keeps_it_out_of_debug_output() {
    for value in ["", "  "] {
        assert_eq!(
            config_with_token(&[(API_KEY_ENV, value)]),
            Err(ConfigError::NoApiKey)
        );
    }
    let config = config_with_token(&[]).unwrap();
    assert_eq!(config.curseforge_api_key.as_str(), API_KEY);
    assert!(!format!("{config:?}").contains(API_KEY));
}

#[test]
fn a_single_token_is_one_consumer_with_one_pool() {
    let config = config_with_token(&[]).unwrap();
    assert_eq!(
        config.consumers,
        vec![Consumer {
            name: DEFAULT_CONSUMER.to_owned(),
            token: Token::new(TOKEN),
            pools: vec![DEFAULT_POOL.to_owned()],
        }]
    );
}

#[test]
fn reads_consumers_with_their_pools() {
    let json = format!(
        r#"[{{"name":"app-prod","token":"{TOKEN}","pools":["launcher","app"]}},
            {{"name":"sandbox","token":"{OTHER_TOKEN}","pools":["jobs"]}}]"#
    );
    let config = consumers_from(&json).unwrap();
    assert_eq!(config.consumers.len(), 2);
    assert_eq!(config.consumers[0].name, "app-prod");
    assert_eq!(config.consumers[0].pools, ["launcher", "app"]);
    assert_eq!(config.consumers[1].token, Token::new(OTHER_TOKEN));
}

#[test]
fn needs_exactly_one_way_of_naming_consumers() {
    assert_eq!(config_from(&[]), Err(ConfigError::NoConsumers));
    let json = format!(r#"[{{"name":"app","token":"{TOKEN}","pools":["app"]}}]"#);
    assert_eq!(
        config_from(&[(CONSUMERS_ENV, &json), (TOKEN_ENV, TOKEN)]),
        Err(ConfigError::ConsumerSources)
    );
}

#[test]
fn an_empty_variable_counts_as_unset() {
    let json = format!(r#"[{{"name":"app","token":"{TOKEN}","pools":["app"]}}]"#);
    let config = config_from(&[
        (CONSUMERS_ENV, &json),
        (TOKEN_ENV, ""),
        (HOST_ENV, " "),
        (PORT_ENV, ""),
        (BUDGET_ENV, ""),
    ])
    .unwrap();
    assert_eq!(config.consumers[0].name, "app");
    assert_eq!(config.listen, "127.0.0.1:5005".parse().unwrap());
    assert_eq!(config.budget_per_minute, 300);
    assert_eq!(
        config_from(&[(TOKEN_ENV, "  ")]),
        Err(ConfigError::NoConsumers)
    );

    let filter = log_filter_from_lookup(lookup_in(&[(LOG_ENV, "")])).unwrap();
    assert_eq!(filter.to_string(), DEFAULT_LOG);
}

#[test]
fn rejects_consumers_that_are_not_valid_json_without_echoing_them() {
    let json = format!(r#"[{{"name":"app","token":"{TOKEN}","pools":"{OTHER_TOKEN}"}}]"#);
    let error = consumers_from(&json).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::ConsumersJson {
            category: "data",
            ..
        }
    ));
    let message = error.to_string();
    assert!(!message.contains(TOKEN) && !message.contains(OTHER_TOKEN));
}

#[test]
fn rejects_unknown_fields() {
    let json = format!(r#"[{{"name":"app","token":"{TOKEN}","pools":["app"],"pool":"x"}}]"#);
    assert!(matches!(
        consumers_from(&json),
        Err(ConfigError::ConsumersJson { .. })
    ));
}

#[test]
fn rejects_bad_names_and_duplicates() {
    let cases = [
        (
            format!(r#"[{{"name":"App","token":"{TOKEN}","pools":["app"]}}]"#),
            ConfigError::ConsumerName {
                name: "App".to_owned(),
            },
        ),
        (
            format!(
                r#"[{{"name":"app","token":"{TOKEN}","pools":["app"]}},{{"name":"app","token":"{OTHER_TOKEN}","pools":["app"]}}]"#
            ),
            ConfigError::DuplicateConsumer {
                name: "app".to_owned(),
            },
        ),
        (
            format!(r#"[{{"name":"app","token":"{TOKEN}","pools":[]}}]"#),
            ConfigError::NoPools {
                consumer: "app".to_owned(),
            },
        ),
        (
            format!(r#"[{{"name":"app","token":"{TOKEN}","pools":["-jobs"]}}]"#),
            ConfigError::PoolName {
                consumer: "app".to_owned(),
                pool: "-jobs".to_owned(),
            },
        ),
        (
            format!(r#"[{{"name":"app","token":"{TOKEN}","pools":["app","app"]}}]"#),
            ConfigError::DuplicatePool {
                consumer: "app".to_owned(),
                pool: "app".to_owned(),
            },
        ),
    ];
    for (json, expected) in cases {
        assert_eq!(consumers_from(&json), Err(expected));
    }
}

#[test]
fn names_may_be_at_most_64_characters() {
    let longest = "a".repeat(64);
    let longer = "a".repeat(65);
    let consumers = |name: &str, pool: &str| {
        consumers_from(&format!(
            r#"[{{"name":"{name}","token":"{TOKEN}","pools":["{pool}"]}}]"#
        ))
    };
    assert!(consumers(&longest, &longest).is_ok());
    assert_eq!(
        consumers(&longer, "app"),
        Err(ConfigError::ConsumerName {
            name: longer.clone()
        })
    );
    assert_eq!(
        consumers("app", &longer),
        Err(ConfigError::PoolName {
            consumer: "app".to_owned(),
            pool: longer.clone(),
        })
    );
}

#[test]
fn rejects_short_and_shared_tokens() {
    assert_eq!(
        config_from(&[(TOKEN_ENV, "too-short")]),
        Err(ConfigError::ShortToken)
    );
    assert_eq!(
        consumers_from(r#"[{"name":"app","token":"too-short","pools":["app"]}]"#),
        Err(ConfigError::ShortConsumerToken {
            consumer: "app".to_owned(),
        })
    );
    for padded in [format!(" {TOKEN}"), format!("{TOKEN}\\n")] {
        assert_eq!(
            consumers_from(&format!(
                r#"[{{"name":"app","token":"{padded}","pools":["app"]}}]"#
            )),
            Err(ConfigError::PaddedToken {
                consumer: "app".to_owned(),
            })
        );
    }
    let json = format!(
        r#"[{{"name":"app","token":"{TOKEN}","pools":["app"]}},{{"name":"sandbox","token":"{TOKEN}","pools":["jobs"]}}]"#
    );
    assert_eq!(
        consumers_from(&json),
        Err(ConfigError::SharedToken {
            first: "app".to_owned(),
            second: "sandbox".to_owned(),
        })
    );
}

#[test]
fn trims_every_value() {
    let padded_token = format!(" {TOKEN}\n");
    let config = config_from(&[
        (TOKEN_ENV, &padded_token),
        (HOST_ENV, " 0.0.0.0 "),
        (PORT_ENV, " 8080\t"),
        (BUDGET_ENV, " 100 "),
        (HISTORY_DAYS_ENV, " 30 "),
        (API_KEY_ENV, " key\n"),
        (USER_AGENT_ENV, " my-app/1.0 "),
    ])
    .unwrap();
    assert_eq!(config.consumers[0].token, Token::new(TOKEN));
    assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
    assert_eq!(
        (config.budget_per_minute, config.budget_history_days),
        (100, 30)
    );
    assert_eq!(config.curseforge_api_key.as_str(), "key");
    assert_eq!(config.user_agent, "my-app/1.0");
    assert_eq!(
        config_from(&[(TOKEN_ENV, " too-short ")]),
        Err(ConfigError::ShortToken)
    );
}

#[test]
fn every_message_names_its_variable() {
    let invalid = |pairs: &[(&str, &str)]| config_with_token(pairs).unwrap_err().to_string();
    let consumers = |json: &str| consumers_from(json).unwrap_err().to_string();
    let messages = [
        (HOST_ENV, invalid(&[(HOST_ENV, "localhost")])),
        (PORT_ENV, invalid(&[(PORT_ENV, "0")])),
        (BUDGET_ENV, invalid(&[(BUDGET_ENV, "0")])),
        (MODRINTH_BUDGET_ENV, invalid(&[(MODRINTH_BUDGET_ENV, "0")])),
        (HISTORY_DAYS_ENV, invalid(&[(HISTORY_DAYS_ENV, "0")])),
        (REDIS_ENV, invalid(&[(REDIS_ENV, "127.0.0.1")])),
        (API_KEY_ENV, invalid(&[(API_KEY_ENV, " ")])),
        (CONSUMERS_ENV, config_from(&[]).unwrap_err().to_string()),
        (
            CONSUMERS_ENV,
            config_with_token(&[(CONSUMERS_ENV, "[]")])
                .unwrap_err()
                .to_string(),
        ),
        (CONSUMERS_ENV, consumers("{")),
        (USER_AGENT_ENV, invalid(&[(USER_AGENT_ENV, "a\tb")])),
        (
            TOKEN_ENV,
            config_from(&[(TOKEN_ENV, "too-short")])
                .unwrap_err()
                .to_string(),
        ),
        (
            CONSUMERS_ENV,
            consumers(r#"[{"name":"App","token":"x","pools":["app"]}]"#),
        ),
        (
            CONSUMERS_ENV,
            consumers(&format!(
                r#"[{{"name":"app","token":"{TOKEN}","pools":["app"]}},{{"name":"app","token":"{OTHER_TOKEN}","pools":["app"]}}]"#
            )),
        ),
        (
            CONSUMERS_ENV,
            consumers(&format!(
                r#"[{{"name":"app","token":"{TOKEN}","pools":[]}}]"#
            )),
        ),
        (
            CONSUMERS_ENV,
            consumers(&format!(
                r#"[{{"name":"app","token":"{TOKEN}","pools":["-x"]}}]"#
            )),
        ),
        (
            CONSUMERS_ENV,
            consumers(&format!(
                r#"[{{"name":"app","token":"{TOKEN}","pools":["a","a"]}}]"#
            )),
        ),
        (
            CONSUMERS_ENV,
            consumers(r#"[{"name":"app","token":"too-short","pools":["app"]}]"#),
        ),
        (
            CONSUMERS_ENV,
            consumers(&format!(
                r#"[{{"name":"app","token":" {TOKEN}","pools":["app"]}}]"#
            )),
        ),
        (
            CONSUMERS_ENV,
            consumers(&format!(
                r#"[{{"name":"app","token":"{TOKEN}","pools":["app"]}},{{"name":"jobs","token":"{TOKEN}","pools":["jobs"]}}]"#
            )),
        ),
        (
            LOG_ENV,
            log_filter_from_lookup(lookup_in(&[(LOG_ENV, "a=debgu")]))
                .unwrap_err()
                .to_string(),
        ),
    ];
    for (variable, message) in messages {
        assert!(message.starts_with(variable), "{message}");
    }
}

#[test]
fn never_shows_a_token_in_debug_output() {
    let config = config_with_token(&[]).unwrap();
    assert!(!format!("{config:?}").contains(TOKEN));
}

#[test]
fn logs_at_info_when_no_filter_is_set() {
    let filter = log_filter_from_lookup(lookup_in(&[])).unwrap();
    assert_eq!(filter.to_string(), DEFAULT_LOG);
}

#[test]
fn reads_a_log_filter() {
    let filter =
        log_filter_from_lookup(lookup_in(&[(LOG_ENV, "warn,content_gateway=debug")])).unwrap();
    assert!(filter.to_string().contains("content_gateway=debug"));
}

#[test]
fn rejects_a_log_filter_that_does_not_parse() {
    let error =
        log_filter_from_lookup(lookup_in(&[(LOG_ENV, "content_gateway=debgu")])).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::LogFilter { ref value, .. }
            if value == "content_gateway=debgu"
    ));
}
