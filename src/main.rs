use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::sync::{mpsc, watch, Semaphore};
use tracing_subscriber::EnvFilter;

use wordle_bot::bot::{Bot, BotOptions};
use wordle_bot::config::Config;
use wordle_bot::fluxer::gateway::{self, GatewayConfig};
use wordle_bot::fluxer::preflight::{self, PreflightReport};
use wordle_bot::fluxer::rest::RestClient;
use wordle_bot::game::Dictionary;
use wordle_bot::puzzle::{prefetch_loop, Clock, NytProvider, PuzzleService, SystemClock};
use wordle_bot::service::GameService;
use wordle_bot::{DEFAULT_WORDS, MIGRATOR};

const STARTUP_ATTEMPTS: u32 = 6;
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        // Plain text when running under Docker (no TTY), colours in a terminal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")))
        .init();
    // reqwest, tokio-tungstenite and sqlx may each pull in a rustls backend; pick one explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let check_only = std::env::args().any(|a| a == "--check");

    match run(check_only).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(check_only: bool) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    tracing::info!(?config, "starting wordle-bot");

    let http = reqwest::Client::builder()
        .user_agent(concat!("wordle-fluxer-bot/", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Fluxer connectivity test. Transient failures (Fluxer still booting) are retried;
    // a bad token or malformed URL stops immediately with a clear message.
    let attempts = if check_only { 1 } else { STARTUP_ATTEMPTS };
    let mut report: Option<PreflightReport> = None;
    for attempt in 1..=attempts {
        match preflight::run(&http, &config.endpoints, &config.bot_token).await {
            Ok(r) => {
                report = Some(r);
                break;
            }
            Err(f) if f.fatal => anyhow::bail!("startup check failed: {f}"),
            Err(f) if attempt == attempts => anyhow::bail!("startup check failed after {attempts} attempt(s): {f}"),
            Err(f) => {
                tracing::warn!(attempt, retry_in = ?STARTUP_RETRY_DELAY, "startup check failed ({f}); retrying");
                tokio::time::sleep(STARTUP_RETRY_DELAY).await;
            }
        }
    }
    let report = report.expect("loop either sets the report or returns");

    // Database
    tracing::info!("connecting to PostgreSQL");
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot connect to PostgreSQL: {e}. Check DATABASE_URL, that the database and role exist (scripts/create-database.sql), and that this container shares a Docker network with the postgres service."
            )
        })?;
    MIGRATOR.run(&pool).await.map_err(|e| anyhow::anyhow!("database migration failed: {e}"))?;
    tracing::info!("database ready");

    if check_only {
        tracing::info!("all startup checks passed");
        return Ok(());
    }

    let dictionary = match &config.words_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read WORDS_FILE {}: {e}", path.display()))?;
            Dictionary::from_text(&text)
        }
        None => Dictionary::from_text(DEFAULT_WORDS),
    };
    anyhow::ensure!(!dictionary.is_empty(), "the word dictionary is empty");
    tracing::info!(words = dictionary.len(), "dictionary loaded");

    let provider = Arc::new(NytProvider::new(config.nyt_base_url.clone())?);
    let puzzles = Arc::new(PuzzleService::new(pool.clone(), provider));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let prefetch_task = tokio::spawn(prefetch_loop(puzzles.clone(), clock.clone(), config.timezone));
    let clock_for_recaps = clock.clone();
    let service = Arc::new(GameService::new(
        pool.clone(),
        puzzles,
        Arc::new(dictionary),
        clock,
        config.timezone,
    ));
    let rest = RestClient::new(http, report.endpoints.clone(), &config.bot_token);
    let app_owner_id = report.application.owner.as_ref().and_then(|o| o.id_i64());
    let bot = Arc::new(Bot::new(
        rest,
        service,
        BotOptions {
            prefix: config.command_prefix.clone(),
            leaderboard_size: config.leaderboard_size,
            compact_results: config.compact_results,
            app_owner_id,
        },
    ));
    // Daily recap: at startup (catches up after downtime) and shortly after each rollover.
    let recap_task = {
        let bot = bot.clone();
        let clock = clock_for_recaps.clone();
        let tz = config.timezone;
        tokio::spawn(async move {
            loop {
                bot.run_recaps().await;
                let wait = wordle_bot::puzzle::until_next_rollover(clock.now(), tz) + Duration::from_secs(30);
                tokio::time::sleep(wait).await;
            }
        })
    };

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let gateway_task = tokio::spawn(gateway::run(
        GatewayConfig { url: report.endpoints.gateway_url.clone(), token: config.bot_token.clone() },
        event_tx,
        shutdown_rx,
    ));

    let limiter = Arc::new(Semaphore::new(32));
    let mut shutdown_signal = Box::pin(shutdown_signal());
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                let permit = limiter.clone().acquire_owned().await?;
                let bot = bot.clone();
                tokio::spawn(async move {
                    bot.handle_event(event).await;
                    drop(permit);
                });
            }
            _ = &mut shutdown_signal => {
                tracing::info!("shutdown requested");
                let _ = shutdown_tx.send(true);
                break;
            }
        }
    }

    match tokio::time::timeout(Duration::from_secs(10), gateway_task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => anyhow::bail!("gateway stopped: {e}"),
        Ok(Err(e)) => anyhow::bail!("gateway task crashed: {e}"),
        Err(_) => tracing::warn!("gateway task did not stop in time"),
    }
    prefetch_task.abort();
    recap_task.abort();
    pool.close().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
