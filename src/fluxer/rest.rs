//! Fluxer REST client. Authentication is `Authorization: Bot <token>`
//! (case-sensitive scheme); every path is under `{api}/v1`.

use std::time::Duration;

use rand::Rng;
use serde::Deserialize;
use serde_json::json;

use super::endpoints::Endpoints;
use super::model::{BotApplication, OutgoingMessage};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status} {code}: {message}")]
    Status { status: u16, code: String, message: String },
    #[error("request failed: {0}")]
    Transport(String),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            ApiError::Transport(_) => None,
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Status { code, .. } => Some(code),
            ApiError::Transport(_) => None,
        }
    }

    pub fn is_unauthorized(&self) -> bool {
        self.status() == Some(401)
    }

    /// The bot lacks a permission or cannot see the channel.
    pub fn is_missing_permissions(&self) -> bool {
        self.status() == Some(403)
            && matches!(
                self.code(),
                Some("MISSING_PERMISSIONS") | Some("MISSING_ACCESS") | Some("ACCESS_DENIED")
            )
    }

    /// The recipient's settings stop the bot from DMing them.
    pub fn is_dm_blocked(&self) -> bool {
        matches!(
            self.code(),
            Some("CANNOT_SEND_MESSAGES_TO_USER") | Some("NEW_CONVERSATIONS_LIMITED") | Some("DIRECT_MESSAGES_DISABLED")
        )
    }

    /// Operator-facing explanation for logs.
    pub fn hint(&self) -> &'static str {
        match self {
            ApiError::Transport(_) => "Cannot reach the Fluxer API. Check FLUXER_API_URL / FLUXER_INSTANCE_URL, DNS, and that the bot container is on the Fluxer Docker network.",
            e if e.is_unauthorized() => "The bot token was rejected. Reset it in User Settings > Applications and update FLUXER_BOT_TOKEN.",
            e if e.is_missing_permissions() => "The bot lacks permission in this channel. It needs View Channel, Send Messages and Embed Links (and Manage Messages to delete leaked guesses). Re-invite it with the invite URL from the startup log, or adjust the channel/role permissions.",
            e if e.is_dm_blocked() => "The user's settings prevent the bot from sending them a DM.",
            e if e.status() == Some(404) => "Not found. The channel or user may no longer exist, or FLUXER_API_URL points at the wrong path.",
            e if e.status() == Some(429) => "Rate limited by Fluxer.",
            _ => "Unexpected response from the Fluxer API.",
        }
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    retry_after: Option<f64>,
}

#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    endpoints: Endpoints,
    auth: String,
}

fn random_nonce() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..24).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}

impl RestClient {
    pub fn new(http: reqwest::Client, endpoints: Endpoints, token: &str) -> RestClient {
        RestClient { http, endpoints, auth: format!("Bot {}", token.trim()) }
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(reqwest::StatusCode, String), ApiError> {
        let resp = req
            .header("Authorization", &self.auth)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| ApiError::Transport(e.to_string()))?;
        Ok((status, body))
    }

    fn to_error(status: reqwest::StatusCode, body: &str) -> (ApiError, Option<f64>) {
        let parsed: Option<ErrorBody> = serde_json::from_str(body).ok();
        let retry_after = parsed.as_ref().and_then(|p| p.retry_after);
        let code = parsed
            .as_ref()
            .and_then(|p| p.code.clone())
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("UNKNOWN").to_string());
        let message = parsed.and_then(|p| p.message).unwrap_or_else(|| body.chars().take(200).collect());
        (ApiError::Status { status: status.as_u16(), code, message }, retry_after)
    }

    /// `GET /v1/applications/@me`: succeeds only for a live bot token, which
    /// makes it the right token validity check (`401 INVALID_TOKEN` otherwise).
    pub async fn current_application(&self) -> Result<BotApplication, ApiError> {
        let url = self.endpoints.api_url("applications/@me");
        let (status, body) = self.send(self.http.get(&url)).await?;
        if !status.is_success() {
            return Err(Self::to_error(status, &body).0);
        }
        serde_json::from_str(&body).map_err(|e| ApiError::Transport(format!("unexpected application payload: {e}")))
    }

    /// `POST /v1/users/@me/channels` opens (or returns) the DM with a user.
    pub async fn open_dm(&self, user_id: &str) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Channel {
            id: String,
        }
        let url = self.endpoints.api_url("users/@me/channels");
        let (status, body) = self.send(self.http.post(&url).json(&json!({ "recipient_id": user_id }))).await?;
        if !status.is_success() {
            return Err(Self::to_error(status, &body).0);
        }
        serde_json::from_str::<Channel>(&body)
            .map(|c| c.id)
            .map_err(|e| ApiError::Transport(format!("unexpected channel payload: {e}")))
    }

    /// `POST /v1/channels/{id}/messages`. Retries on 429/5xx/transport errors
    /// reusing one nonce, so Fluxer replays the first result instead of posting twice.
    pub async fn send_message(&self, channel_id: &str, message: &OutgoingMessage) -> Result<(), ApiError> {
        let url = self.endpoints.api_url(&format!("channels/{channel_id}/messages"));
        let mut payload = serde_json::to_value(message).map_err(|e| ApiError::Transport(e.to_string()))?;
        payload["nonce"] = json!(random_nonce());
        payload["allowed_mentions"] = json!({ "parse": [] });

        let mut attempt = 0u32;
        loop {
            let result = self.send(self.http.post(&url).json(&payload)).await;
            let (err, wait) = match result {
                Ok((status, _)) if status.is_success() => return Ok(()),
                Ok((status, body)) => {
                    let (err, retry_after) = Self::to_error(status, &body);
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if !retryable {
                        return Err(err);
                    }
                    let wait = retry_after.map(|s| s.clamp(0.05, 10.0)).unwrap_or(0.5 * 2f64.powi(attempt as i32));
                    (err, wait)
                }
                Err(e) => (e, 0.5 * 2f64.powi(attempt as i32)),
            };
            attempt += 1;
            if attempt >= 4 {
                return Err(err);
            }
            tracing::warn!(%channel_id, attempt, wait_secs = wait, error = %err, "retrying message send");
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    /// `DELETE /v1/channels/{id}/messages/{id}` (needs Manage Messages for others' messages).
    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), ApiError> {
        let url = self.endpoints.api_url(&format!("channels/{channel_id}/messages/{message_id}"));
        let (status, body) = self.send(self.http.delete(&url)).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(Self::to_error(status, &body).0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn classifies_errors() {
        let (e, _) = RestClient::to_error(StatusCode::UNAUTHORIZED, r#"{"code":"INVALID_TOKEN","message":"bad"}"#);
        assert!(e.is_unauthorized());
        assert!(e.hint().contains("FLUXER_BOT_TOKEN"));

        let (e, _) = RestClient::to_error(StatusCode::FORBIDDEN, r#"{"code":"MISSING_PERMISSIONS","message":"x"}"#);
        assert!(e.is_missing_permissions());

        let (e, _) =
            RestClient::to_error(StatusCode::BAD_REQUEST, r#"{"code":"CANNOT_SEND_MESSAGES_TO_USER","message":"x"}"#);
        assert!(e.is_dm_blocked());
    }

    #[test]
    fn parses_rate_limit_body() {
        let (e, retry) = RestClient::to_error(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"code":"RATE_LIMITED","message":"slow","global":false,"retry_after":0.428}"#,
        );
        assert_eq!(e.status(), Some(429));
        assert_eq!(retry, Some(0.428));
    }

    #[test]
    fn tolerates_non_json_error_bodies() {
        let (e, _) = RestClient::to_error(StatusCode::BAD_GATEWAY, "<html>bad gateway</html>");
        assert_eq!(e.status(), Some(502));
        assert!(e.to_string().contains("502"));
    }

    #[test]
    fn nonce_fits_fluxer_limit() {
        let n = random_nonce();
        assert!((1..=32).contains(&n.len()));
    }
}
