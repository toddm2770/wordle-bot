//! Resolving where the Fluxer REST API and Gateway live.
//!
//! Verified against Fluxer's docs and source:
//! * Discovery document: `GET {instance}/.well-known/fluxer` (no `/v1` prefix,
//!   unauthenticated). Third-party clients use `endpoints.api_public` for REST
//!   and `endpoints.gateway` for the WebSocket.
//! * REST operations live under `{api}/v1/...` (`/v1` is the only version).
//! * Gateway needs `?v=1&encoding=json`; there is no intents system.
//! * Bot invitations are authorised in the web app at
//!   `{webapp}/oauth2/authorize?client_id=<application id>&scope=bot&permissions=<mask>`.
//!
//! In the stock self-hosting stack the edge proxy serves the API at `/api`
//! (-> `api:8080`) and the Gateway at `/gateway` (-> `gateway:8080`), so inside the
//! Fluxer Docker network the bot can use `http://api:8080` and `ws://gateway:8080/`.

use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};

/// VIEW_CHANNEL (1<<10) | SEND_MESSAGES (1<<11) | MANAGE_MESSAGES (1<<13) | EMBED_LINKS (1<<14).
/// MANAGE_MESSAGES is only used to delete a guess someone mistakenly typed in a public channel.
pub const BOT_PERMISSIONS: u64 = (1 << 10) | (1 << 11) | (1 << 13) | (1 << 14);
/// Same without MANAGE_MESSAGES.
pub const BOT_PERMISSIONS_MINIMAL: u64 = (1 << 10) | (1 << 11) | (1 << 14);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// REST origin + path prefix, without the trailing `/v1` (e.g. `https://host/api`).
    pub api_base: String,
    /// Gateway WebSocket URL including `v=1&encoding=json`.
    pub gateway_url: String,
    /// Web application origin, used to build the invite link.
    pub web_app: String,
    pub api_source: Source,
    pub gateway_source: Source,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Override,
    Discovery,
}

impl Endpoints {
    pub fn api_url(&self, path: &str) -> String {
        format!("{}/v1/{}", self.api_base, path.trim_start_matches('/'))
    }

    pub fn invite_url(&self, application_id: &str, permissions: u64) -> String {
        invite_url(&self.web_app, application_id, permissions)
    }
}

#[derive(Debug, Deserialize)]
pub struct Discovery {
    pub endpoints: DiscoveryEndpoints,
}

#[derive(Debug, Deserialize)]
pub struct DiscoveryEndpoints {
    pub api_public: Option<String>,
    pub api: Option<String>,
    pub gateway: Option<String>,
    pub webapp: Option<String>,
}

/// User-supplied endpoint configuration.
#[derive(Clone, Debug, Default)]
pub struct EndpointConfig {
    pub instance_url: String,
    pub api_override: Option<String>,
    pub gateway_override: Option<String>,
}

pub fn invite_url(web_app: &str, application_id: &str, permissions: u64) -> String {
    format!(
        "{}/oauth2/authorize?client_id={}&scope=bot&permissions={}",
        web_app.trim_end_matches('/'),
        application_id,
        permissions
    )
}

/// Accepts `http://api:8080`, `https://host/api`, or either with a trailing `/v1`.
pub fn normalize_api_base(raw: &str) -> Result<String> {
    let url = Url::parse(raw.trim()).map_err(|e| Error::Config(format!("invalid API URL `{raw}`: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Config(format!("API URL `{raw}` must start with http:// or https://")));
    }
    let mut s = url.as_str().trim_end_matches('/').to_string();
    if let Some(stripped) = s.strip_suffix("/v1") {
        s = stripped.to_string();
    }
    Ok(s)
}

/// Accepts `ws://gateway:8080/` or `wss://host/gateway`, with or without query,
/// and forces `v=1&encoding=json` (the only values Fluxer implements).
pub fn normalize_gateway_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw.trim()).map_err(|e| Error::Config(format!("invalid gateway URL `{raw}`: {e}")))?;
    match url.scheme() {
        "ws" | "wss" => {}
        "http" => {
            let _ = url.set_scheme("ws");
        }
        "https" => {
            let _ = url.set_scheme("wss");
        }
        _ => {
            return Err(Error::Config(format!(
                "gateway URL `{raw}` must start with ws:// or wss://"
            )))
        }
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "v" && k != "encoding")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut q = url.query_pairs_mut();
        q.clear();
        q.append_pair("v", "1");
        q.append_pair("encoding", "json");
        for (k, v) in kept {
            q.append_pair(&k, &v);
        }
    }
    Ok(url.to_string())
}

fn normalize_origin(raw: &str) -> Result<String> {
    let url = Url::parse(raw.trim()).map_err(|e| Error::Config(format!("invalid instance URL `{raw}`: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Config(format!(
            "instance URL `{raw}` must start with http:// or https://"
        )));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Pure resolution step. `discovery` is only consulted for values without an override.
pub fn resolve(cfg: &EndpointConfig, discovery: Option<&Discovery>) -> Result<Endpoints> {
    let instance = normalize_origin(&cfg.instance_url)?;

    let (api_base, api_source) = match &cfg.api_override {
        Some(o) => (normalize_api_base(o)?, Source::Override),
        None => {
            let d = discovery
                .ok_or_else(|| Error::Config("no FLUXER_API_URL set and discovery was unavailable".into()))?;
            let raw = d
                .endpoints
                .api_public
                .as_deref()
                .or(d.endpoints.api.as_deref())
                .ok_or_else(|| Error::Config("discovery document has no endpoints.api_public".into()))?;
            (normalize_api_base(raw)?, Source::Discovery)
        }
    };

    let (gateway_url, gateway_source) = match &cfg.gateway_override {
        Some(o) => (normalize_gateway_url(o)?, Source::Override),
        None => {
            let d = discovery
                .ok_or_else(|| Error::Config("no FLUXER_GATEWAY_URL set and discovery was unavailable".into()))?;
            let raw = d
                .endpoints
                .gateway
                .as_deref()
                .ok_or_else(|| Error::Config("discovery document has no endpoints.gateway".into()))?;
            (normalize_gateway_url(raw)?, Source::Discovery)
        }
    };

    let web_app = match discovery.and_then(|d| d.endpoints.webapp.as_deref()) {
        Some(w) => normalize_origin(w).unwrap_or(instance),
        None => instance,
    };

    Ok(Endpoints { api_base, gateway_url, web_app, api_source, gateway_source })
}

/// True unless both endpoints are overridden.
pub fn needs_discovery(cfg: &EndpointConfig) -> bool {
    cfg.api_override.is_none() || cfg.gateway_override.is_none()
}

pub async fn fetch_discovery(http: &reqwest::Client, instance_url: &str) -> Result<Discovery> {
    let url = format!("{}/.well-known/fluxer", normalize_origin(instance_url)?);
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| Error::Config(format!("could not reach {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Config(format!(
            "discovery request to {url} returned HTTP {status} (is this a Fluxer instance, and is /.well-known/fluxer routed to the API?)"
        )));
    }
    resp.json::<Discovery>()
        .await
        .map_err(|e| Error::Config(format!("{url} did not return a Fluxer discovery document: {e}")))
}

/// Resolve endpoints, fetching the discovery document only if needed.
pub async fn discover(http: &reqwest::Client, cfg: &EndpointConfig) -> Result<Endpoints> {
    let discovery = if needs_discovery(cfg) {
        Some(fetch_discovery(http, &cfg.instance_url).await?)
    } else {
        None
    };
    resolve(cfg, discovery.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovery() -> Discovery {
        serde_json::from_str(
            r#"{"api_code_version":1,"endpoints":{"api":"https://fluxer.example.net/api","api_client":"https://fluxer.example.net/api","api_public":"https://fluxer.example.net/api","gateway":"wss://fluxer.example.net/gateway","webapp":"https://fluxer.example.net","media":"x"},"captcha":{}}"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_from_discovery() {
        let cfg = EndpointConfig { instance_url: "https://fluxer.example.net/".into(), ..Default::default() };
        let e = resolve(&cfg, Some(&discovery())).unwrap();
        assert_eq!(e.api_base, "https://fluxer.example.net/api");
        assert_eq!(e.api_url("channels/1/messages"), "https://fluxer.example.net/api/v1/channels/1/messages");
        assert_eq!(e.gateway_url, "wss://fluxer.example.net/gateway?v=1&encoding=json");
        assert_eq!(e.web_app, "https://fluxer.example.net");
        assert_eq!((e.api_source, e.gateway_source), (Source::Discovery, Source::Discovery));
    }

    #[test]
    fn overrides_win_and_need_no_discovery() {
        let cfg = EndpointConfig {
            instance_url: "https://fluxer.example.net".into(),
            api_override: Some("http://api:8080/v1/".into()),
            gateway_override: Some("ws://gateway:8080/".into()),
        };
        assert!(!needs_discovery(&cfg));
        let e = resolve(&cfg, None).unwrap();
        assert_eq!(e.api_base, "http://api:8080");
        assert_eq!(e.api_url("/applications/@me"), "http://api:8080/v1/applications/@me");
        assert_eq!(e.gateway_url, "ws://gateway:8080/?v=1&encoding=json");
        assert_eq!(e.web_app, "https://fluxer.example.net");
        assert_eq!(e.api_source, Source::Override);
    }

    #[test]
    fn mixed_override_uses_discovery_for_the_rest() {
        let cfg = EndpointConfig {
            instance_url: "https://fluxer.example.net".into(),
            api_override: Some("http://api:8080".into()),
            gateway_override: None,
        };
        assert!(needs_discovery(&cfg));
        let e = resolve(&cfg, Some(&discovery())).unwrap();
        assert_eq!(e.api_base, "http://api:8080");
        assert!(e.gateway_url.starts_with("wss://fluxer.example.net/gateway"));
    }

    #[test]
    fn missing_discovery_is_a_config_error() {
        let cfg = EndpointConfig { instance_url: "https://fluxer.example.net".into(), ..Default::default() };
        assert!(matches!(resolve(&cfg, None), Err(Error::Config(_))));
    }

    #[test]
    fn api_base_normalisation() {
        assert_eq!(normalize_api_base("https://h/api/v1").unwrap(), "https://h/api");
        assert_eq!(normalize_api_base("https://h/api/").unwrap(), "https://h/api");
        assert_eq!(normalize_api_base("http://api:8080").unwrap(), "http://api:8080");
        assert!(normalize_api_base("ftp://x").is_err());
        assert!(normalize_api_base("nonsense").is_err());
    }

    #[test]
    fn gateway_url_normalisation_forces_version_and_encoding() {
        assert_eq!(
            normalize_gateway_url("wss://h/gateway?v=9&encoding=etf").unwrap(),
            "wss://h/gateway?v=1&encoding=json"
        );
        assert_eq!(normalize_gateway_url("https://h/gateway").unwrap(), "wss://h/gateway?v=1&encoding=json");
        assert!(normalize_gateway_url("ftp://h").is_err());
    }

    #[test]
    fn permission_masks() {
        assert_eq!(BOT_PERMISSIONS, 27648);
        assert_eq!(BOT_PERMISSIONS_MINIMAL, 19456);
    }

    #[test]
    fn builds_invite_url() {
        assert_eq!(
            invite_url("https://fluxer.example.net/", "1234567890", BOT_PERMISSIONS),
            "https://fluxer.example.net/oauth2/authorize?client_id=1234567890&scope=bot&permissions=27648"
        );
    }
}
