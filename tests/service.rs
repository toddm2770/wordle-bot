//! Database-backed tests for the game service.
//!
//! Each test gets a fresh, migrated database from `#[sqlx::test]`. They need a
//! PostgreSQL server and a `DATABASE_URL` whose role may create databases, e.g.
//!
//! ```text
//! DATABASE_URL=postgres://wordle:wordle@localhost/wordle_test cargo test
//! ```
//!
//! The clock and the word provider are fakes, so day transitions and upstream
//! failures are fully deterministic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use sqlx::PgPool;

use wordle_bot::error::Error;
use wordle_bot::game::{Dictionary, GameMode, HardModeViolation, Tile};
use wordle_bot::puzzle::{Clock, ProviderError, ProviderPuzzle, PuzzleService, WordProvider};
use wordle_bot::service::{
    GameService, GameStatus, GuessOutcome, LeaderboardSort, Origin, Player, Rejection, ShareOutcome,
};

const WORDS: &str = "crane\nslate\ncigar\nrebut\nsissy\nhumph\nawake\nblush\nfocal\nevade\nnaval\nserve\nheath\ndwarf\nmodel\nkarma\nstink\ngrade\nquiet\nbench\nabate\nfeign\nmajor\ndeath\nfresh\ncrust\nstool\ncolon\nabase\nmarry\nreact\ntrace\ncrate\nfuzzy\nmummy\npious\nradio\ntrain\nbrain\ngrain\ndrain\nstain\n";
const GUILD_A: i64 = 1_000;
const GUILD_B: i64 = 2_000;

fn day(n: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, n).unwrap()
}

/// Noon in New York on the given puzzle date.
fn noon(date: NaiveDate) -> DateTime<Utc> {
    let tz: Tz = "America/New_York".parse().unwrap();
    tz.from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap()).unwrap().with_timezone(&Utc)
}

struct TestClock(Mutex<DateTime<Utc>>);

impl TestClock {
    fn set_day(&self, date: NaiveDate) {
        *self.0.lock().unwrap() = noon(date);
    }
    fn set(&self, t: DateTime<Utc>) {
        *self.0.lock().unwrap() = t;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// Serves solutions from a map; dates not in the map are "not published".
struct FakeProvider {
    solutions: Mutex<HashMap<NaiveDate, String>>,
    fail: Mutex<bool>,
    calls: AtomicUsize,
}

impl FakeProvider {
    fn new(pairs: &[(NaiveDate, &str)]) -> FakeProvider {
        FakeProvider {
            solutions: Mutex::new(pairs.iter().map(|(d, w)| (*d, w.to_string())).collect()),
            fail: Mutex::new(false),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl WordProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "test"
    }

    async fn fetch(&self, date: NaiveDate) -> Result<ProviderPuzzle, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.fail.lock().unwrap() {
            return Err(ProviderError::BadResponse("simulated outage".into()));
        }
        match self.solutions.lock().unwrap().get(&date) {
            Some(s) => Ok(ProviderPuzzle { number: Some(2000 + date.format("%d").to_string().parse::<i32>().unwrap()), solution: s.clone() }),
            None => Err(ProviderError::NotAvailable(date)),
        }
    }
}

struct Harness {
    pool: PgPool,
    clock: Arc<TestClock>,
    provider: Arc<FakeProvider>,
    svc: GameService,
}

impl Harness {
    fn new(pool: PgPool, solutions: &[(NaiveDate, &str)]) -> Harness {
        let clock = Arc::new(TestClock(Mutex::new(noon(day(1)))));
        let provider = Arc::new(FakeProvider::new(solutions));
        let svc = Self::service(&pool, &clock, &provider);
        Harness { pool, clock, provider, svc }
    }

    /// A brand-new service over the same database, as after a bot restart.
    fn service(pool: &PgPool, clock: &Arc<TestClock>, provider: &Arc<FakeProvider>) -> GameService {
        GameService::new(
            pool.clone(),
            Arc::new(PuzzleService::new(pool.clone(), provider.clone())),
            Arc::new(Dictionary::from_text(WORDS)),
            clock.clone(),
            "America/New_York".parse().unwrap(),
        )
    }

    fn restart(&mut self) {
        self.svc = Self::service(&self.pool, &self.clock, &self.provider);
    }

    async fn guess(&self, user: i64, word: &str) -> GuessOutcome {
        self.svc.submit_guess(user, word, None).await.unwrap()
    }

    /// Play one complete day: start, then the given guesses.
    async fn play_day(&self, p: &Player, guild: i64, guesses: &[&str]) {
        self.svc.start_game(p, Some(guild), None).await.unwrap();
        for g in guesses {
            assert!(matches!(self.guess(p.user_id, g).await, GuessOutcome::Accepted { .. }), "guess {g} rejected");
        }
    }
}

fn player(id: i64, name: &str) -> Player {
    Player { user_id: id, display_name: name.into() }
}

fn accepted(o: GuessOutcome) -> (wordle_bot::service::GameView, Option<wordle_bot::service::PlayerStats>) {
    match o {
        GuessOutcome::Accepted { view, stats } => (view, stats),
        other => panic!("expected Accepted, got {other:?}"),
    }
}

fn rejected(o: GuessOutcome) -> Rejection {
    match o {
        GuessOutcome::Rejected(r) => r,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

const LOSING: [&str; 6] = ["slate", "rebut", "humph", "focal", "dwarf", "karma"];

// ---------------------------------------------------------------- gameplay

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn win_updates_stats_and_reports_feedback(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    let alice = player(1, "Alice");

    let start = h.svc.start_game(&alice, Some(GUILD_A), None).await.unwrap();
    assert!(start.created);
    assert_eq!(start.view.puzzle_number, 2001, "provider's puzzle number is used");
    assert_eq!(start.view.mode, GameMode::Normal);
    assert_eq!(start.view.guesses_remaining(), 6);
    assert!(start.view.solution.is_none());

    let (view, stats) = accepted(h.guess(1, "crane").await);
    assert_eq!(view.status, GameStatus::InProgress);
    assert_eq!(view.guesses[0].feedback, [Tile::Correct, Tile::Present, Tile::Present, Tile::Absent, Tile::Absent]);
    assert!(stats.is_none());
    assert!(view.solution.is_none(), "solution must stay hidden while playing");

    let (view, stats) = accepted(h.guess(1, "CIGAR").await);
    assert_eq!(view.status, GameStatus::Won);
    assert!(view.solution.is_none(), "a win does not need to reveal anything extra");
    let stats = stats.expect("stats on completion");
    assert_eq!((stats.games_played, stats.games_won, stats.current_streak, stats.longest_streak), (1, 1, 1, 1));
    assert_eq!(stats.distribution, [0, 1, 0, 0, 0, 0]);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn loss_reveals_solution_and_resets_streak(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "crane")]);
    let bob = player(2, "Bob");
    h.play_day(&bob, GUILD_A, &["cigar"]).await;

    h.clock.set_day(day(2));
    h.svc.start_game(&bob, Some(GUILD_A), None).await.unwrap();
    for (i, w) in LOSING.iter().enumerate() {
        let (view, stats) = accepted(h.guess(2, w).await);
        if i < 5 {
            assert_eq!(view.status, GameStatus::InProgress);
            assert!(view.solution.is_none());
        } else {
            assert_eq!(view.status, GameStatus::Lost);
            assert_eq!(view.solution.as_deref(), Some("crane"));
            let s = stats.unwrap();
            assert_eq!((s.games_played, s.games_won, s.current_streak, s.longest_streak), (2, 1, 0, 1));
        }
    }
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn invalid_guesses_do_not_consume_attempts(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();

    assert!(matches!(rejected(h.guess(1, "cran").await), Rejection::Format(_)));
    assert!(matches!(rejected(h.guess(1, "cr4ne").await), Rejection::Format(_)));
    assert_eq!(rejected(h.guess(1, "zzzzz").await), Rejection::NotInDictionary("zzzzz".into()));

    let board = h.svc.board(1).await.unwrap().unwrap();
    assert_eq!(board.guesses.len(), 0);
    assert_eq!(board.guesses_remaining(), 6);

    let (view, _) = accepted(h.guess(1, "crane").await);
    assert_eq!(view.guesses.len(), 1);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn guessing_without_a_game_is_rejected(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    assert_eq!(rejected(h.guess(9, "crane").await), Rejection::NoGame);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn finished_game_cannot_be_replayed(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    let p = player(1, "A");
    h.play_day(&p, GUILD_A, &LOSING).await;

    assert_eq!(rejected(h.guess(1, "cigar").await), Rejection::AlreadyFinished);
    let again = h.svc.start_game(&p, Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    assert!(!again.created);
    assert_eq!(again.view.status, GameStatus::Lost);
    assert_eq!(again.view.guesses.len(), 6);

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM wordle_games WHERE user_id = 1").fetch_one(&h.pool).await.unwrap();
    assert_eq!(rows.0, 1);
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.games_played, s.games_won), (1, 0));
}

// ---------------------------------------------------------------- modes

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn hard_mode_enforced_and_violations_are_free(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "train")]);
    let start = h.svc.start_game(&player(1, "A"), Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    assert_eq!(start.view.mode, GameMode::Hard);

    // CRANE vs TRAIN: R and A green, N yellow
    accepted(h.guess(1, "crane").await);
    assert_eq!(
        rejected(h.guess(1, "slate").await),
        Rejection::HardMode(HardModeViolation::WrongPosition { position: 2, letter: 'r' })
    );
    assert_eq!(
        rejected(h.guess(1, "grade").await),
        Rejection::HardMode(HardModeViolation::MissingLetter { letter: 'n' })
    );
    assert_eq!(h.svc.board(1).await.unwrap().unwrap().guesses.len(), 1);
    // Gray letters (C, E) are not banned; BRAIN keeps R, A in place and includes N.
    accepted(h.guess(1, "brain").await);
    let (view, _) = accepted(h.guess(1, "train").await);
    assert_eq!(view.status, GameStatus::Won);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn normal_mode_allows_ignoring_clues(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "train")]);
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();
    accepted(h.guess(1, "crane").await);
    accepted(h.guess(1, "slate").await); // would violate hard mode
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn mode_can_change_until_first_guess_then_locks(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "crane")]);
    let p = player(1, "A");

    let s = h.svc.start_game(&p, Some(GUILD_A), None).await.unwrap();
    assert_eq!(s.view.mode, GameMode::Normal);
    let s = h.svc.start_game(&p, Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    assert_eq!(s.view.mode, GameMode::Hard);
    assert!(!s.mode_locked);

    let change = h.svc.set_preferred_mode(&p, GameMode::Normal).await.unwrap();
    assert!(change.applied_to_today && !change.locked_today);

    accepted(h.guess(1, "crane").await);
    let s = h.svc.start_game(&p, Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    assert!(s.mode_locked);
    assert_eq!(s.view.mode, GameMode::Normal);

    let change = h.svc.set_preferred_mode(&p, GameMode::Hard).await.unwrap();
    assert!(!change.applied_to_today && change.locked_today);
    assert_eq!(h.svc.board(1).await.unwrap().unwrap().mode, GameMode::Normal);

    // Preference carries to the next day.
    h.clock.set_day(day(2));
    let s = h.svc.start_game(&p, Some(GUILD_A), None).await.unwrap();
    assert_eq!(s.view.mode, GameMode::Hard);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn games_record_their_mode(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    h.svc.start_game(&player(2, "B"), Some(GUILD_A), None).await.unwrap();
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT user_id, mode FROM wordle_games ORDER BY user_id").fetch_all(&h.pool).await.unwrap();
    assert_eq!(rows, vec![(1, "hard".into()), (2, "normal".into())]);
}

// ---------------------------------------------------------------- idempotency & concurrency

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn duplicate_message_is_processed_once(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();

    accepted(h.svc.submit_guess(1, "crane", Some(555)).await.unwrap());
    assert!(matches!(h.svc.submit_guess(1, "crane", Some(555)).await.unwrap(), GuessOutcome::Duplicate));
    assert!(matches!(h.svc.submit_guess(1, "slate", Some(555)).await.unwrap(), GuessOutcome::Duplicate));
    assert_eq!(h.svc.board(1).await.unwrap().unwrap().guesses.len(), 1);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn concurrent_guesses_are_serialised(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();
    let svc = Arc::new(h.svc);

    // Ten simultaneous guesses: exactly six may be accepted, numbered 1..=6, and
    // the same message delivered twice at once must still count once.
    let words: [&'static str; 10] = ["crane", "slate", "rebut", "humph", "focal", "dwarf", "karma", "stink", "grade", "quiet"];
    let mut tasks = Vec::new();
    for (i, w) in words.into_iter().enumerate() {
        let svc = svc.clone();
        tasks.push(tokio::spawn(async move { svc.submit_guess(1, w, Some(100 + i as i64)).await.unwrap() }));
    }
    for _ in 0..3 {
        let svc = svc.clone();
        tasks.push(tokio::spawn(async move { svc.submit_guess(1, "crane", Some(100)).await.unwrap() }));
    }
    let mut accepted_count = 0;
    let mut finished_count = 0;
    for t in tasks {
        match t.await.unwrap() {
            GuessOutcome::Accepted { .. } => accepted_count += 1,
            GuessOutcome::Rejected(Rejection::AlreadyFinished) => finished_count += 1,
            GuessOutcome::Duplicate => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(accepted_count, 6);
    assert_eq!(finished_count, 4);

    let numbers: Vec<(i16,)> = sqlx::query_as("SELECT guess_number FROM wordle_guesses ORDER BY guess_number")
        .fetch_all(&h.pool)
        .await
        .unwrap();
    assert_eq!(numbers.into_iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6]);
    let game: (String, i16) = sqlx::query_as("SELECT status, guesses_used FROM wordle_games").fetch_one(&h.pool).await.unwrap();
    assert_eq!(game, ("lost".into(), 6));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn concurrent_starts_create_one_game(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    let svc = Arc::new(h.svc);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let svc = svc.clone();
            tokio::spawn(async move { svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap() })
        })
        .collect();
    let mut created = 0;
    for t in tasks {
        if t.await.unwrap().created {
            created += 1;
        }
    }
    assert_eq!(created, 1);
    assert_eq!(h.provider.calls.load(Ordering::SeqCst), 1, "puzzle fetched once and then served from cache");
}

// ---------------------------------------------------------------- persistence & puzzles

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn unfinished_game_survives_restart(pool: PgPool) {
    let mut h = Harness::new(pool, &[(day(1), "cigar")]);
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    accepted(h.guess(1, "crane").await);
    accepted(h.guess(1, "cigar").await);

    h.restart();
    let board = h.svc.board(1).await.unwrap().unwrap();
    assert_eq!(board.status, GameStatus::Won);

    // And a game in progress keeps its guesses and mode.
    let resumed = h.svc.start_game(&player(2, "B"), Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    assert!(resumed.created);
    accepted(h.guess(2, "slate").await);
    h.restart();
    let again = h.svc.start_game(&player(2, "B"), Some(GUILD_A), None).await.unwrap();
    assert!(!again.created);
    assert_eq!(again.view.mode, GameMode::Hard);
    assert_eq!(again.view.guesses.len(), 1);
    assert_eq!(again.view.guesses[0].word, "slate");
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn puzzle_is_cached_and_shared_by_all_players(pool: PgPool) {
    let mut h = Harness::new(pool, &[(day(1), "cigar")]);
    for id in 1..=5 {
        h.svc.start_game(&player(id, "p"), Some(GUILD_A), None).await.unwrap();
    }
    h.restart();
    h.svc.start_game(&player(6, "p"), Some(GUILD_A), None).await.unwrap();
    assert_eq!(h.provider.calls.load(Ordering::SeqCst), 1);

    let row: (String, i32, String) =
        sqlx::query_as("SELECT solution, puzzle_number, source FROM wordle_daily_puzzles").fetch_one(&h.pool).await.unwrap();
    assert_eq!(row, ("cigar".into(), 2001, "test".into()));

    // Upstream outage after caching does not matter.
    *h.provider.fail.lock().unwrap() = true;
    accepted(h.guess(6, "crane").await);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn provider_failure_is_reported_and_creates_nothing(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    *h.provider.fail.lock().unwrap() = true;
    let err = h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap_err();
    assert!(matches!(err, Error::PuzzleUnavailable(_)));
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM wordle_games").fetch_one(&h.pool).await.unwrap();
    assert_eq!(n.0, 0);

    // Recovers once upstream is back.
    *h.provider.fail.lock().unwrap() = false;
    assert!(h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap().created);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn unpublished_puzzle_is_unavailable(pool: PgPool) {
    let h = Harness::new(pool, &[]);
    let err = h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap_err();
    assert!(matches!(err, Error::PuzzleUnavailable(_)));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn daily_transition_at_new_york_midnight(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "crane")]);
    let tz: Tz = "America/New_York".parse().unwrap();
    let at = |d: NaiveDate, hh, mm| tz.from_local_datetime(&d.and_hms_opt(hh, mm, 0).unwrap()).unwrap().with_timezone(&Utc);

    h.clock.set(at(day(1), 23, 59));
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();
    accepted(h.guess(1, "slate").await);

    // One minute later it is a new puzzle: yesterday's unfinished game is closed.
    h.clock.set(at(day(2), 0, 0));
    assert!(h.svc.board(1).await.unwrap().is_none());
    assert_eq!(rejected(h.guess(1, "crane").await), Rejection::NoGame);

    let s = h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();
    assert!(s.created);
    assert_eq!(s.view.puzzle_date, day(2));
    assert_eq!(s.view.guesses.len(), 0);
    let (view, _) = accepted(h.guess(1, "crane").await);
    assert_eq!(view.status, GameStatus::Won);
}

// ---------------------------------------------------------------- streaks

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn streaks_extend_survive_until_missed_day_and_longest_never_drops(pool: PgPool) {
    let solutions: Vec<(NaiveDate, &str)> = (1..=12).map(|d| (day(d), "cigar")).collect();
    let h = Harness::new(pool, &solutions);
    let p = player(1, "A");

    for d in 1..=3 {
        h.clock.set_day(day(d));
        h.play_day(&p, GUILD_A, &["cigar"]).await;
        let s = h.svc.stats(1).await.unwrap().unwrap();
        assert_eq!(s.current_streak, d as i32);
    }

    // Day 4: yesterday's win keeps the streak alive while today is open.
    h.clock.set_day(day(4));
    assert_eq!(h.svc.stats(1).await.unwrap().unwrap().current_streak, 3);
    assert_eq!(h.svc.leaderboard(GUILD_A, LeaderboardSort::Solved).await.unwrap()[0].current_streak, 3);

    // Day 5: day 4 was missed and can no longer be played, so the streak is gone,
    // without any game being played or any background job running.
    h.clock.set_day(day(5));
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.current_streak, s.longest_streak, s.games_won), (0, 3, 3));
    assert_eq!(h.svc.leaderboard(GUILD_A, LeaderboardSort::Solved).await.unwrap()[0].current_streak, 0);

    // A new run starts at 1, and the longest stays 3 until it is beaten.
    h.play_day(&p, GUILD_A, &["cigar"]).await;
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.current_streak, s.longest_streak), (1, 3));

    for d in 6..=8 {
        h.clock.set_day(day(d));
        h.play_day(&p, GUILD_A, &["crane", "cigar"]).await;
    }
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.current_streak, s.longest_streak), (4, 4));

    // A loss resets immediately; longest is untouched.
    h.clock.set_day(day(9));
    h.play_day(&p, GUILD_A, &LOSING).await;
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.current_streak, s.longest_streak, s.games_played, s.games_won), (0, 4, 8, 7));

    // Cached stats always match a full recomputation from history.
    let recomputed = h.svc.recompute_stats(1).await.unwrap();
    assert_eq!((recomputed.current, recomputed.longest, recomputed.games_won), (0, 4, 7));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn hard_and_normal_wins_share_one_streak(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "cigar"), (day(3), "cigar")]);
    let p = player(1, "A");
    for (d, mode) in [(1, GameMode::Hard), (2, GameMode::Normal), (3, GameMode::Hard)] {
        h.clock.set_day(day(d));
        h.svc.start_game(&p, Some(GUILD_A), Some(mode)).await.unwrap();
        accepted(h.guess(1, "cigar").await);
    }
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.current_streak, s.longest_streak, s.games_won), (3, 3, 3));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn abandoned_game_breaks_streak(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "cigar"), (day(3), "cigar")]);
    let p = player(1, "A");
    h.play_day(&p, GUILD_A, &["cigar"]).await;
    h.clock.set_day(day(2));
    h.play_day(&p, GUILD_A, &["crane"]).await; // never finished
    h.clock.set_day(day(3));
    assert_eq!(h.svc.stats(1).await.unwrap().unwrap().current_streak, 0);
    h.play_day(&p, GUILD_A, &["cigar"]).await;
    let s = h.svc.stats(1).await.unwrap().unwrap();
    assert_eq!((s.current_streak, s.longest_streak), (1, 1));
}

// ---------------------------------------------------------------- leaderboard & share

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn leaderboard_is_accurate_sorted_and_per_guild(pool: PgPool) {
    let solutions: Vec<(NaiveDate, &str)> = (1..=4).map(|d| (day(d), "cigar")).collect();
    let h = Harness::new(pool, &solutions);
    let alice = player(1, "Alice");
    let bob = player(2, "Bob");
    let cara = player(3, "Cara");
    let dave = player(4, "Dave"); // only in guild B

    // Alice: wins days 1-4 (solved 4, current 4, longest 4)
    // Bob:   wins 1, 2, loses 3, wins 4 (solved 3, current 1, longest 2)
    // Cara:  wins 1 only (solved 1, current 0 after day 2 passes, longest 1)
    for d in 1..=4 {
        h.clock.set_day(day(d));
        h.play_day(&alice, GUILD_A, &["cigar"]).await;
        if d == 3 {
            h.play_day(&bob, GUILD_A, &LOSING).await;
        } else {
            h.play_day(&bob, GUILD_A, &["crane", "cigar"]).await;
        }
        if d == 1 {
            h.play_day(&cara, GUILD_A, &["cigar"]).await;
        }
        h.play_day(&dave, GUILD_B, &["cigar"]).await;
    }
    // Someone who only started a game should not appear.
    h.svc.start_game(&player(5, "Eve"), Some(GUILD_A), None).await.unwrap();

    let board = h.svc.leaderboard(GUILD_A, LeaderboardSort::Solved).await.unwrap();
    let rows: Vec<_> =
        board.iter().map(|e| (e.rank, e.display_name.as_str(), e.solved, e.current_streak, e.longest_streak)).collect();
    assert_eq!(rows, vec![(1, "Alice", 4, 4, 4), (2, "Bob", 3, 1, 2), (3, "Cara", 1, 0, 1)]);

    let by_longest = h.svc.leaderboard(GUILD_A, LeaderboardSort::LongestStreak).await.unwrap();
    assert_eq!(by_longest.iter().map(|e| e.display_name.as_str()).collect::<Vec<_>>(), vec!["Alice", "Bob", "Cara"]);
    let by_current = h.svc.leaderboard(GUILD_A, LeaderboardSort::CurrentStreak).await.unwrap();
    assert_eq!(by_current[1].display_name, "Bob");

    let b = h.svc.leaderboard(GUILD_B, LeaderboardSort::Solved).await.unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!((b[0].display_name.as_str(), b[0].solved), ("Dave", 4));

    // A player active in two guilds appears on both with the same stats.
    h.svc.start_game(&alice, Some(GUILD_B), None).await.unwrap();
    let b = h.svc.leaderboard(GUILD_B, LeaderboardSort::Solved).await.unwrap();
    assert_eq!(b.iter().find(|e| e.user_id == 1).unwrap().solved, 4);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn display_name_updates_but_identity_is_the_user_id(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    h.play_day(&player(1, "OldName"), GUILD_A, &["cigar"]).await;
    h.svc.start_game(&player(1, "NewName"), Some(GUILD_A), None).await.unwrap();
    let board = h.svc.leaderboard(GUILD_A, LeaderboardSort::Solved).await.unwrap();
    assert_eq!(board.len(), 1);
    assert_eq!((board[0].display_name.as_str(), board[0].solved), ("NewName", 1));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn share_only_after_finishing_and_without_letters(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    assert!(matches!(h.svc.share(1).await.unwrap(), ShareOutcome::NoGame));
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), Some(GameMode::Hard)).await.unwrap();
    accepted(h.guess(1, "crane").await);
    assert!(matches!(h.svc.share(1).await.unwrap(), ShareOutcome::InProgress));
    accepted(h.guess(1, "cigar").await);
    match h.svc.share(1).await.unwrap() {
        ShareOutcome::Ready(d) => {
            assert!(d.won);
            assert_eq!((d.guesses_used, d.puzzle_number, d.mode), (2, 2001, GameMode::Hard));
            let text = wordle_bot::bot::render::share_text("A", &d, None);
            assert!(!text.to_lowercase().contains("cigar") && !text.to_lowercase().contains("crane"));
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

// ---------------------------------------------------------------- community streak

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn community_streak_needs_one_solver_per_day(pool: PgPool) {
    let solutions: Vec<(NaiveDate, &str)> = (1..=8).map(|d| (day(d), "cigar")).collect();
    let h = Harness::new(pool, &solutions);
    let (alice, bob, cara) = (player(1, "Alice"), player(2, "Bob"), player(3, "Cara"));

    let empty = h.svc.community_streak(GUILD_A).await.unwrap();
    assert_eq!((empty.current, empty.longest), (0, 0));
    assert!(!empty.safe_today());

    // Day 1: only Alice solves. Day 2: only Bob. Day 3: both solve.
    h.play_day(&alice, GUILD_A, &["cigar"]).await;
    h.clock.set_day(day(2));
    h.play_day(&bob, GUILD_A, &["crane", "cigar"]).await;
    h.clock.set_day(day(3));
    h.play_day(&alice, GUILD_A, &["cigar"]).await;
    h.play_day(&bob, GUILD_A, &["cigar"]).await;
    let c = h.svc.community_streak(GUILD_A).await.unwrap();
    assert_eq!((c.current, c.longest), (3, 3));
    assert_eq!(c.solved_today, vec!["Alice".to_string(), "Bob".to_string()]);
    assert_eq!(c.puzzle_number, Some(2003));

    // Day 4: Cara loses, nobody solves. A loss doesn't break it, but the day is still open.
    h.clock.set_day(day(4));
    h.play_day(&cara, GUILD_A, &LOSING).await;
    let c = h.svc.community_streak(GUILD_A).await.unwrap();
    assert_eq!(c.current, 3, "yesterday's streak survives while today is open");
    assert!(!c.safe_today());

    // Day 5: day 4 passed with no solver, so the streak is broken; longest stays 3.
    h.clock.set_day(day(5));
    let c = h.svc.community_streak(GUILD_A).await.unwrap();
    assert_eq!((c.current, c.longest), (0, 3));

    // Cara solves on day 5 and starts a new streak of 1.
    h.play_day(&cara, GUILD_A, &["cigar"]).await;
    let c = h.svc.community_streak(GUILD_A).await.unwrap();
    assert_eq!((c.current, c.longest, c.solved_today.len()), (1, 3, 1));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn community_streaks_are_per_community(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "cigar")]);
    h.play_day(&player(1, "A"), GUILD_A, &["cigar"]).await;
    h.play_day(&player(2, "B"), GUILD_B, &["cigar"]).await;
    h.clock.set_day(day(2));
    h.play_day(&player(2, "B"), GUILD_B, &["cigar"]).await;

    let a = h.svc.community_streak(GUILD_A).await.unwrap();
    let b = h.svc.community_streak(GUILD_B).await.unwrap();
    assert_eq!((a.current, a.safe_today()), (1, false));
    assert_eq!((b.current, b.safe_today()), (2, true));

    // The win carries its community so the bot can announce it.
    h.svc.start_game(&player(1, "A"), Some(GUILD_A), None).await.unwrap();
    let (view, _) = accepted(h.guess(1, "cigar").await);
    assert_eq!(view.guild_id, Some(GUILD_A));
    assert_eq!(h.svc.community_streak(GUILD_A).await.unwrap().current, 2);
}

// ---------------------------------------------------------------- DM play, results channel, recaps

fn from_channel(guild_id: i64, channel_id: i64) -> Origin {
    Origin::Server { guild_id, channel_id: Some(channel_id) }
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn later_days_can_start_from_the_dm(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "crane")]);
    let p = player(1, "A");

    // Never played from a server: there is no community to credit.
    assert!(matches!(h.svc.start_game_at(&p, Origin::Dm, None).await, Err(Error::NoHomeCommunity)));

    let first = h.svc.start_game_at(&p, from_channel(GUILD_A, 77), Some(GameMode::Hard)).await.unwrap();
    assert_eq!((first.view.guild_id, first.view.channel_id), (Some(GUILD_A), Some(77)));
    accepted(h.guess(1, "cigar").await);

    // Next day, from the DM: same community and channel, preferred mode.
    h.clock.set_day(day(2));
    h.svc.set_preferred_mode(&p, GameMode::Hard).await.unwrap();
    let next = h.svc.start_game_at(&p, Origin::Dm, None).await.unwrap();
    assert!(next.created);
    assert_eq!((next.view.guild_id, next.view.channel_id, next.view.mode), (Some(GUILD_A), Some(77), GameMode::Hard));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn home_follows_the_last_server_channel(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "cigar"), (day(3), "cigar")]);
    let p = player(1, "A");
    h.svc.start_game_at(&p, from_channel(GUILD_A, 10), None).await.unwrap();
    h.clock.set_day(day(2));
    h.svc.start_game_at(&p, from_channel(GUILD_B, 20), None).await.unwrap();
    h.clock.set_day(day(3));
    let v = h.svc.start_game_at(&p, Origin::Dm, None).await.unwrap().view;
    assert_eq!((v.guild_id, v.channel_id), (Some(GUILD_B), Some(20)));
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn results_channel_can_be_set_and_cleared(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar")]);
    assert_eq!(h.svc.results_channel(GUILD_A).await.unwrap(), None);
    h.svc.set_results_channel(GUILD_A, Some(555), 1).await.unwrap();
    assert_eq!(h.svc.results_channel(GUILD_A).await.unwrap(), Some(555));
    assert_eq!(h.svc.results_channel(GUILD_B).await.unwrap(), None);
    h.svc.set_results_channel(GUILD_A, None, 1).await.unwrap();
    assert_eq!(h.svc.results_channel(GUILD_A).await.unwrap(), None);
}

#[sqlx::test(migrator = "wordle_bot::MIGRATOR")]
async fn result_posts_become_one_recap_after_the_day_ends(pool: PgPool) {
    let h = Harness::new(pool, &[(day(1), "cigar"), (day(2), "cigar")]);
    let (alice, bob, cara) = (player(1, "Alice"), player(2, "Bob"), player(3, "Cara"));
    h.svc.start_game_at(&alice, from_channel(GUILD_A, 9), Some(GameMode::Hard)).await.unwrap();
    let (a, _) = accepted(h.guess(1, "cigar").await);
    h.svc.start_game_at(&bob, from_channel(GUILD_A, 9), None).await.unwrap();
    for w in LOSING {
        accepted(h.guess(2, w).await);
    }
    let b = h.svc.board(2).await.unwrap().unwrap();
    h.svc.start_game_at(&cara, from_channel(GUILD_A, 9), None).await.unwrap();
    accepted(h.guess(3, "crane").await);
    let (c, _) = accepted(h.guess(3, "cigar").await);

    for (msg, v) in [(101, &a), (102, &b), (103, &c)] {
        assert!(!h.svc.result_posted(v.game_id).await.unwrap());
        h.svc.record_result_post(GUILD_A, day(1), 9, msg, v.game_id).await.unwrap();
        assert!(h.svc.result_posted(v.game_id).await.unwrap());
    }
    // Same game recorded twice is ignored.
    h.svc.record_result_post(GUILD_A, day(1), 9, 999, a.game_id).await.unwrap();

    // Nothing to recap while the day is still open.
    assert!(h.svc.pending_recaps().await.unwrap().is_empty());

    h.clock.set_day(day(2));
    let pending = h.svc.pending_recaps().await.unwrap();
    assert_eq!(pending.len(), 1);
    let r = &pending[0];
    assert_eq!((r.guild_id, r.puzzle_date, r.channel_id, r.puzzle_number), (GUILD_A, day(1), 9, 2001));
    assert_eq!(r.post_message_ids, vec![101, 102, 103]);

    let results = h.svc.day_results(GUILD_A, day(1)).await.unwrap();
    let summary: Vec<_> = results.iter().map(|r| (r.display_name.as_str(), r.won, r.guesses, r.mode)).collect();
    assert_eq!(
        summary,
        vec![("Alice", true, 1, GameMode::Hard), ("Cara", true, 2, GameMode::Normal), ("Bob", false, 6, GameMode::Normal)]
    );
    let streak = h.svc.community_streak_on(GUILD_A, day(1)).await.unwrap();
    assert_eq!(streak.current, 1);

    h.svc.complete_recap(r, Some(500)).await.unwrap();
    assert!(h.svc.pending_recaps().await.unwrap().is_empty(), "recap is done exactly once");
    let left: (i64,) = sqlx::query_as("SELECT count(*) FROM wordle_result_posts").fetch_one(&h.pool).await.unwrap();
    assert_eq!(left.0, 0);
}
