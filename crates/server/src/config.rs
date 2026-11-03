use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use serde::Deserialize;
use tracing_subscriber::EnvFilter;

const HOST_ENV: &str = "CONTENT_GATEWAY_HOST";
const PORT_ENV: &str = "CONTENT_GATEWAY_PORT";
const LOG_ENV: &str = "CONTENT_GATEWAY_LOG";
const CONSUMERS_ENV: &str = "CONTENT_GATEWAY_CONSUMERS";
const TOKEN_ENV: &str = "CONTENT_GATEWAY_TOKEN";
const BUDGET_ENV: &str = "CONTENT_GATEWAY_BUDGET_PER_MINUTE";
const REDIS_ENV: &str = "REDIS_URL";
const API_KEY_ENV: &str = "CURSEFORGE_API_KEY";
const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const DEFAULT_PORT: u16 = 5005;
const DEFAULT_BUDGET_PER_MINUTE: u32 = 300;
const MIN_TOKEN_LENGTH: usize = 32;
const MAX_NAME_LENGTH: usize = 64;
pub const DEFAULT_LOG: &str = "info";
pub const DEFAULT_CONSUMER: &str = "default";
pub const DEFAULT_POOL: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub consumers: Vec<Consumer>,
    pub budget_per_minute: u32,
    pub redis_url: Option<RedisUrl>,
    pub curseforge_api_key: ApiKey,
}

/// A process allowed to call the gateway, and the budget pools it may draw from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consumer {
    pub name: String,
    pub token: Token,
    pub pools: Vec<String>,
}

/// A consumer's bearer token. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token(..)")
    }
}

/// The CurseForge API key. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(..)")
    }
}

/// The Redis URL, which may carry a password. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct RedisUrl(String);

impl RedisUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedisUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedisUrl(..)")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{name} must be an IP address, got {value:?}")]
    Host { name: &'static str, value: String },
    #[error("{name} must be a port number from 1 to 65535, got {value:?}")]
    Port { name: &'static str, value: String },
    #[error("{name} is not a valid log filter, got {value:?}: {reason}")]
    LogFilter {
        name: &'static str,
        value: String,
        reason: String,
    },
    #[error("{name} must be a whole number of calls per minute from 1 up, got {value:?}")]
    Budget { name: &'static str, value: String },
    #[error(
        "{name} must be a URL such as redis://:password@127.0.0.1:6379/0 (rediss:// is not supported)"
    )]
    RedisUrl { name: &'static str },
    #[error("set {API_KEY_ENV} to the CurseForge API key the gateway calls CurseForge with")]
    NoApiKey,
    #[error("set {CONSUMERS_ENV} or {TOKEN_ENV} so that something can call the gateway")]
    NoConsumers,
    #[error("set {CONSUMERS_ENV} or {TOKEN_ENV}, not both")]
    ConsumerSources,
    #[error(
        "{CONSUMERS_ENV} must be a JSON array of {{\"name\", \"token\", \"pools\"}} objects ({category} error at line {line} column {column})"
    )]
    ConsumersJson {
        category: &'static str,
        line: usize,
        column: usize,
    },
    #[error(
        "consumer name {name:?} must be 1 to {MAX_NAME_LENGTH} characters of a-z, 0-9 and -, starting with a letter or digit"
    )]
    ConsumerName { name: String },
    #[error("consumer {name:?} is listed twice")]
    DuplicateConsumer { name: String },
    #[error("consumer {consumer:?} needs at least one pool")]
    NoPools { consumer: String },
    #[error(
        "pool name {pool:?} of consumer {consumer:?} must be 1 to {MAX_NAME_LENGTH} characters of a-z, 0-9 and -, starting with a letter or digit"
    )]
    PoolName { consumer: String, pool: String },
    #[error("consumer {consumer:?} lists pool {pool:?} twice")]
    DuplicatePool { consumer: String, pool: String },
    #[error("the token of consumer {consumer:?} must be at least {MIN_TOKEN_LENGTH} characters")]
    ShortToken { consumer: String },
    #[error("consumers {first:?} and {second:?} have the same token")]
    SharedToken { first: String, second: String },
}

pub fn log_filter_from_env() -> Result<EnvFilter, ConfigError> {
    log_filter_from_lookup(|name| std::env::var(name).ok())
}

fn log_filter_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<EnvFilter, ConfigError> {
    match set(lookup(LOG_ENV)) {
        None => Ok(EnvFilter::new(DEFAULT_LOG)),
        Some(value) => EnvFilter::try_new(&value).map_err(|error| ConfigError::LogFilter {
            name: LOG_ENV,
            value,
            reason: error.to_string(),
        }),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let lookup = |name: &str| set(lookup(name));
        let host = match lookup(HOST_ENV) {
            Some(value) => value.parse().map_err(|_| ConfigError::Host {
                name: HOST_ENV,
                value,
            })?,
            None => DEFAULT_HOST,
        };

        let port = match lookup(PORT_ENV) {
            Some(value) => match value.parse::<u16>() {
                Ok(port) if port != 0 => port,
                _ => {
                    return Err(ConfigError::Port {
                        name: PORT_ENV,
                        value,
                    });
                }
            },
            None => DEFAULT_PORT,
        };

        let budget_per_minute = match lookup(BUDGET_ENV) {
            Some(value) => match value.parse::<u32>() {
                Ok(budget) if budget != 0 => budget,
                _ => {
                    return Err(ConfigError::Budget {
                        name: BUDGET_ENV,
                        value,
                    });
                }
            },
            None => DEFAULT_BUDGET_PER_MINUTE,
        };

        let redis_url = match lookup(REDIS_ENV) {
            Some(value) => match redis::Client::open(value.as_str()) {
                Ok(_) => Some(RedisUrl(value)),
                Err(_) => return Err(ConfigError::RedisUrl { name: REDIS_ENV }),
            },
            None => None,
        };

        let consumers = match (lookup(CONSUMERS_ENV), lookup(TOKEN_ENV)) {
            (Some(_), Some(_)) => return Err(ConfigError::ConsumerSources),
            (None, None) => return Err(ConfigError::NoConsumers),
            (Some(json), None) => parse_consumers(&json)?,
            (None, Some(token)) => vec![Consumer {
                name: DEFAULT_CONSUMER.to_owned(),
                token: Token(token),
                pools: vec![DEFAULT_POOL.to_owned()],
            }],
        };
        validate_consumers(&consumers)?;

        let curseforge_api_key = lookup(API_KEY_ENV)
            .map(|key| ApiKey(key.trim().to_owned()))
            .ok_or(ConfigError::NoApiKey)?;

        Ok(Self {
            listen: SocketAddr::new(host, port),
            consumers,
            budget_per_minute,
            redis_url,
            curseforge_api_key,
        })
    }
}

fn set(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn parse_consumers(json: &str) -> Result<Vec<Consumer>, ConfigError> {
    serde_json::from_str(json).map_err(|error| ConfigError::ConsumersJson {
        category: match error.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "end of input",
        },
        line: error.line(),
        column: error.column(),
    })
}

fn validate_consumers(consumers: &[Consumer]) -> Result<(), ConfigError> {
    let mut names = HashSet::new();
    for (index, consumer) in consumers.iter().enumerate() {
        if !is_valid_name(&consumer.name) {
            return Err(ConfigError::ConsumerName {
                name: consumer.name.clone(),
            });
        }
        if !names.insert(consumer.name.as_str()) {
            return Err(ConfigError::DuplicateConsumer {
                name: consumer.name.clone(),
            });
        }
        if consumer.pools.is_empty() {
            return Err(ConfigError::NoPools {
                consumer: consumer.name.clone(),
            });
        }
        let mut pools = HashSet::new();
        for pool in &consumer.pools {
            if !is_valid_name(pool) {
                return Err(ConfigError::PoolName {
                    consumer: consumer.name.clone(),
                    pool: pool.clone(),
                });
            }
            if !pools.insert(pool.as_str()) {
                return Err(ConfigError::DuplicatePool {
                    consumer: consumer.name.clone(),
                    pool: pool.clone(),
                });
            }
        }
        if consumer.token.0.chars().count() < MIN_TOKEN_LENGTH {
            return Err(ConfigError::ShortToken {
                consumer: consumer.name.clone(),
            });
        }
        if let Some(first) = consumers[..index]
            .iter()
            .find(|earlier| earlier.token == consumer.token)
        {
            return Err(ConfigError::SharedToken {
                first: first.name.clone(),
                second: consumer.name.clone(),
            });
        }
    }
    Ok(())
}

fn is_valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    name.len() <= MAX_NAME_LENGTH
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

#[cfg(test)]
mod tests {
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
                name: HOST_ENV,
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
                    name: PORT_ENV,
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
            assert_eq!(error, ConfigError::RedisUrl { name: REDIS_ENV }, "{value}");
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
    fn rejects_short_and_shared_tokens() {
        assert_eq!(
            config_from(&[(TOKEN_ENV, "too-short")]),
            Err(ConfigError::ShortToken {
                consumer: DEFAULT_CONSUMER.to_owned(),
            })
        );
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
            ConfigError::LogFilter { name: LOG_ENV, ref value, .. }
                if value == "content_gateway=debgu"
        ));
    }
}
