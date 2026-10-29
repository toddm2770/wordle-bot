use std::path::PathBuf;

use chrono_tz::Tz;

use crate::error::{Error, Result};
use crate::fluxer::endpoints::EndpointConfig;

pub const DEFAULT_INSTANCE_URL: &str = "https://fluxer.squeekienuts.net";

#[derive(Clone)]
pub struct Config {
    pub bot_token: String,
    pub endpoints: EndpointConfig,
    pub database_url: String,
    pub timezone: Tz,
    pub command_prefix: String,
    pub nyt_base_url: String,
    pub words_file: Option<PathBuf>,
    pub leaderboard_size: usize,
    /// Replace each day's individual result posts with one recap after midnight.
    pub compact_results: bool,
}

// Hand-written so the token and database password never reach the logs.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bot_token", &"<redacted>")
            .field("endpoints", &self.endpoints)
            .field("database_url", &"<redacted>")
            .field("timezone", &self.timezone)
            .field("command_prefix", &self.command_prefix)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Config::from_lookup(|k| std::env::var(k).ok())
    }

    /// Empty values count as unset so `docker compose` can pass blank variables through.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config> {
        let val = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let required = |k: &str| {
            val(k).ok_or_else(|| Error::Config(format!("{k} is required (see .env.example)")))
        };

        let bot_token = required("FLUXER_BOT_TOKEN")?;
        if !bot_token.contains('.') {
            return Err(Error::Config(
                "FLUXER_BOT_TOKEN does not look like a bot token (expected `<application_id>.<secret>`). \
                 Copy it from User Settings > Applications > your application > Bot."
                    .into(),
            ));
        }

        let timezone: Tz = val("PUZZLE_TIMEZONE")
            .unwrap_or_else(|| "America/New_York".into())
            .parse()
            .map_err(|_| Error::Config("PUZZLE_TIMEZONE is not a valid IANA timezone (e.g. America/New_York)".into()))?;

        let leaderboard_size = match val("LEADERBOARD_SIZE") {
            Some(v) => v
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=50).contains(n))
                .ok_or_else(|| Error::Config("LEADERBOARD_SIZE must be a number from 1 to 50".into()))?,
            None => 15,
        };

        let compact_results = match val("COMPACT_RESULTS").map(|v| v.to_ascii_lowercase()) {
            None => true,
            Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => true,
            Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => false,
            Some(_) => return Err(Error::Config("COMPACT_RESULTS must be true or false".into())),
        };

        Ok(Config {
            bot_token,
            endpoints: EndpointConfig {
                instance_url: val("FLUXER_INSTANCE_URL").unwrap_or_else(|| DEFAULT_INSTANCE_URL.into()),
                api_override: val("FLUXER_API_URL"),
                gateway_override: val("FLUXER_GATEWAY_URL"),
            },
            database_url: required("DATABASE_URL")?,
            timezone,
            command_prefix: val("COMMAND_PREFIX").unwrap_or_else(|| "!wordle".into()),
            nyt_base_url: val("NYT_WORDLE_URL").unwrap_or_else(|| "https://www.nytimes.com/svc/wordle/v2".into()),
            words_file: val("WORDS_FILE").map(PathBuf::from),
            leaderboard_size,
            compact_results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let c = cfg(&[("FLUXER_BOT_TOKEN", "123.abc"), ("DATABASE_URL", "postgres://u:p@db/x")]).unwrap();
        assert_eq!(c.endpoints.instance_url, DEFAULT_INSTANCE_URL);
        assert!(c.endpoints.api_override.is_none() && c.endpoints.gateway_override.is_none());
        assert_eq!(c.timezone, chrono_tz::America::New_York);
        assert_eq!(c.command_prefix, "!wordle");
        assert_eq!(c.leaderboard_size, 15);
        assert!(c.compact_results);
    }

    #[test]
    fn blank_values_count_as_unset() {
        let c = cfg(&[
            ("FLUXER_BOT_TOKEN", "123.abc"),
            ("DATABASE_URL", "postgres://x"),
            ("FLUXER_API_URL", "  "),
            ("FLUXER_GATEWAY_URL", ""),
        ])
        .unwrap();
        assert!(c.endpoints.api_override.is_none() && c.endpoints.gateway_override.is_none());
    }

    #[test]
    fn overrides_are_read() {
        let c = cfg(&[
            ("FLUXER_BOT_TOKEN", "123.abc"),
            ("DATABASE_URL", "postgres://x"),
            ("FLUXER_API_URL", "http://api:8080"),
            ("FLUXER_GATEWAY_URL", "ws://gateway:8080/"),
        ])
        .unwrap();
        assert_eq!(c.endpoints.api_override.as_deref(), Some("http://api:8080"));
        assert_eq!(c.endpoints.gateway_override.as_deref(), Some("ws://gateway:8080/"));
    }

    #[test]
    fn rejects_missing_and_malformed_values() {
        assert!(cfg(&[("DATABASE_URL", "x")]).is_err());
        assert!(cfg(&[("FLUXER_BOT_TOKEN", "123.abc")]).is_err());
        assert!(cfg(&[("FLUXER_BOT_TOKEN", "nodot"), ("DATABASE_URL", "x")]).is_err());
        assert!(cfg(&[("FLUXER_BOT_TOKEN", "1.a"), ("DATABASE_URL", "x"), ("PUZZLE_TIMEZONE", "Mars/Base")]).is_err());
        assert!(cfg(&[("FLUXER_BOT_TOKEN", "1.a"), ("DATABASE_URL", "x"), ("LEADERBOARD_SIZE", "0")]).is_err());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let c = cfg(&[("FLUXER_BOT_TOKEN", "123.supersecret"), ("DATABASE_URL", "postgres://u:hunter2@db/x")]).unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("supersecret") && !dbg.contains("hunter2"));
    }
}
