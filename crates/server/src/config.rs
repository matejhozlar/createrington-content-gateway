use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::RangeInclusive,
    str::FromStr,
};

use redis::{ConnectionInfo, IntoConnectionInfo};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use crate::budget::{DEFAULT_HISTORY_DAYS, MAX_HISTORY_DAYS};

const HOST_ENV: &str = "CONTENT_GATEWAY_HOST";
const PORT_ENV: &str = "CONTENT_GATEWAY_PORT";
const LOG_ENV: &str = "CONTENT_GATEWAY_LOG";
const CONSUMERS_ENV: &str = "CONTENT_GATEWAY_CONSUMERS";
const TOKEN_ENV: &str = "CONTENT_GATEWAY_TOKEN";
const BUDGET_ENV: &str = "CONTENT_GATEWAY_BUDGET_PER_MINUTE";
const MODRINTH_BUDGET_ENV: &str = "CONTENT_GATEWAY_MODRINTH_PER_MINUTE";
const HISTORY_DAYS_ENV: &str = "CONTENT_GATEWAY_BUDGET_HISTORY_DAYS";
const REDIS_ENV: &str = "REDIS_URL";
const API_KEY_ENV: &str = "CURSEFORGE_API_KEY";
const USER_AGENT_ENV: &str = "CONTENT_GATEWAY_USER_AGENT";
const MODRINTH_USER_AGENT_ENV: &str = "MODRINTH_USER_AGENT";
const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const DEFAULT_PORT: u16 = 5005;
const DEFAULT_BUDGET_PER_MINUTE: u32 = 300;
const DEFAULT_MODRINTH_PER_MINUTE: u32 = 300;
const MIN_TOKEN_LENGTH: usize = 32;
const MAX_NAME_LENGTH: usize = 64;
/// What the gateway calls itself to every upstream unless `CONTENT_GATEWAY_USER_AGENT` says otherwise.
pub const DEFAULT_USER_AGENT: &str = concat!("content-gateway/", env!("CARGO_PKG_VERSION"));
pub const DEFAULT_LOG: &str = "info";
const DEFAULT_CONSUMER: &str = "default";
const DEFAULT_POOL: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub consumers: Vec<Consumer>,
    pub budget_per_minute: u32,
    pub modrinth_per_minute: u32,
    pub budget_history_days: u32,
    pub redis_url: Option<RedisUrl>,
    pub curseforge_api_key: ApiKey,
    pub user_agent: String,
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
    #[cfg(test)]
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
    #[cfg(test)]
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

/// The Redis URL, read once into the connection it names, which may carry a password. Its `Debug` output never
/// shows the value.
#[derive(Clone)]
pub struct RedisUrl {
    url: String,
    connection: ConnectionInfo,
}

impl RedisUrl {
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.url
    }

    pub fn connection(&self) -> ConnectionInfo {
        self.connection.clone()
    }
}

impl PartialEq for RedisUrl {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl Eq for RedisUrl {}

impl fmt::Debug for RedisUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedisUrl(..)")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{HOST_ENV} must be an IP address, got {value:?}")]
    Host { value: String },
    #[error("{PORT_ENV} must be a port number from 1 to 65535, got {value:?}")]
    Port { value: String },
    #[error("{LOG_ENV} is not a valid log filter, got {value:?}: {reason}")]
    LogFilter { value: String, reason: String },
    #[error("{name} must be a whole number of calls per minute from 1 up, got {value:?}")]
    Budget { name: &'static str, value: String },
    #[error(
        "{HISTORY_DAYS_ENV} must be a whole number of days from 1 to {MAX_HISTORY_DAYS}, got {value:?}"
    )]
    HistoryDays { value: String },
    #[error(
        "{REDIS_ENV} must be a URL such as redis://:password@127.0.0.1:6379/0 (rediss:// is not supported)"
    )]
    RedisUrl,
    #[error(
        "{name} must be a User-Agent of printable ASCII, such as my-app/1.0 (https://example.com), got {value:?}"
    )]
    UserAgent { name: &'static str, value: String },
    #[error(
        "{API_KEY_ENV} must be set to the CurseForge API key the gateway calls CurseForge with"
    )]
    NoApiKey,
    #[error("{CONSUMERS_ENV} or {TOKEN_ENV} must be set so that something can call the gateway")]
    NoConsumers,
    #[error("{CONSUMERS_ENV} and {TOKEN_ENV} are both set, set only one of them")]
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
        "{CONSUMERS_ENV}: consumer name {name:?} must be 1 to {MAX_NAME_LENGTH} characters of a-z, 0-9 and -, starting with a letter or digit"
    )]
    ConsumerName { name: String },
    #[error("{CONSUMERS_ENV} lists consumer {name:?} twice")]
    DuplicateConsumer { name: String },
    #[error("{CONSUMERS_ENV}: consumer {consumer:?} needs at least one pool")]
    NoPools { consumer: String },
    #[error(
        "{CONSUMERS_ENV}: pool name {pool:?} of consumer {consumer:?} must be 1 to {MAX_NAME_LENGTH} characters of a-z, 0-9 and -, starting with a letter or digit"
    )]
    PoolName { consumer: String, pool: String },
    #[error("{CONSUMERS_ENV}: consumer {consumer:?} lists pool {pool:?} twice")]
    DuplicatePool { consumer: String, pool: String },
    #[error("{TOKEN_ENV} must be at least {MIN_TOKEN_LENGTH} characters")]
    ShortToken,
    #[error(
        "{CONSUMERS_ENV}: the token of consumer {consumer:?} must be at least {MIN_TOKEN_LENGTH} characters"
    )]
    ShortConsumerToken { consumer: String },
    #[error(
        "{CONSUMERS_ENV}: the token of consumer {consumer:?} starts or ends with whitespace, which no request can send"
    )]
    PaddedToken { consumer: String },
    #[error("{CONSUMERS_ENV}: consumers {first:?} and {second:?} have the same token")]
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
            value,
            reason: error.to_string(),
        }),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the configuration from `lookup`, which gives a variable's value by its name.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let lookup = |name: &str| set(lookup(name));
        let host = match lookup(HOST_ENV) {
            Some(value) => value.parse().map_err(|_| ConfigError::Host { value })?,
            None => DEFAULT_HOST,
        };
        let port = number(lookup(PORT_ENV), DEFAULT_PORT, 1..=u16::MAX, |value| {
            ConfigError::Port { value }
        })?;
        let budget = |name| move |value| ConfigError::Budget { name, value };
        let budget_per_minute = number(
            lookup(BUDGET_ENV),
            DEFAULT_BUDGET_PER_MINUTE,
            1..=u32::MAX,
            budget(BUDGET_ENV),
        )?;
        let modrinth_per_minute = number(
            lookup(MODRINTH_BUDGET_ENV),
            DEFAULT_MODRINTH_PER_MINUTE,
            1..=u32::MAX,
            budget(MODRINTH_BUDGET_ENV),
        )?;
        let budget_history_days = number(
            lookup(HISTORY_DAYS_ENV),
            DEFAULT_HISTORY_DAYS,
            1..=MAX_HISTORY_DAYS,
            |value| ConfigError::HistoryDays { value },
        )?;

        let redis_url = match lookup(REDIS_ENV) {
            Some(url) => {
                let connection = url
                    .as_str()
                    .into_connection_info()
                    .map_err(|_| ConfigError::RedisUrl)?;
                Some(RedisUrl { url, connection })
            }
            None => None,
        };

        let consumers = match (lookup(CONSUMERS_ENV), lookup(TOKEN_ENV)) {
            (Some(_), Some(_)) => return Err(ConfigError::ConsumerSources),
            (None, None) => return Err(ConfigError::NoConsumers),
            (Some(json), None) => parse_consumers(&json)?,
            (None, Some(token)) if token.chars().count() < MIN_TOKEN_LENGTH => {
                return Err(ConfigError::ShortToken);
            }
            (None, Some(token)) => vec![Consumer {
                name: DEFAULT_CONSUMER.to_owned(),
                token: Token(token),
                pools: vec![DEFAULT_POOL.to_owned()],
            }],
        };
        validate_consumers(&consumers)?;

        let curseforge_api_key = lookup(API_KEY_ENV)
            .map(ApiKey)
            .ok_or(ConfigError::NoApiKey)?;

        let user_agent = match [USER_AGENT_ENV, MODRINTH_USER_AGENT_ENV]
            .into_iter()
            .find_map(|name| lookup(name).map(|value| (name, value)))
        {
            Some((_, value)) if value.bytes().all(|byte| (b' '..=b'~').contains(&byte)) => value,
            Some((name, value)) => return Err(ConfigError::UserAgent { name, value }),
            None => DEFAULT_USER_AGENT.to_owned(),
        };

        Ok(Self {
            listen: SocketAddr::new(host, port),
            consumers,
            budget_per_minute,
            modrinth_per_minute,
            budget_history_days,
            redis_url,
            curseforge_api_key,
            user_agent,
        })
    }
}

fn set(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn number<T: FromStr + PartialOrd>(
    value: Option<String>,
    default: T,
    allowed: RangeInclusive<T>,
    invalid: impl FnOnce(String) -> ConfigError,
) -> Result<T, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value.parse::<T>() {
        Ok(number) if allowed.contains(&number) => Ok(number),
        _ => Err(invalid(value)),
    }
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
        if consumer.token.0.trim() != consumer.token.0 {
            return Err(ConfigError::PaddedToken {
                consumer: consumer.name.clone(),
            });
        }
        if consumer.token.0.chars().count() < MIN_TOKEN_LENGTH {
            return Err(ConfigError::ShortConsumerToken {
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
mod tests;
