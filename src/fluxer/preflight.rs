//! Startup connectivity test: discovery -> REST token check -> gateway handshake.
//! Each step logs what it verified, and each failure logs what to fix.

use std::time::Duration;

use super::endpoints::{self, EndpointConfig, Endpoints, Source, BOT_PERMISSIONS};
use super::gateway;
use super::model::BotApplication;
use super::rest::{ApiError, RestClient};
use crate::error::Error;

#[derive(Debug)]
pub struct PreflightReport {
    pub endpoints: Endpoints,
    pub application: BotApplication,
    pub guild_count: usize,
    pub invite_url: String,
}

#[derive(Debug)]
pub struct PreflightFailure {
    /// Retrying will not help (bad token, malformed URL, sharding...).
    pub fatal: bool,
    pub summary: String,
}

impl std::fmt::Display for PreflightFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

fn source_label(s: Source) -> &'static str {
    match s {
        Source::Override => "configured",
        Source::Discovery => "discovered",
    }
}

pub async fn run(
    http: &reqwest::Client,
    cfg: &EndpointConfig,
    token: &str,
) -> Result<PreflightReport, PreflightFailure> {
    // 1. Endpoints
    tracing::info!(instance = %cfg.instance_url, "[1/3] resolving Fluxer endpoints");
    let endpoints = endpoints::discover(http, cfg).await.map_err(|e| {
        let fatal = !matches!(&e, Error::Config(m) if m.contains("could not reach") || m.contains("HTTP 5"));
        tracing::error!(
            "cannot resolve Fluxer endpoints: {e}. Check FLUXER_INSTANCE_URL, or set FLUXER_API_URL and FLUXER_GATEWAY_URL explicitly."
        );
        PreflightFailure { fatal, summary: e.to_string() }
    })?;
    tracing::info!(
        api = %endpoints.api_base,
        api_source = source_label(endpoints.api_source),
        gateway = %endpoints.gateway_url,
        gateway_source = source_label(endpoints.gateway_source),
        "endpoints resolved"
    );

    // 2. REST + token
    tracing::info!("[2/3] checking the bot token over REST (GET /v1/applications/@me)");
    let rest = RestClient::new(http.clone(), endpoints.clone(), token);
    let application = rest.current_application().await.map_err(|e| {
        let fatal = e.is_unauthorized() || matches!(&e, ApiError::Status { status, .. } if *status < 500 && *status != 429);
        tracing::error!(error = %e, "REST check failed. {}", e.hint());
        PreflightFailure { fatal, summary: format!("REST check failed: {e}") }
    })?;
    tracing::info!(
        application = %application.name,
        application_id = %application.id,
        bot_public = application.bot_public,
        "bot token accepted"
    );

    // 3. Gateway
    tracing::info!("[3/3] testing the gateway handshake (Hello -> Identify -> Ready)");
    let ready = gateway::probe(&endpoints.gateway_url, token, Duration::from_secs(20))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "gateway check failed. {}", e.hint());
            PreflightFailure { fatal: e.is_fatal(), summary: format!("gateway check failed: {e}") }
        })?;
    let guild_count = ready.guilds.len();
    tracing::info!(
        bot_user = %ready.user.username,
        bot_user_id = %ready.user.id,
        guilds = guild_count,
        "gateway handshake succeeded"
    );

    let invite_url = endpoints.invite_url(&application.id, BOT_PERMISSIONS);
    if guild_count == 0 {
        tracing::warn!(
            "the bot is not in any community yet. Open this URL as a user with Manage Community permission to add it: {invite_url}"
        );
    } else {
        tracing::info!("invite URL for additional communities: {invite_url}");
    }

    Ok(PreflightReport { endpoints, application, guild_count, invite_url })
}
