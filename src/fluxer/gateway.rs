//! Fluxer Gateway client (JSON over WebSocket, protocol version 1, no intents).
//!
//! Flow per the Fluxer docs: connect -> Hello(op 10) -> Identify(op 2, raw token
//! with no `Bot ` prefix) -> Ready -> Heartbeat(op 1) every `heartbeat_interval`.
//! A dropped connection is resumed (op 6) while the server still retains the
//! session (60 s); otherwise the bot identifies again.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use super::model::{GuildStub, Message, Ready};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Events the bot does not need; Fluxer drops them server-side (Identify `ignored_events`).
const IGNORED_EVENTS: &[&str] = &[
    "TYPING_START",
    "PRESENCE_UPDATE",
    "MESSAGE_UPDATE",
    "MESSAGE_DELETE",
    "MESSAGE_DELETE_BULK",
    "MESSAGE_REACTION_ADD",
    "MESSAGE_REACTION_REMOVE",
    "VOICE_STATE_UPDATE",
    "CHANNEL_PINS_UPDATE",
];

#[derive(Debug)]
pub enum GatewayEvent {
    Ready(Ready),
    GuildCreate(GuildStub),
    MessageCreate(Box<Message>),
    Resumed,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("could not open the gateway connection: {0}")]
    Connect(String),
    #[error("gateway closed the connection with code {code} ({reason})")]
    Closed { code: u16, reason: String },
    #[error("gateway protocol error: {0}")]
    Protocol(String),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
}

impl GatewayError {
    /// Retrying cannot fix these; the operator has to change something.
    pub fn is_fatal(&self) -> bool {
        matches!(self, GatewayError::Closed { code: 4004 | 4010 | 4011 | 4012, .. })
    }

    fn resets_session(&self) -> bool {
        matches!(self, GatewayError::Closed { code: 4003 | 4004 | 4007, .. })
    }

    /// Operator-facing explanation for logs.
    pub fn hint(&self) -> String {
        match self {
            GatewayError::Closed { code: 4004, .. } => "Authentication failed: the bot token is invalid or was reset. Copy the current token from User Settings > Applications > your app > Bot, and set FLUXER_BOT_TOKEN (the gateway wants the raw token, the bot adds the `Bot ` prefix itself for REST).".into(),
            GatewayError::Closed { code: 4008, .. } => "Gateway rate limit hit (too many connections or payloads). The bot will back off and retry.".into(),
            GatewayError::Closed { code: 4010, .. } => "Invalid shard value (internal error).".into(),
            GatewayError::Closed { code: 4011, .. } => "Sharding required: the bot is in more than 2,500 guilds.".into(),
            GatewayError::Closed { code: 4012, .. } => "Invalid API version: the gateway URL must carry v=1. Check FLUXER_GATEWAY_URL.".into(),
            GatewayError::Closed { code: 4009, .. } => "Heartbeat timeout; the connection was lost silently. Reconnecting.".into(),
            GatewayError::Closed { .. } => "The gateway ended the session; the bot will reconnect.".into(),
            GatewayError::Connect(m) if m.contains("HTTP 404") || m.contains("HTTP 400") => "The gateway path is wrong. With the stock Fluxer edge proxy use wss://<host>/gateway (public) or ws://gateway:8080/ (inside the Fluxer Docker network). Check FLUXER_GATEWAY_URL.".into(),
            GatewayError::Connect(m) if m.contains("HTTP 502") || m.contains("HTTP 503") || m.contains("HTTP 504") => "The reverse proxy answered but cannot reach the Fluxer gateway container. Check `docker compose ps gateway` on the Fluxer stack and your proxy's WebSocket support.".into(),
            GatewayError::Connect(_) => "Could not open a WebSocket to the gateway. Check FLUXER_GATEWAY_URL / FLUXER_INSTANCE_URL, DNS, TLS certificates and that the proxy forwards WebSocket upgrades.".into(),
            GatewayError::Timeout(_) | GatewayError::Protocol(_) => "Unexpected gateway behaviour; the bot will reconnect.".into(),
        }
    }
}

#[derive(Deserialize)]
struct Payload {
    op: u8,
    #[serde(default)]
    d: Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

fn identify_payload(token: &str) -> String {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "properties": { "os": std::env::consts::OS, "browser": "wordle-bot", "device": "wordle-bot" },
            "ignored_events": IGNORED_EVENTS,
        }
    })
    .to_string()
}

fn resume_payload(token: &str, session_id: &str, seq: u64) -> String {
    json!({ "op": 6, "d": { "token": token, "session_id": session_id, "seq": seq } }).to_string()
}

fn heartbeat_payload(seq: Option<u64>) -> String {
    json!({ "op": 1, "d": seq }).to_string()
}

fn describe_connect_error(e: &tungstenite::Error) -> String {
    match e {
        tungstenite::Error::Http(resp) => format!("server answered HTTP {} instead of upgrading to WebSocket", resp.status().as_u16()),
        other => other.to_string(),
    }
}

fn close_error(frame: Option<tungstenite::protocol::CloseFrame>) -> GatewayError {
    match frame {
        Some(f) => GatewayError::Closed { code: u16::from(f.code), reason: f.reason.to_string() },
        None => GatewayError::Closed { code: 1005, reason: "no close frame".into() },
    }
}

/// Open the socket and wait for Hello. Returns the heartbeat interval.
async fn open(url: &str) -> Result<(Ws, Duration), GatewayError> {
    let (mut ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(url))
        .await
        .map_err(|_| GatewayError::Connect("connection attempt timed out".into()))?
        .map_err(|e| GatewayError::Connect(describe_connect_error(&e)))?;

    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(t))) => {
                    let p: Payload = serde_json::from_str(t.as_str())
                        .map_err(|e| GatewayError::Protocol(format!("invalid Hello payload: {e}")))?;
                    if p.op == 10 {
                        let ms = p.d.get("heartbeat_interval").and_then(Value::as_u64).unwrap_or(41_250);
                        return Ok(Duration::from_millis(ms));
                    }
                    return Err(GatewayError::Protocol(format!("expected Hello (op 10), got op {}", p.op)));
                }
                Some(Ok(WsMessage::Close(frame))) => return Err(close_error(frame)),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(GatewayError::Connect(e.to_string())),
                None => {
                    return Err(GatewayError::Connect(
                        "connection closed before Hello (the gateway may have hit its concurrent connection limit)".into(),
                    ))
                }
            }
        }
    })
    .await
    .map_err(|_| GatewayError::Timeout("Hello"))??;

    Ok((ws, hello))
}

/// One-shot check used at startup: connect, Identify, wait for Ready, disconnect.
pub async fn probe(url: &str, token: &str, timeout: Duration) -> Result<Ready, GatewayError> {
    let (mut ws, _) = open(url).await?;
    ws.send(WsMessage::text(identify_payload(token)))
        .await
        .map_err(|e| GatewayError::Connect(e.to_string()))?;

    let result = tokio::time::timeout(timeout, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(t))) => {
                    let Ok(p) = serde_json::from_str::<Payload>(t.as_str()) else { continue };
                    if p.op == 0 && p.t.as_deref() == Some("READY") {
                        return serde_json::from_value::<Ready>(p.d)
                            .map_err(|e| GatewayError::Protocol(format!("invalid READY payload: {e}")));
                    }
                }
                Some(Ok(WsMessage::Close(frame))) => return Err(close_error(frame)),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(GatewayError::Connect(e.to_string())),
                None => return Err(GatewayError::Connect("connection dropped while waiting for Ready".into())),
            }
        }
    })
    .await
    .map_err(|_| GatewayError::Timeout("Ready"))?;

    let _ = ws.close(None).await;
    result
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    seq: Option<u64>,
}

enum ConnEnd {
    Shutdown,
    Reconnect { reset_session: bool },
}

pub struct GatewayConfig {
    pub url: String,
    pub token: String,
}

/// Run until shutdown or a fatal error, reconnecting with backoff.
pub async fn run(
    cfg: GatewayConfig,
    tx: mpsc::Sender<GatewayEvent>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), GatewayError> {
    let mut session = Session::default();
    let mut backoff = 1u64;

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let mut reached_ready = false;
        match connection(&cfg, &mut session, &tx, &mut shutdown, &mut reached_ready).await {
            Ok(ConnEnd::Shutdown) => return Ok(()),
            Ok(ConnEnd::Reconnect { reset_session }) => {
                if reset_session {
                    session = Session::default();
                }
                if reached_ready {
                    backoff = 1;
                }
                let wait = Duration::from_millis(rand::thread_rng().gen_range(1000..5000));
                tracing::info!(?wait, "reconnecting to the gateway");
                if sleep_or_shutdown(wait, &mut shutdown).await {
                    return Ok(());
                }
            }
            Err(e) => {
                if e.resets_session() {
                    session = Session::default();
                }
                if e.is_fatal() {
                    tracing::error!(error = %e, "{}", e.hint());
                    return Err(e);
                }
                if reached_ready {
                    backoff = 1;
                }
                tracing::warn!(error = %e, retry_in_secs = backoff, "{}", e.hint());
                if sleep_or_shutdown(Duration::from_secs(backoff), &mut shutdown).await {
                    return Ok(());
                }
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// Returns true if shutdown was requested while waiting.
async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = shutdown.changed() => true,
    }
}

async fn connection(
    cfg: &GatewayConfig,
    session: &mut Session,
    tx: &mpsc::Sender<GatewayEvent>,
    shutdown: &mut watch::Receiver<bool>,
    reached_ready: &mut bool,
) -> Result<ConnEnd, GatewayError> {
    let (ws, interval) = open(&cfg.url).await?;
    let (mut write, mut read) = ws.split();

    let first = match (&session.id, session.seq) {
        (Some(id), Some(seq)) => {
            tracing::info!(session_id = %id, seq, "resuming gateway session");
            resume_payload(&cfg.token, id, seq)
        }
        _ => {
            session.id = None;
            session.seq = None;
            tracing::info!("identifying with the gateway");
            identify_payload(&cfg.token)
        }
    };
    write
        .send(WsMessage::text(first))
        .await
        .map_err(|e| GatewayError::Connect(e.to_string()))?;

    let jitter = rand::thread_rng().gen_range(0.0..1.0);
    let mut ticker = tokio::time::interval_at(Instant::now() + interval.mul_f64(jitter), interval);
    let mut awaiting_ack = false;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let _ = write.send(WsMessage::Close(None)).await;
                return Ok(ConnEnd::Shutdown);
            }
            _ = ticker.tick() => {
                if awaiting_ack {
                    tracing::warn!("no heartbeat ACK received; the connection looks dead");
                    return Ok(ConnEnd::Reconnect { reset_session: false });
                }
                write.send(WsMessage::text(heartbeat_payload(session.seq)))
                    .await
                    .map_err(|e| GatewayError::Connect(e.to_string()))?;
                awaiting_ack = true;
            }
            msg = read.next() => {
                let msg = match msg {
                    None => return Err(GatewayError::Closed { code: 1006, reason: "connection dropped".into() }),
                    Some(Err(e)) => return Err(GatewayError::Connect(e.to_string())),
                    Some(Ok(m)) => m,
                };
                match msg {
                    WsMessage::Text(text) => {
                        let payload: Payload = match serde_json::from_str(text.as_str()) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!(error = %e, "ignoring malformed gateway payload");
                                continue;
                            }
                        };
                        match payload.op {
                            0 => {
                                if let Some(s) = payload.s {
                                    session.seq = Some(s);
                                }
                                if !dispatch(payload, session, tx, reached_ready).await {
                                    return Ok(ConnEnd::Shutdown);
                                }
                            }
                            1 => {
                                write.send(WsMessage::text(heartbeat_payload(session.seq)))
                                    .await
                                    .map_err(|e| GatewayError::Connect(e.to_string()))?;
                            }
                            7 => return Ok(ConnEnd::Reconnect { reset_session: false }),
                            9 => {
                                let resumable = payload.d.as_bool().unwrap_or(false);
                                tracing::info!(resumable, "gateway invalidated the session");
                                return Ok(ConnEnd::Reconnect { reset_session: !resumable });
                            }
                            11 => awaiting_ack = false,
                            _ => {}
                        }
                    }
                    WsMessage::Close(frame) => return Err(close_error(frame)),
                    WsMessage::Ping(p) => {
                        let _ = write.send(WsMessage::Pong(p)).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Returns false when the receiving side is gone and the loop should stop.
async fn dispatch(
    payload: Payload,
    session: &mut Session,
    tx: &mpsc::Sender<GatewayEvent>,
    reached_ready: &mut bool,
) -> bool {
    let Some(name) = payload.t.as_deref() else { return true };
    let event = match name {
        "READY" => match serde_json::from_value::<Ready>(payload.d) {
            Ok(r) => {
                session.id = Some(r.session_id.clone());
                *reached_ready = true;
                GatewayEvent::Ready(r)
            }
            Err(e) => {
                tracing::error!(error = %e, "could not parse READY");
                return true;
            }
        },
        "RESUMED" => {
            *reached_ready = true;
            GatewayEvent::Resumed
        }
        "GUILD_CREATE" => match serde_json::from_value::<GuildStub>(payload.d) {
            Ok(g) => GatewayEvent::GuildCreate(g),
            Err(_) => return true,
        },
        "MESSAGE_CREATE" => match serde_json::from_value::<Message>(payload.d) {
            Ok(m) => GatewayEvent::MessageCreate(Box::new(m)),
            Err(e) => {
                tracing::warn!(error = %e, "could not parse MESSAGE_CREATE");
                return true;
            }
        },
        _ => return true,
    };
    tx.send(event).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_has_raw_token_and_required_properties() {
        let v: Value = serde_json::from_str(&identify_payload("123.secret")).unwrap();
        assert_eq!(v["op"], 2);
        assert_eq!(v["d"]["token"], "123.secret");
        for k in ["os", "browser", "device"] {
            assert!(v["d"]["properties"][k].is_string(), "{k} must be a string");
        }
        assert!(v["d"].get("intents").is_none());
    }

    #[test]
    fn resume_and_heartbeat_payloads() {
        let v: Value = serde_json::from_str(&resume_payload("t", "sess", 42)).unwrap();
        assert_eq!((v["op"].as_u64(), v["d"]["seq"].as_u64()), (Some(6), Some(42)));
        assert_eq!(heartbeat_payload(None), r#"{"d":null,"op":1}"#);
        assert_eq!(heartbeat_payload(Some(7)), r#"{"d":7,"op":1}"#);
    }

    #[test]
    fn close_code_classification() {
        let closed = |code| GatewayError::Closed { code, reason: String::new() };
        assert!(closed(4004).is_fatal());
        assert!(closed(4011).is_fatal());
        assert!(closed(4012).is_fatal());
        assert!(!closed(4000).is_fatal());
        assert!(!closed(4009).is_fatal());
        assert!(closed(4007).resets_session());
        assert!(!closed(4009).resets_session());
        assert!(closed(4004).hint().contains("token"));
    }

    #[test]
    fn connect_hints() {
        let e = GatewayError::Connect("server answered HTTP 404 instead of upgrading to WebSocket".into());
        assert!(e.hint().contains("FLUXER_GATEWAY_URL"));
        let e = GatewayError::Connect("server answered HTTP 502 instead of upgrading to WebSocket".into());
        assert!(e.hint().contains("proxy"));
    }
}
