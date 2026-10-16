//! Text command parsing (`!wordle <subcommand> ...`).

use crate::game::GameMode;
use crate::service::LeaderboardSort;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Play(Option<GameMode>),
    Guess(String),
    Board,
    Stats,
    Leaderboard(LeaderboardSort),
    Mode(GameMode),
    Share,
    Community,
    Help,
    /// Recognised prefix but unusable arguments; carries a short usage hint.
    Usage(String),
}

/// Returns `None` when the message is not addressed to the bot at all.
pub fn parse(content: &str, prefix: &str) -> Option<Command> {
    let content = content.trim();
    if content.len() < prefix.len() || !content.is_char_boundary(prefix.len()) {
        return None;
    }
    let (head, rest) = content.split_at(prefix.len());
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }

    let mut parts = rest.split_whitespace();
    let Some(sub) = parts.next() else { return Some(Command::Help) };
    let arg = parts.next();
    let usage = |s: &str| Some(Command::Usage(s.to_string()));

    match sub.to_ascii_lowercase().as_str() {
        "play" | "start" => match arg {
            None => Some(Command::Play(None)),
            Some(a) => match GameMode::parse(a) {
                Some(m) => Some(Command::Play(Some(m))),
                None => usage(&format!("Usage: `{prefix} play [normal|hard]`")),
            },
        },
        "guess" | "g" => match arg {
            Some(a) => Some(Command::Guess(a.to_string())),
            None => usage(&format!("Usage: `{prefix} guess CRANE`")),
        },
        "board" | "b" => Some(Command::Board),
        "stats" | "stat" | "me" => Some(Command::Stats),
        "leaderboard" | "lb" | "top" => match arg {
            None => Some(Command::Leaderboard(LeaderboardSort::Solved)),
            Some(a) => match LeaderboardSort::parse(a) {
                Some(s) => Some(Command::Leaderboard(s)),
                None => usage(&format!("Usage: `{prefix} leaderboard [solved|streak|longest]`")),
            },
        },
        "mode" => match arg.and_then(GameMode::parse) {
            Some(m) => Some(Command::Mode(m)),
            None => usage(&format!("Usage: `{prefix} mode normal` or `{prefix} mode hard`")),
        },
        "share" => Some(Command::Share),
        "community" | "server" | "group" | "team" | "streak" => Some(Command::Community),
        "help" | "?" => Some(Command::Help),
        other => usage(&format!("Unknown command `{other}`. Try `{prefix} help`.")),
    }
}

/// A DM consisting of a single five-letter word (e.g. `crane`) is a guess.
pub fn bare_guess(content: &str) -> Option<Command> {
    let word = content.trim();
    (word.len() == crate::game::WORD_LEN && word.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| Command::Guess(word.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "!wordle";

    #[test]
    fn ignores_unrelated_messages() {
        assert_eq!(parse("hello", P), None);
        assert_eq!(parse("!wordlexyz", P), None);
        assert_eq!(parse("", P), None);
        assert_eq!(parse("!word", P), None);
        assert_eq!(parse("é!wordle", P), None);
    }

    #[test]
    fn bare_prefix_shows_help() {
        assert_eq!(parse("!wordle", P), Some(Command::Help));
        assert_eq!(parse("  !WORDLE  ", P), Some(Command::Help));
    }

    #[test]
    fn parses_all_commands() {
        assert_eq!(parse("!wordle play", P), Some(Command::Play(None)));
        assert_eq!(parse("!wordle play hard", P), Some(Command::Play(Some(GameMode::Hard))));
        assert_eq!(parse("!wordle PLAY Normal", P), Some(Command::Play(Some(GameMode::Normal))));
        assert_eq!(parse("!wordle guess CRANE", P), Some(Command::Guess("CRANE".into())));
        assert_eq!(parse("!wordle board", P), Some(Command::Board));
        assert_eq!(parse("!wordle stats", P), Some(Command::Stats));
        assert_eq!(parse("!wordle leaderboard", P), Some(Command::Leaderboard(LeaderboardSort::Solved)));
        assert_eq!(parse("!wordle leaderboard streak", P), Some(Command::Leaderboard(LeaderboardSort::CurrentStreak)));
        assert_eq!(parse("!wordle mode hard", P), Some(Command::Mode(GameMode::Hard)));
        assert_eq!(parse("!wordle share", P), Some(Command::Share));
        assert_eq!(parse("!wordle community", P), Some(Command::Community));
        assert_eq!(parse("!wordle streak", P), Some(Command::Community));
        assert_eq!(parse("!wordle help", P), Some(Command::Help));
    }

    #[test]
    fn bad_arguments_produce_usage_hints() {
        assert!(matches!(parse("!wordle guess", P), Some(Command::Usage(_))));
        assert!(matches!(parse("!wordle mode", P), Some(Command::Usage(_))));
        assert!(matches!(parse("!wordle mode easy", P), Some(Command::Usage(_))));
        assert!(matches!(parse("!wordle play impossible", P), Some(Command::Usage(_))));
        assert!(matches!(parse("!wordle leaderboard nonsense", P), Some(Command::Usage(_))));
        assert!(matches!(parse("!wordle dance", P), Some(Command::Usage(_))));
    }

    #[test]
    fn bare_words_are_guesses_only_when_five_letters() {
        assert_eq!(bare_guess(" Crane "), Some(Command::Guess("Crane".into())));
        assert_eq!(bare_guess("hello there"), None);
        assert_eq!(bare_guess("hi"), None);
        assert_eq!(bare_guess("cr4ne"), None);
    }

    #[test]
    fn extra_whitespace_is_fine() {
        assert_eq!(parse("!wordle    guess    crane  ", P), Some(Command::Guess("crane".into())));
    }
}
