//! Game orchestration on top of PostgreSQL. All state lives in the database so
//! the bot can restart at any time; every multi-step change is one transaction
//! and guess submission serialises per game through a row lock.

use std::sync::Arc;

use chrono::NaiveDate;
use chrono_tz::Tz;
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::{Error, Result};
use crate::game::{
    check_hard_mode, evaluate, feedback_from_str, feedback_to_string, is_win, normalize_guess, Dictionary, Feedback,
    GameMode, Guess, GuessFormatError, HardModeViolation, MAX_GUESSES,
};
use crate::puzzle::{puzzle_date, Clock, PuzzleService};
use crate::streaks::{self, FinishedGame, Streaks};

#[derive(Clone, Debug)]
pub struct Player {
    pub user_id: i64,
    pub display_name: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GameStatus {
    InProgress,
    Won,
    Lost,
}

impl GameStatus {
    fn parse(s: &str) -> GameStatus {
        match s {
            "won" => GameStatus::Won,
            "lost" => GameStatus::Lost,
            _ => GameStatus::InProgress,
        }
    }

    pub fn is_finished(self) -> bool {
        self != GameStatus::InProgress
    }
}

#[derive(Clone, Debug)]
pub struct GameView {
    pub game_id: i64,
    pub puzzle_date: NaiveDate,
    pub puzzle_number: i32,
    pub mode: GameMode,
    pub status: GameStatus,
    pub guesses: Vec<Guess>,
    /// Only populated once the player has lost; never exposed earlier.
    pub solution: Option<String>,
}

impl GameView {
    pub fn guesses_remaining(&self) -> usize {
        MAX_GUESSES - self.guesses.len()
    }
}

#[derive(Clone, Debug)]
pub struct StartOutcome {
    pub view: GameView,
    pub created: bool,
    /// The requested mode differs from the mode this game is locked into.
    pub mode_locked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    Format(GuessFormatError),
    NotInDictionary(String),
    HardMode(HardModeViolation),
    NoGame,
    AlreadyFinished,
}

#[derive(Clone, Debug)]
pub enum GuessOutcome {
    Accepted {
        view: GameView,
        /// Set when this guess finished the game.
        stats: Option<PlayerStats>,
    },
    Rejected(Rejection),
    /// The same Fluxer message was already processed (gateway replay / retry).
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerStats {
    pub display_name: String,
    pub games_played: i32,
    pub games_won: i32,
    pub current_streak: i32,
    pub longest_streak: i32,
    pub distribution: [i64; MAX_GUESSES],
}

impl PlayerStats {
    pub fn win_rate_percent(&self) -> i32 {
        if self.games_played == 0 {
            0
        } else {
            ((self.games_won as f64 / self.games_played as f64) * 100.0).round() as i32
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeChange {
    pub preferred: GameMode,
    /// Today's not-yet-started game was switched to the new mode.
    pub applied_to_today: bool,
    /// Today's game already has guesses in a different mode.
    pub locked_today: bool,
}

#[derive(Clone, Debug)]
pub struct ShareData {
    pub puzzle_number: i32,
    pub mode: GameMode,
    pub won: bool,
    pub guesses_used: usize,
    pub rows: Vec<Feedback>,
}

#[derive(Clone, Debug)]
pub enum ShareOutcome {
    NoGame,
    InProgress,
    Ready(ShareData),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum LeaderboardSort {
    #[default]
    Solved,
    CurrentStreak,
    LongestStreak,
}

impl LeaderboardSort {
    pub fn parse(s: &str) -> Option<LeaderboardSort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "solved" | "words" | "total" | "wins" => Some(LeaderboardSort::Solved),
            "streak" | "current" => Some(LeaderboardSort::CurrentStreak),
            "longest" | "best" | "max" => Some(LeaderboardSort::LongestStreak),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LeaderboardSort::Solved => "Words Solved",
            LeaderboardSort::CurrentStreak => "Current Streak",
            LeaderboardSort::LongestStreak => "Longest Streak",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaderboardEntry {
    pub rank: usize,
    pub user_id: i64,
    pub display_name: String,
    pub solved: i32,
    pub current_streak: i32,
    pub longest_streak: i32,
}

/// Sort entries for `sort`. Players tied on the whole sort key share a rank, and
/// the next distinct score takes the next rank (dense ranking: 1, 1, 1, 2).
pub fn rank_entries(mut entries: Vec<LeaderboardEntry>, sort: LeaderboardSort) -> Vec<LeaderboardEntry> {
    let key = |e: &LeaderboardEntry| match sort {
        LeaderboardSort::Solved => (e.solved, e.longest_streak, e.current_streak),
        LeaderboardSort::CurrentStreak => (e.current_streak, e.longest_streak, e.solved),
        LeaderboardSort::LongestStreak => (e.longest_streak, e.current_streak, e.solved),
    };
    entries.sort_by(|a, b| {
        key(b)
            .cmp(&key(a))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    let mut rank = 0;
    let mut previous = None;
    for entry in entries.iter_mut() {
        let k = key(entry);
        if previous != Some(k) {
            rank += 1;
            previous = Some(k);
        }
        entry.rank = rank;
    }
    entries
}

#[derive(sqlx::FromRow)]
struct GameRow {
    id: i64,
    puzzle_date: NaiveDate,
    puzzle_number: i32,
    mode: String,
    guesses_used: i16,
    status: String,
    solution: String,
}

#[derive(sqlx::FromRow)]
struct GuessRow {
    word: String,
    feedback: String,
}

pub struct GameService {
    pool: PgPool,
    puzzles: Arc<PuzzleService>,
    dictionary: Arc<Dictionary>,
    clock: Arc<dyn Clock>,
    tz: Tz,
}

type Tx<'a> = Transaction<'a, Postgres>;

impl GameService {
    pub fn new(
        pool: PgPool,
        puzzles: Arc<PuzzleService>,
        dictionary: Arc<Dictionary>,
        clock: Arc<dyn Clock>,
        tz: Tz,
    ) -> GameService {
        GameService { pool, puzzles, dictionary, clock, tz }
    }

    pub fn today(&self) -> NaiveDate {
        puzzle_date(self.clock.now(), self.tz)
    }

    async fn upsert_player(tx: &mut Tx<'_>, player: &Player, preferred: Option<GameMode>) -> Result<GameMode> {
        let row: (String,) = sqlx::query_as(
            "INSERT INTO wordle_players (user_id, display_name, preferred_mode)
             VALUES ($1, $2, COALESCE($3, 'normal'))
             ON CONFLICT (user_id) DO UPDATE SET
                display_name = EXCLUDED.display_name,
                preferred_mode = COALESCE($3, wordle_players.preferred_mode),
                updated_at = now()
             RETURNING preferred_mode",
        )
        .bind(player.user_id)
        .bind(&player.display_name)
        .bind(preferred.map(|m| m.as_str()))
        .fetch_one(&mut **tx)
        .await?;
        Ok(GameMode::parse(&row.0).unwrap_or_default())
    }

    async fn load_guesses(tx: &mut Tx<'_>, game_id: i64) -> Result<Vec<Guess>> {
        let rows: Vec<GuessRow> =
            sqlx::query_as("SELECT word, feedback FROM wordle_guesses WHERE game_id = $1 ORDER BY guess_number")
                .bind(game_id)
                .fetch_all(&mut **tx)
                .await?;
        rows.into_iter()
            .map(|r| {
                let feedback = feedback_from_str(&r.feedback)
                    .ok_or_else(|| Error::Internal(format!("corrupt feedback for game {game_id}")))?;
                Ok(Guess { word: r.word, feedback })
            })
            .collect()
    }

    fn view(row: &GameRow, guesses: Vec<Guess>) -> GameView {
        let status = GameStatus::parse(&row.status);
        GameView {
            game_id: row.id,
            puzzle_date: row.puzzle_date,
            puzzle_number: row.puzzle_number,
            mode: GameMode::parse(&row.mode).unwrap_or_default(),
            status,
            guesses,
            solution: (status == GameStatus::Lost).then(|| row.solution.clone()),
        }
    }

    async fn lock_todays_game(tx: &mut Tx<'_>, user_id: i64, today: NaiveDate) -> Result<Option<GameRow>> {
        Ok(sqlx::query_as::<_, GameRow>(
            "SELECT g.id, g.puzzle_date, g.puzzle_number, g.mode, g.guesses_used, g.status, p.solution
             FROM wordle_games g JOIN wordle_daily_puzzles p USING (puzzle_date)
             WHERE g.user_id = $1 AND g.puzzle_date = $2
             FOR UPDATE OF g",
        )
        .bind(user_id)
        .bind(today)
        .fetch_optional(&mut **tx)
        .await?)
    }

    /// Start or resume today's game. `requested` overrides the player's
    /// preferred mode for today's game while it has no accepted guesses.
    pub async fn start_game(
        &self,
        player: &Player,
        guild_id: Option<i64>,
        requested: Option<GameMode>,
    ) -> Result<StartOutcome> {
        let today = self.today();
        let puzzle = self.puzzles.get_or_fetch(today, today).await?;

        let mut tx = self.pool.begin().await?;
        let preferred = Self::upsert_player(&mut tx, player, None).await?;
        if let Some(guild) = guild_id {
            sqlx::query(
                "INSERT INTO wordle_guild_players (guild_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(guild)
            .bind(player.user_id)
            .execute(&mut *tx)
            .await?;
        }

        let mode = requested.unwrap_or(preferred);
        let inserted = sqlx::query(
            "INSERT INTO wordle_games (user_id, guild_id, puzzle_date, puzzle_number, mode)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (user_id, puzzle_date) DO NOTHING",
        )
        .bind(player.user_id)
        .bind(guild_id)
        .bind(today)
        .bind(puzzle.puzzle_number)
        .bind(mode.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        let mut row = Self::lock_todays_game(&mut tx, player.user_id, today)
            .await?
            .ok_or_else(|| Error::Internal("game missing after insert".into()))?;

        let mut mode_locked = false;
        if let Some(req) = requested {
            if GameMode::parse(&row.mode) != Some(req) {
                if row.status == "in_progress" && row.guesses_used == 0 {
                    sqlx::query("UPDATE wordle_games SET mode = $2 WHERE id = $1")
                        .bind(row.id)
                        .bind(req.as_str())
                        .execute(&mut *tx)
                        .await?;
                    row.mode = req.as_str().to_string();
                } else {
                    mode_locked = true;
                }
            }
        }

        let guesses = Self::load_guesses(&mut tx, row.id).await?;
        tx.commit().await?;
        Ok(StartOutcome { view: Self::view(&row, guesses), created: inserted, mode_locked })
    }

    /// Submit a guess for today's game. Invalid guesses never consume an attempt.
    /// `message_id` makes retries and gateway replays of the same message idempotent.
    pub async fn submit_guess(&self, user_id: i64, raw: &str, message_id: Option<i64>) -> Result<GuessOutcome> {
        let word = match normalize_guess(raw) {
            Ok(w) => w,
            Err(e) => return Ok(GuessOutcome::Rejected(Rejection::Format(e))),
        };
        let today = self.today();

        match self.submit_inner(user_id, &word, message_id, today).await {
            Err(Error::Database(sqlx::Error::Database(db))) if db.is_unique_violation() && message_id.is_some() => {
                Ok(GuessOutcome::Duplicate)
            }
            other => other,
        }
    }

    async fn submit_inner(
        &self,
        user_id: i64,
        word: &str,
        message_id: Option<i64>,
        today: NaiveDate,
    ) -> Result<GuessOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some(row) = Self::lock_todays_game(&mut tx, user_id, today).await? else {
            return Ok(GuessOutcome::Rejected(Rejection::NoGame));
        };

        if let Some(mid) = message_id {
            let seen: Option<(i64,)> = sqlx::query_as("SELECT id FROM wordle_guesses WHERE source_message_id = $1")
                .bind(mid)
                .fetch_optional(&mut *tx)
                .await?;
            if seen.is_some() {
                return Ok(GuessOutcome::Duplicate);
            }
        }

        if GameStatus::parse(&row.status).is_finished() {
            return Ok(GuessOutcome::Rejected(Rejection::AlreadyFinished));
        }

        if !self.dictionary.contains(word) && word != row.solution {
            return Ok(GuessOutcome::Rejected(Rejection::NotInDictionary(word.to_string())));
        }

        let mut guesses = Self::load_guesses(&mut tx, row.id).await?;
        let mode = GameMode::parse(&row.mode).unwrap_or_default();
        if mode == GameMode::Hard {
            if let Err(v) = check_hard_mode(&guesses, word) {
                return Ok(GuessOutcome::Rejected(Rejection::HardMode(v)));
            }
        }

        let feedback = evaluate(word, &row.solution);
        let number = guesses.len() + 1;
        let won = is_win(&feedback);
        let lost = !won && number >= MAX_GUESSES;
        let status = if won {
            "won"
        } else if lost {
            "lost"
        } else {
            "in_progress"
        };

        sqlx::query(
            "INSERT INTO wordle_guesses (game_id, guess_number, word, feedback, source_message_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(row.id)
        .bind(number as i16)
        .bind(word)
        .bind(feedback_to_string(&feedback))
        .bind(message_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE wordle_games
             SET guesses_used = $2, status = $3,
                 completed_at = CASE WHEN $3 = 'in_progress' THEN NULL ELSE now() END
             WHERE id = $1",
        )
        .bind(row.id)
        .bind(number as i16)
        .bind(status)
        .execute(&mut *tx)
        .await?;

        guesses.push(Guess { word: word.to_string(), feedback });

        let stats = if won || lost {
            let s = Self::recompute_stats_tx(&mut tx, user_id, today).await?;
            Some(s)
        } else {
            None
        };

        let mut updated = row;
        updated.guesses_used = number as i16;
        updated.status = status.to_string();
        let view = Self::view(&updated, guesses);

        let stats = match stats {
            Some(_) => Some(Self::player_stats_tx(&mut tx, user_id, today).await?.ok_or_else(|| {
                Error::Internal("player missing after completion".into())
            })?),
            None => None,
        };
        tx.commit().await?;
        Ok(GuessOutcome::Accepted { view, stats })
    }

    /// Rebuild the cached stats row from game history.
    async fn recompute_stats_tx(tx: &mut Tx<'_>, user_id: i64, today: NaiveDate) -> Result<Streaks> {
        let rows: Vec<(NaiveDate, bool)> = sqlx::query_as(
            "SELECT puzzle_date, status = 'won' FROM wordle_games
             WHERE user_id = $1 AND status <> 'in_progress' ORDER BY puzzle_date",
        )
        .bind(user_id)
        .fetch_all(&mut **tx)
        .await?;
        let finished: Vec<FinishedGame> = rows.into_iter().map(|(date, won)| FinishedGame { date, won }).collect();
        let s = streaks::compute(&finished, today);

        sqlx::query(
            "UPDATE wordle_players SET games_played = $2, games_won = $3, current_streak = $4,
                 longest_streak = $5, last_win_date = $6, last_played_date = $7, updated_at = now()
             WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(s.games_played)
        .bind(s.games_won)
        .bind(s.current)
        .bind(s.longest)
        .bind(s.last_win)
        .bind(s.last_played)
        .execute(&mut **tx)
        .await?;
        Ok(s)
    }

    /// Recompute a player's cached statistics from history (maintenance / validation).
    pub async fn recompute_stats(&self, user_id: i64) -> Result<Streaks> {
        let mut tx = self.pool.begin().await?;
        let s = Self::recompute_stats_tx(&mut tx, user_id, self.today()).await?;
        tx.commit().await?;
        Ok(s)
    }

    async fn player_stats_tx(tx: &mut Tx<'_>, user_id: i64, today: NaiveDate) -> Result<Option<PlayerStats>> {
        let row: Option<(String, i32, i32, i32, i32, Option<NaiveDate>)> = sqlx::query_as(
            "SELECT display_name, games_played, games_won, current_streak, longest_streak, last_win_date
             FROM wordle_players WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((display_name, games_played, games_won, current, longest, last_win)) = row else {
            return Ok(None);
        };

        let dist_rows: Vec<(i16, i64)> = sqlx::query_as(
            "SELECT guesses_used, count(*) FROM wordle_games
             WHERE user_id = $1 AND status = 'won' GROUP BY guesses_used",
        )
        .bind(user_id)
        .fetch_all(&mut **tx)
        .await?;
        let mut distribution = [0i64; MAX_GUESSES];
        for (used, count) in dist_rows {
            if (1..=MAX_GUESSES as i16).contains(&used) {
                distribution[(used - 1) as usize] = count;
            }
        }

        Ok(Some(PlayerStats {
            display_name,
            games_played,
            games_won,
            current_streak: streaks::effective_current(current, last_win, today),
            longest_streak: longest,
            distribution,
        }))
    }

    pub async fn stats(&self, user_id: i64) -> Result<Option<PlayerStats>> {
        let mut tx = self.pool.begin().await?;
        let s = Self::player_stats_tx(&mut tx, user_id, self.today()).await?;
        tx.commit().await?;
        Ok(s)
    }

    pub async fn board(&self, user_id: i64) -> Result<Option<GameView>> {
        let today = self.today();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, GameRow>(
            "SELECT g.id, g.puzzle_date, g.puzzle_number, g.mode, g.guesses_used, g.status, p.solution
             FROM wordle_games g JOIN wordle_daily_puzzles p USING (puzzle_date)
             WHERE g.user_id = $1 AND g.puzzle_date = $2",
        )
        .bind(user_id)
        .bind(today)
        .fetch_optional(&mut *tx)
        .await?;
        let view = match row {
            Some(r) => {
                let guesses = Self::load_guesses(&mut tx, r.id).await?;
                Some(Self::view(&r, guesses))
            }
            None => None,
        };
        tx.commit().await?;
        Ok(view)
    }

    pub async fn set_preferred_mode(&self, player: &Player, mode: GameMode) -> Result<ModeChange> {
        let today = self.today();
        let mut tx = self.pool.begin().await?;
        Self::upsert_player(&mut tx, player, Some(mode)).await?;

        let switched = sqlx::query(
            "UPDATE wordle_games SET mode = $3
             WHERE user_id = $1 AND puzzle_date = $2 AND status = 'in_progress'
               AND guesses_used = 0 AND mode <> $3",
        )
        .bind(player.user_id)
        .bind(today)
        .bind(mode.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        let locked: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM wordle_games
             WHERE user_id = $1 AND puzzle_date = $2 AND mode <> $3 AND guesses_used > 0",
        )
        .bind(player.user_id)
        .bind(today)
        .bind(mode.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(ModeChange { preferred: mode, applied_to_today: switched, locked_today: locked.is_some() })
    }

    pub async fn share(&self, user_id: i64) -> Result<ShareOutcome> {
        Ok(match self.board(user_id).await? {
            None => ShareOutcome::NoGame,
            Some(v) if !v.status.is_finished() => ShareOutcome::InProgress,
            Some(v) => ShareOutcome::Ready(ShareData {
                puzzle_number: v.puzzle_number,
                mode: v.mode,
                won: v.status == GameStatus::Won,
                guesses_used: v.guesses.len(),
                rows: v.guesses.iter().map(|g| g.feedback).collect(),
            }),
        })
    }

    pub async fn leaderboard(&self, guild_id: i64, sort: LeaderboardSort) -> Result<Vec<LeaderboardEntry>> {
        let today = self.today();
        let rows: Vec<(i64, String, i32, i32, i32, Option<NaiveDate>)> = sqlx::query_as(
            "SELECT p.user_id, p.display_name, p.games_won, p.current_streak, p.longest_streak, p.last_win_date
             FROM wordle_guild_players g JOIN wordle_players p USING (user_id)
             WHERE g.guild_id = $1 AND p.games_played > 0",
        )
        .bind(guild_id)
        .fetch_all(&self.pool)
        .await?;

        let entries = rows
            .into_iter()
            .map(|(user_id, display_name, solved, current, longest, last_win)| LeaderboardEntry {
                rank: 0,
                user_id,
                display_name,
                solved,
                current_streak: streaks::effective_current(current, last_win, today),
                longest_streak: longest,
            })
            .collect();
        Ok(rank_entries(entries, sort))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, name: &str, solved: i32, current: i32, longest: i32) -> LeaderboardEntry {
        LeaderboardEntry {
            rank: 0,
            user_id: id,
            display_name: name.into(),
            solved,
            current_streak: current,
            longest_streak: longest,
        }
    }

    #[test]
    fn ranks_by_solved_by_default() {
        let ranked = rank_entries(
            vec![entry(3, "Charlie", 89, 0, 16), entry(1, "Alice", 124, 18, 42), entry(2, "Bob", 107, 7, 29)],
            LeaderboardSort::Solved,
        );
        let names: Vec<_> = ranked.iter().map(|e| (e.rank, e.display_name.as_str())).collect();
        assert_eq!(names, vec![(1, "Alice"), (2, "Bob"), (3, "Charlie")]);
    }

    #[test]
    fn alternate_sorts() {
        let data = vec![entry(1, "Alice", 124, 3, 42), entry(2, "Bob", 107, 7, 29), entry(3, "Charlie", 89, 0, 50)];
        let by_current = rank_entries(data.clone(), LeaderboardSort::CurrentStreak);
        assert_eq!(by_current[0].display_name, "Bob");
        let by_longest = rank_entries(data, LeaderboardSort::LongestStreak);
        assert_eq!(by_longest[0].display_name, "Charlie");
    }

    #[test]
    fn ties_share_rank() {
        let ranked = rank_entries(
            vec![entry(2, "bob", 5, 1, 2), entry(1, "Alice", 5, 1, 2), entry(3, "Cy", 4, 0, 1)],
            LeaderboardSort::Solved,
        );
        assert_eq!(ranked.iter().map(|e| e.rank).collect::<Vec<_>>(), vec![1, 1, 2]);
        assert_eq!(ranked[0].display_name, "Alice");
    }

    #[test]
    fn sort_parsing() {
        assert_eq!(LeaderboardSort::parse("streak"), Some(LeaderboardSort::CurrentStreak));
        assert_eq!(LeaderboardSort::parse("LONGEST"), Some(LeaderboardSort::LongestStreak));
        assert_eq!(LeaderboardSort::parse("solved"), Some(LeaderboardSort::Solved));
        assert_eq!(LeaderboardSort::parse("nope"), None);
    }

    #[test]
    fn win_rate() {
        let s = PlayerStats {
            display_name: "x".into(),
            games_played: 3,
            games_won: 2,
            current_streak: 0,
            longest_streak: 0,
            distribution: [0; 6],
        };
        assert_eq!(s.win_rate_percent(), 67);
    }
}
