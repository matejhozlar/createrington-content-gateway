use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tracing_subscriber::EnvFilter;

const HOST_ENV: &str = "CONTENT_GATEWAY_HOST";
const PORT_ENV: &str = "CONTENT_GATEWAY_PORT";
const LOG_ENV: &str = "CONTENT_GATEWAY_LOG";
const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const DEFAULT_PORT: u16 = 5005;
pub const DEFAULT_LOG: &str = "info";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
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
}

pub fn log_filter_from_env() -> Result<EnvFilter, ConfigError> {
    log_filter_from_lookup(|name| std::env::var(name).ok())
}

fn log_filter_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<EnvFilter, ConfigError> {
    match lookup(LOG_ENV) {
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

        Ok(Self {
            listen: SocketAddr::new(host, port),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        Config::from_lookup(|name| env.get(name).cloned())
    }

    #[test]
    fn listens_on_loopback_by_default() {
        let config = config_from(&[]).unwrap();
        assert_eq!(config.listen, "127.0.0.1:5005".parse().unwrap());
    }

    #[test]
    fn reads_host_and_port() {
        let config = config_from(&[(HOST_ENV, "0.0.0.0"), (PORT_ENV, "8080")]).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8080".parse().unwrap());
    }

    #[test]
    fn rejects_a_host_that_is_not_an_ip_address() {
        assert_eq!(
            config_from(&[(HOST_ENV, "localhost")]),
            Err(ConfigError::Host {
                name: HOST_ENV,
                value: "localhost".to_owned(),
            })
        );
    }

    fn log_filter_from(pairs: &[(&str, &str)]) -> Result<EnvFilter, ConfigError> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        log_filter_from_lookup(|name| env.get(name).cloned())
    }

    #[test]
    fn logs_at_info_when_no_filter_is_set() {
        let filter = log_filter_from(&[]).unwrap();
        assert_eq!(filter.to_string(), DEFAULT_LOG);
    }

    #[test]
    fn reads_a_log_filter() {
        let filter = log_filter_from(&[(LOG_ENV, "warn,content_gateway=debug")]).unwrap();
        assert!(filter.to_string().contains("content_gateway=debug"));
    }

    #[test]
    fn rejects_a_log_filter_that_does_not_parse() {
        let error = log_filter_from(&[(LOG_ENV, "content_gateway=debgu")]).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::LogFilter { name: LOG_ENV, ref value, .. }
                if value == "content_gateway=debgu"
        ));
    }

    #[test]
    fn rejects_port_zero_and_ports_out_of_range() {
        for value in ["0", "65536", "http"] {
            assert_eq!(
                config_from(&[(PORT_ENV, value)]),
                Err(ConfigError::Port {
                    name: PORT_ENV,
                    value: value.to_owned(),
                })
            );
        }
    }
}
