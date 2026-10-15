//! Streak calculation from finished-game history. Pure and clock-independent:
//! callers pass `today` (the current puzzle date in the puzzle timezone).

use chrono::{Days, NaiveDate};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FinishedGame {
    pub date: NaiveDate,
    pub won: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Streaks {
    pub games_played: i32,
    pub games_won: i32,
    pub current: i32,
    pub longest: i32,
    pub last_win: Option<NaiveDate>,
    pub last_played: Option<NaiveDate>,
}

/// Recompute everything from history.
///
/// * A streak is a run of consecutive puzzle dates that were all won.
/// * The current streak is the run ending at the latest win, provided that win
///   is today or yesterday (yesterday's win stays alive while today's puzzle is
///   still playable) and no loss happened after it.
/// * The longest streak is the longest run ever.
pub fn compute(games: &[FinishedGame], today: NaiveDate) -> Streaks {
    let mut wins: Vec<NaiveDate> = games.iter().filter(|g| g.won).map(|g| g.date).collect();
    wins.sort_unstable();
    wins.dedup();

    let last_win = wins.last().copied();
    let last_played = games.iter().map(|g| g.date).max();

    let mut longest = 0;
    let mut run = 0;
    let mut prev: Option<NaiveDate> = None;
    for d in &wins {
        run = match prev {
            Some(p) if p.checked_add_days(Days::new(1)) == Some(*d) => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        prev = Some(*d);
    }
    // `run` is now the run that ends at the latest win.

    let lost_after_last_win = games
        .iter()
        .any(|g| !g.won && last_win.map_or(true, |w| g.date > w));
    let current = match last_win {
        Some(w) if !lost_after_last_win && is_alive(w, today) => run,
        _ => 0,
    };

    Streaks {
        games_played: games.len() as i32,
        games_won: wins.len() as i32,
        current,
        longest,
        last_win,
        last_played,
    }
}

/// A streak ending at `last_win` is still alive if that was yesterday or today.
pub fn is_alive(last_win: NaiveDate, today: NaiveDate) -> bool {
    last_win >= today.checked_sub_days(Days::new(1)).unwrap_or(today)
}

/// Streak as it should be displayed now, given the value cached at the last
/// completion. Handles the "missed a whole day" reset without any background job.
pub fn effective_current(stored_current: i32, last_win: Option<NaiveDate>, today: NaiveDate) -> i32 {
    match last_win {
        Some(w) if is_alive(w, today) => stored_current,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, day).unwrap()
    }
    fn win(day: u32) -> FinishedGame {
        FinishedGame { date: d(day), won: true }
    }
    fn loss(day: u32) -> FinishedGame {
        FinishedGame { date: d(day), won: false }
    }

    #[test]
    fn empty_history() {
        assert_eq!(compute(&[], d(10)), Streaks::default());
    }

    #[test]
    fn consecutive_wins_extend_streak() {
        let s = compute(&[win(8), win(9), win(10)], d(10));
        assert_eq!((s.current, s.longest, s.games_won, s.games_played), (3, 3, 3, 3));
        assert_eq!(s.last_win, Some(d(10)));
    }

    #[test]
    fn yesterdays_streak_survives_while_today_is_open() {
        let s = compute(&[win(8), win(9)], d(10));
        assert_eq!((s.current, s.longest), (2, 2));
    }

    #[test]
    fn missed_day_resets_current_but_not_longest() {
        let s = compute(&[win(5), win(6), win(7)], d(9));
        assert_eq!((s.current, s.longest), (0, 3));
    }

    #[test]
    fn loss_resets_current_immediately() {
        let s = compute(&[win(8), win(9), loss(10)], d(10));
        assert_eq!((s.current, s.longest, s.games_played, s.games_won), (0, 2, 3, 2));
    }

    #[test]
    fn win_after_loss_starts_new_streak() {
        let s = compute(&[win(7), win(8), loss(9), win(10)], d(10));
        assert_eq!((s.current, s.longest), (1, 2));
    }

    #[test]
    fn gap_splits_runs() {
        let s = compute(&[win(1), win(2), win(3), win(5), win(6)], d(6));
        assert_eq!((s.current, s.longest), (2, 3));
    }

    #[test]
    fn longest_never_decreases_after_reset() {
        let before = compute(&[win(1), win(2), win(3), win(4)], d(4));
        let after = compute(&[win(1), win(2), win(3), win(4), loss(5)], d(5));
        assert!(after.longest >= before.longest);
        assert_eq!(after.current, 0);
    }

    #[test]
    fn duplicate_rows_for_a_day_do_not_inflate_streak() {
        let s = compute(&[win(9), win(9), win(10)], d(10));
        assert_eq!((s.current, s.longest, s.games_won), (2, 2, 2));
    }

    #[test]
    fn only_losses() {
        let s = compute(&[loss(3), loss(4)], d(4));
        assert_eq!((s.current, s.longest, s.games_won, s.games_played), (0, 0, 0, 2));
        assert_eq!(s.last_win, None);
    }

    #[test]
    fn effective_current_expires_after_missed_day() {
        assert_eq!(effective_current(5, Some(d(9)), d(10)), 5);
        assert_eq!(effective_current(5, Some(d(10)), d(10)), 5);
        assert_eq!(effective_current(5, Some(d(8)), d(10)), 0);
        assert_eq!(effective_current(5, None, d(10)), 0);
    }

    #[test]
    fn works_across_month_boundary() {
        let a = NaiveDate::from_ymd_opt(2026, 2, 28).unwrap();
        let b = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let s = compute(
            &[FinishedGame { date: a, won: true }, FinishedGame { date: b, won: true }],
            b,
        );
        // Feb 2026 has 28 days, so these are consecutive.
        assert_eq!(s.current, 2);
    }
}
