//! Daily puzzle sourcing. The game only depends on [`WordProvider`]; the NYT
//! implementation is one replaceable adapter.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::game::WORD_LEN;

/// Wordle #0 was published on this date.
pub const WORDLE_EPOCH: (i32, u32, u32) = (2021, 6, 19);

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// The puzzle date at `now`: the calendar date in the puzzle timezone, so it
/// rolls over at local midnight (DST-aware).
pub fn puzzle_date(now: DateTime<Utc>, tz: Tz) -> NaiveDate {
    now.with_timezone(&tz).date_naive()
}

/// Time from `now` until the next puzzle rollover (local midnight in `tz`).
/// DST-safe: if midnight does not exist locally, the first valid instant after it is used.
pub fn until_next_rollover(now: DateTime<Utc>, tz: Tz) -> Duration {
    use chrono::{Days, TimeZone};
    let tomorrow = puzzle_date(now, tz).checked_add_days(Days::new(1)).expect("date in range");
    let mut candidate = tomorrow.and_hms_opt(0, 0, 0).expect("midnight");
    let next = loop {
        match tz.from_local_datetime(&candidate).earliest() {
            Some(t) => break t.with_timezone(&Utc),
            None => candidate += chrono::Duration::minutes(30),
        }
    };
    (next - now).to_std().unwrap_or(Duration::ZERO)
}

/// Background task: fetch today's puzzle at startup and again just after every
/// rollover, so the cache is warm and upstream problems are logged early.
pub async fn prefetch_loop(puzzles: Arc<PuzzleService>, clock: Arc<dyn Clock>, tz: Tz) {
    loop {
        let today = puzzle_date(clock.now(), tz);
        let mut wait = Duration::from_secs(300);
        match puzzles.get_or_fetch(today, today).await {
            Ok(p) => {
                tracing::info!(date = %p.puzzle_date, number = p.puzzle_number, source = %p.source, "daily puzzle cached");
                wait = until_next_rollover(clock.now(), tz) + Duration::from_secs(5);
            }
            Err(e) => tracing::warn!(%today, error = %e, "could not prefetch today's puzzle; retrying in 5 minutes"),
        }
        tokio::time::sleep(wait).await;
    }
}

pub fn puzzle_number_for(date: NaiveDate) -> i32 {
    let (y, m, d) = WORDLE_EPOCH;
    let epoch = NaiveDate::from_ymd_opt(y, m, d).expect("valid epoch");
    (date - epoch).num_days() as i32
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderPuzzle {
    pub number: Option<i32>,
    pub solution: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("puzzle not available for {0}")]
    NotAvailable(NaiveDate),
    #[error("unexpected response: {0}")]
    BadResponse(String),
    #[error("request failed: {0}")]
    Transport(String),
}

#[async_trait]
pub trait WordProvider: Send + Sync {
    /// Short identifier stored with each cached puzzle (e.g. `nyt`).
    fn name(&self) -> &'static str;
    async fn fetch(&self, date: NaiveDate) -> std::result::Result<ProviderPuzzle, ProviderError>;
}

pub struct NytProvider {
    client: reqwest::Client,
    base_url: String,
}

#[derive(Deserialize)]
struct NytResponse {
    solution: Option<String>,
    print_date: Option<String>,
    days_since_launch: Option<i32>,
}

/// Validate and convert the NYT JSON body. Separated from I/O for testing.
pub fn parse_nyt_response(body: &str, date: NaiveDate) -> std::result::Result<ProviderPuzzle, ProviderError> {
    let parsed: NytResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::BadResponse(format!("invalid JSON: {e}")))?;

    let solution = parsed
        .solution
        .ok_or_else(|| ProviderError::BadResponse("missing `solution`".into()))?
        .trim()
        .to_ascii_lowercase();
    if solution.len() != WORD_LEN || !solution.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(ProviderError::BadResponse(format!(
            "solution is not a {WORD_LEN}-letter word"
        )));
    }

    if let Some(printed) = parsed.print_date {
        if printed != date.to_string() {
            return Err(ProviderError::BadResponse(format!(
                "response is for {printed}, expected {date}"
            )));
        }
    }

    Ok(ProviderPuzzle {
        number: parsed.days_since_launch.filter(|n| *n >= 0),
        solution,
    })
}

impl NytProvider {
    pub fn new(base_url: impl Into<String>) -> Result<NytProvider> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("wordle-fluxer-bot/0.1")
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;
        Ok(NytProvider {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
    }
}

#[async_trait]
impl WordProvider for NytProvider {
    fn name(&self) -> &'static str {
        "nyt"
    }

    async fn fetch(&self, date: NaiveDate) -> std::result::Result<ProviderPuzzle, ProviderError> {
        let url = format!("{}/{}.json", self.base_url, date);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ProviderError::NotAvailable(date));
        }
        if !status.is_success() {
            return Err(ProviderError::Transport(format!("HTTP {status}")));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        parse_nyt_response(&body, date)
    }
}

/// The bot's own daily word, independent of the NYT, so it can't be looked up.
///
/// The answer list is put in a fixed secret order (sorted by SHA-256 of
/// `secret:word`) and day N of the Wordle calendar uses entry N (wrapping
/// around). Without the secret the order can't be predicted, every word is used
/// once before any repeats, and the same date always gives the same word.
pub struct LocalProvider {
    order: Vec<String>,
}

impl LocalProvider {
    pub fn new(answers: Vec<String>, secret: &str) -> Result<LocalProvider> {
        use sha2::{Digest, Sha256};
        if answers.is_empty() {
            return Err(Error::Config("the answer list is empty".into()));
        }
        let mut keyed: Vec<([u8; 32], String)> = answers
            .into_iter()
            .map(|w| {
                let mut h = Sha256::new();
                h.update(secret.as_bytes());
                h.update(b":");
                h.update(w.as_bytes());
                (h.finalize().into(), w)
            })
            .collect();
        keyed.sort();
        keyed.dedup_by(|a, b| a.1 == b.1);
        Ok(LocalProvider { order: keyed.into_iter().map(|(_, w)| w).collect() })
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn word_for(&self, date: NaiveDate) -> &str {
        let day = puzzle_number_for(date) as i64;
        &self.order[day.rem_euclid(self.order.len() as i64) as usize]
    }
}

/// Parse an answer list: one word per line, `#` comments ignored. Only words the
/// guess dictionary accepts are kept, so every answer can actually be typed.
pub fn parse_answers(text: &str, dictionary: &crate::game::Dictionary) -> Vec<String> {
    text.lines()
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|w| w.len() == WORD_LEN && w.bytes().all(|b| b.is_ascii_lowercase()) && dictionary.contains(w))
        .collect()
}

#[async_trait]
impl WordProvider for LocalProvider {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn fetch(&self, date: NaiveDate) -> std::result::Result<ProviderPuzzle, ProviderError> {
        Ok(ProviderPuzzle { number: None, solution: self.word_for(date).to_string() })
    }
}

/// The secret that orders the local answer list: `WORDLE_SECRET` if set,
/// otherwise generated once and kept in the database so it survives restarts.
pub async fn load_or_create_secret(pool: &PgPool, configured: Option<&str>) -> Result<String> {
    if let Some(s) = configured {
        return Ok(s.to_string());
    }
    let generated: String = {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..32).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()
    };
    sqlx::query("INSERT INTO wordle_bot_settings (key, value) VALUES ('word_secret', $1) ON CONFLICT (key) DO NOTHING")
        .bind(&generated)
        .execute(pool)
        .await?;
    let (secret,): (String,) = sqlx::query_as("SELECT value FROM wordle_bot_settings WHERE key = 'word_secret'")
        .fetch_one(pool)
        .await?;
    Ok(secret)
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Puzzle {
    pub puzzle_date: NaiveDate,
    pub puzzle_number: i32,
    pub solution: String,
    pub source: String,
}

/// Cache-through access to daily puzzles backed by PostgreSQL.
pub struct PuzzleService {
    pool: PgPool,
    provider: Arc<dyn WordProvider>,
    fetch_lock: Mutex<()>,
}

impl PuzzleService {
    pub fn new(pool: PgPool, provider: Arc<dyn WordProvider>) -> PuzzleService {
        PuzzleService {
            pool,
            provider,
            fetch_lock: Mutex::new(()),
        }
    }

    pub async fn cached(&self, date: NaiveDate) -> Result<Option<Puzzle>> {
        let row = sqlx::query_as::<_, Puzzle>(
            "SELECT puzzle_date, puzzle_number, solution, source FROM wordle_daily_puzzles WHERE puzzle_date = $1",
        )
        .bind(date)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Returns the puzzle for `date`, fetching and caching it on first use.
    /// `today` guards against ever fetching a future puzzle.
    pub async fn get_or_fetch(&self, date: NaiveDate, today: NaiveDate) -> Result<Puzzle> {
        if let Some(p) = self.cached(date).await? {
            return Ok(p);
        }
        if date > today {
            return Err(Error::PuzzleUnavailable(format!("{date} is in the future")));
        }

        // Serialise cache misses so a burst of players triggers one upstream call.
        let _guard = self.fetch_lock.lock().await;
        if let Some(p) = self.cached(date).await? {
            return Ok(p);
        }

        let mut last_err = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt))).await;
            }
            match self.provider.fetch(date).await {
                Ok(fetched) => {
                    let number = fetched.number.unwrap_or_else(|| puzzle_number_for(date));
                    sqlx::query(
                        "INSERT INTO wordle_daily_puzzles (puzzle_date, puzzle_number, solution, source)
                         VALUES ($1, $2, $3, $4) ON CONFLICT (puzzle_date) DO NOTHING",
                    )
                    .bind(date)
                    .bind(number)
                    .bind(&fetched.solution)
                    .bind(self.provider.name())
                    .execute(&self.pool)
                    .await?;
                    return self
                        .cached(date)
                        .await?
                        .ok_or_else(|| Error::PuzzleUnavailable("puzzle vanished after insert".into()));
                }
                // Not retrying a definitive "not published" answer.
                Err(e @ ProviderError::NotAvailable(_)) => {
                    tracing::warn!(%date, error = %e, "puzzle not available from provider");
                    return Err(Error::PuzzleUnavailable(e.to_string()));
                }
                Err(e) => {
                    tracing::warn!(%date, attempt, error = %e, "puzzle fetch failed");
                    last_err = Some(e);
                }
            }
        }
        Err(Error::PuzzleUnavailable(
            last_err.map(|e| e.to_string()).unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn parses_valid_nyt_body() {
        let body = r#"{"id":2370,"solution":"Cigar","print_date":"2024-03-15","days_since_launch":1000,"editor":"x"}"#;
        let p = parse_nyt_response(body, date(2024, 3, 15)).unwrap();
        assert_eq!(p, ProviderPuzzle { number: Some(1000), solution: "cigar".into() });
    }

    #[test]
    fn rejects_bad_bodies() {
        let d = date(2024, 3, 15);
        assert!(matches!(parse_nyt_response("not json", d), Err(ProviderError::BadResponse(_))));
        assert!(matches!(parse_nyt_response("{}", d), Err(ProviderError::BadResponse(_))));
        assert!(matches!(
            parse_nyt_response(r#"{"solution":"toolong"}"#, d),
            Err(ProviderError::BadResponse(_))
        ));
        assert!(matches!(
            parse_nyt_response(r#"{"solution":"ab1de"}"#, d),
            Err(ProviderError::BadResponse(_))
        ));
        assert!(matches!(
            parse_nyt_response(r#"{"solution":"crane","print_date":"2024-03-16"}"#, d),
            Err(ProviderError::BadResponse(_))
        ));
    }

    #[test]
    fn missing_number_is_allowed() {
        let p = parse_nyt_response(r#"{"solution":"crane"}"#, date(2024, 1, 1)).unwrap();
        assert_eq!(p.number, None);
    }

    #[test]
    fn puzzle_numbers_match_known_dates() {
        assert_eq!(puzzle_number_for(date(2021, 6, 19)), 0);
        assert_eq!(puzzle_number_for(date(2024, 3, 15)), 1000);
    }

    #[test]
    fn rolls_over_at_new_york_midnight() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // 03:59 UTC on Mar 16 is 23:59 EDT (UTC-4) on Mar 15 (DST began Mar 10, 2024)
        let before = Utc.with_ymd_and_hms(2024, 3, 16, 3, 59, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2024, 3, 16, 4, 0, 0).unwrap();
        assert_eq!(puzzle_date(before, tz), date(2024, 3, 15));
        assert_eq!(puzzle_date(after, tz), date(2024, 3, 16));
    }

    fn local(words: &[&str], secret: &str) -> LocalProvider {
        LocalProvider::new(words.iter().map(|w| w.to_string()).collect(), secret).unwrap()
    }

    #[test]
    fn local_provider_is_deterministic_and_uses_every_word_once_per_cycle() {
        let words = ["crane", "slate", "cigar", "rebut", "sissy", "humph", "awake"];
        let p = local(&words, "s3cret");
        let start = date(2026, 11, 3);
        let cycle: Vec<String> =
            (0..7).map(|i| p.word_for(start + chrono::Days::new(i)).to_string()).collect();
        let mut sorted = cycle.clone();
        sorted.sort();
        let mut expected: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        expected.sort();
        assert_eq!(sorted, expected, "each word exactly once per cycle");
        // Same date, same word, even from a fresh provider; the cycle then repeats.
        assert_eq!(local(&words, "s3cret").word_for(start), p.word_for(start));
        assert_eq!(p.word_for(start + chrono::Days::new(7)), p.word_for(start));
    }

    #[test]
    fn local_provider_order_depends_on_the_secret() {
        let words: Vec<String> = (0..200).map(|i| format!("w{i:04}")).collect();
        let a = LocalProvider::new(words.clone(), "one").unwrap();
        let b = LocalProvider::new(words, "two").unwrap();
        let d = date(2026, 11, 3);
        let same = (0..50).filter(|i| a.word_for(d + chrono::Days::new(*i)) == b.word_for(d + chrono::Days::new(*i))).count();
        assert!(same < 5, "different secrets must give different orders ({same} matches)");
    }

    #[test]
    fn local_provider_rejects_empty_list_and_dedupes() {
        assert!(LocalProvider::new(vec![], "s").is_err());
        assert_eq!(local(&["crane", "crane", "slate"], "s").len(), 2);
    }

    #[test]
    fn parse_answers_skips_comments_and_unknown_words() {
        let dict = crate::game::Dictionary::from_text("crane\nslate\ncigar\n");
        let parsed = parse_answers("# header\nCrane\n\nzzzzz\nslate\ntoolong\n", &dict);
        assert_eq!(parsed, vec!["crane".to_string(), "slate".to_string()]);
    }

    #[test]
    fn bundled_answers_are_all_valid_guesses() {
        let dict = crate::game::Dictionary::from_text(crate::DEFAULT_WORDS);
        let raw = crate::DEFAULT_ANSWERS.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).count();
        let parsed = parse_answers(crate::DEFAULT_ANSWERS, &dict);
        assert_eq!(parsed.len(), raw);
        assert!(parsed.len() > 1500);
    }

    #[test]
    fn time_until_next_rollover() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // 23:00 EDT -> one hour to midnight
        let now = Utc.with_ymd_and_hms(2024, 3, 16, 3, 0, 0).unwrap();
        assert_eq!(until_next_rollover(now, tz), Duration::from_secs(3600));
        // Day before DST ends (Nov 3, 2024): midnight is still EDT
        let now = Utc.with_ymd_and_hms(2024, 11, 3, 3, 30, 0).unwrap(); // 23:30 EDT Nov 2
        assert_eq!(until_next_rollover(now, tz), Duration::from_secs(1800));
    }

    #[test]
    fn rollover_in_winter_uses_est() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // EST is UTC-5 in January
        let before = Utc.with_ymd_and_hms(2024, 1, 16, 4, 59, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2024, 1, 16, 5, 0, 0).unwrap();
        assert_eq!(puzzle_date(before, tz), date(2024, 1, 15));
        assert_eq!(puzzle_date(after, tz), date(2024, 1, 16));
    }
}
