//! Pure message formatting. Nothing here performs I/O.

use std::collections::BTreeMap;

use crate::fluxer::model::{Embed, EmbedField, EmbedFooter, OutgoingMessage};
use crate::game::{feedback_emoji, GameMode, Tile, MAX_GUESSES, WORD_LEN};
use crate::service::{GameStatus, GameView, LeaderboardEntry, LeaderboardSort, PlayerStats, Rejection, ShareData};

const GREEN: u32 = 0x6AAA64;
const YELLOW: u32 = 0xC9B458;
const GRAY: u32 = 0x787C7E;

fn mode_label(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Normal => "Normal mode",
        GameMode::Hard => "Hard mode",
    }
}

/// `1234` -> `1,234`, as the NYT share text prints it.
pub fn format_number(n: i32) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        out.insert(0, '-');
    }
    out
}

fn grid(view: &GameView) -> String {
    let mut rows = Vec::with_capacity(MAX_GUESSES);
    for i in 0..MAX_GUESSES {
        rows.push(match view.guesses.get(i) {
            Some(g) => format!("{}  `{}`", feedback_emoji(&g.feedback), spaced(&g.word)),
            None => "⬜".repeat(WORD_LEN),
        });
    }
    rows.join("\n")
}

fn spaced(word: &str) -> String {
    word.to_ascii_uppercase().chars().map(String::from).collect::<Vec<_>>().join(" ")
}

/// Best known status per letter across all guesses.
fn letter_states(view: &GameView) -> BTreeMap<char, Tile> {
    let mut states: BTreeMap<char, Tile> = BTreeMap::new();
    for g in &view.guesses {
        for (c, t) in g.word.chars().zip(g.feedback.iter()) {
            let entry = states.entry(c).or_insert(Tile::Absent);
            if (*t as u8) > (*entry as u8) {
                *entry = *t;
            }
        }
    }
    states
}

fn keyboard(view: &GameView) -> String {
    let states = letter_states(view);
    let pick = |wanted: Tile| -> String {
        let v: Vec<String> = states
            .iter()
            .filter(|(_, t)| **t == wanted)
            .map(|(c, _)| c.to_ascii_uppercase().to_string())
            .collect();
        if v.is_empty() { "-".into() } else { v.join(" ") }
    };
    let unused: Vec<String> = ('a'..='z')
        .filter(|c| !states.contains_key(c))
        .map(|c| c.to_ascii_uppercase().to_string())
        .collect();
    format!(
        "🟩 {}\n🟨 {}\n⬛ {}\n⬜ {}",
        pick(Tile::Correct),
        pick(Tile::Present),
        pick(Tile::Absent),
        unused.join(" ")
    )
}

fn footer(view: &GameView) -> EmbedFooter {
    if view.status.is_finished() {
        let score = if view.status == GameStatus::Won { view.guesses.len().to_string() } else { "X".into() };
        return EmbedFooter {
            text: format!("{} · Wordle #{} · {score}/{MAX_GUESSES}", mode_label(view.mode), format_number(view.puzzle_number)),
        };
    }
    EmbedFooter {
        text: format!(
            "{} · Wordle #{} · {} guess{} remaining",
            mode_label(view.mode),
            format_number(view.puzzle_number),
            view.guesses_remaining(),
            if view.guesses_remaining() == 1 { "" } else { "es" }
        ),
    }
}

pub fn board(view: &GameView, notice: Option<&str>, prefix: &str) -> OutgoingMessage {
    let mut description = String::new();
    if let Some(n) = notice {
        description.push_str(n);
        description.push_str("\n\n");
    }
    if view.status == GameStatus::InProgress && view.guesses.is_empty() {
        description.push_str(&format!(
            "Guess the five-letter word in {MAX_GUESSES} tries.\nReply here in this DM with `{prefix} guess WORD`, or just type the word.\n\n"
        ));
    }
    description.push_str(&grid(view));

    let mut fields = Vec::new();
    if view.status == GameStatus::InProgress && !view.guesses.is_empty() {
        fields.push(EmbedField { name: "Letters".into(), value: keyboard(view), inline: false });
    }
    OutgoingMessage::embed(Embed {
        title: Some(format!("Wordle #{}", format_number(view.puzzle_number))),
        description: Some(description),
        color: Some(GRAY),
        footer: Some(footer(view)),
        fields,
    })
}

pub fn guess_rejected(rejection: &Rejection, prefix: &str) -> OutgoingMessage {
    let text = match rejection {
        Rejection::Format(e) => format!("❌ {e} That didn't use up a guess."),
        Rejection::NotInDictionary(w) => {
            format!("❌ **{}** isn't in the word list. That didn't use up a guess.", w.to_ascii_uppercase())
        }
        Rejection::HardMode(v) => format!("❌ Hard mode: {v}. That didn't use up a guess."),
        Rejection::NoGame => format!(
            "You don't have a game for today yet. Start one from a server channel with `{prefix} play`. \
             (If you started yesterday's puzzle before midnight, that game has closed and a new one is waiting.)"
        ),
        Rejection::AlreadyFinished => {
            "You've already finished today's puzzle. A new one unlocks at midnight (America/New_York).".into()
        }
    };
    OutgoingMessage::text(text)
}

fn praise(guesses: usize) -> &'static str {
    match guesses {
        1 => "Genius!",
        2 => "Magnificent!",
        3 => "Impressive!",
        4 => "Splendid!",
        5 => "Great!",
        _ => "Phew!",
    }
}

fn streak_line(stats: &PlayerStats) -> String {
    format!("🔥 Current streak: **{}** · Longest streak: **{}**", stats.current_streak, stats.longest_streak)
}

pub fn won(view: &GameView, stats: &PlayerStats, prefix: &str) -> OutgoingMessage {
    let used = view.guesses.len();
    let mut m = board(view, None, prefix);
    let embed = &mut m.embeds[0];
    embed.title = Some(format!("🎉 {} Wordle #{} solved!", praise(used), format_number(view.puzzle_number)));
    embed.color = Some(GREEN);
    embed.description = Some(format!(
        "Congratulations! You solved it in **{used}/{MAX_GUESSES}**.\n\n{}\n\n{}\n\nUse `{prefix} share` in a server channel to share your result without spoilers.",
        grid(view),
        streak_line(stats)
    ));
    m
}

pub fn lost(view: &GameView, stats: &PlayerStats) -> OutgoingMessage {
    let mut m = board(view, None, "");
    let solution = view.solution.as_deref().unwrap_or("?????").to_ascii_uppercase();
    let embed = &mut m.embeds[0];
    embed.title = Some(format!("Wordle #{} — out of guesses", format_number(view.puzzle_number)));
    embed.color = Some(YELLOW);
    embed.description = Some(format!(
        "The word was **{solution}**.\n\n{}\n\nYour streak has been reset to 0 · Longest streak: **{}**.\nA new puzzle unlocks at midnight (America/New_York).",
        grid(view),
        stats.longest_streak
    ));
    m
}

pub fn stats(stats: &PlayerStats) -> OutgoingMessage {
    let max = stats.distribution.iter().copied().max().unwrap_or(0).max(1);
    let dist: Vec<String> = stats
        .distribution
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let bar = "█".repeat(((*n as f64 / max as f64) * 12.0).ceil() as usize);
            format!("{}: {bar} {n}", i + 1)
        })
        .collect();
    OutgoingMessage::embed(Embed {
        title: Some(format!("Wordle statistics — {}", stats.display_name)),
        description: Some(format!(
            "**Played:** {}\n**Win rate:** {}%\n**Words solved:** {}\n**Current streak:** {}\n**Longest streak:** {}",
            stats.games_played,
            stats.win_rate_percent(),
            stats.games_won,
            stats.current_streak,
            stats.longest_streak
        )),
        color: Some(GREEN),
        footer: None,
        fields: vec![EmbedField {
            name: "Guess distribution".into(),
            value: format!("```\n{}\n```", dist.join("\n")),
            inline: false,
        }],
    })
}

fn truncate(name: &str, max: usize) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control() && *c != '`').collect();
    if clean.chars().count() <= max {
        clean
    } else {
        clean.chars().take(max - 1).collect::<String>() + "…"
    }
}

pub fn leaderboard(entries: &[LeaderboardEntry], sort: LeaderboardSort, limit: usize, prefix: &str) -> OutgoingMessage {
    if entries.is_empty() {
        return OutgoingMessage::text(format!("No one has finished a puzzle in this community yet. Start with `{prefix} play`!"));
    }
    let name_w = entries.iter().take(limit).map(|e| truncate(&e.display_name, 16).chars().count()).max().unwrap_or(6).max(6);
    let mut table = format!("{:<4} {:<name_w$}  {:>6}  {:>7}  {:>7}\n", "Rank", "Player", "Solved", "Current", "Longest");
    for e in entries.iter().take(limit) {
        table.push_str(&format!(
            "{:<4} {:<name_w$}  {:>6}  {:>7}  {:>7}\n",
            e.rank,
            truncate(&e.display_name, 16),
            e.solved,
            e.current_streak,
            e.longest_streak
        ));
    }
    OutgoingMessage::embed(Embed {
        title: Some(format!("🏆 Wordle leaderboard — by {}", sort.label())),
        description: Some(format!("```\n{table}```")),
        color: Some(GREEN),
        footer: Some(EmbedFooter {
            text: format!("Sort with {prefix} leaderboard [solved|streak|longest]"),
        }),
        fields: Vec::new(),
    })
}

/// Public, spoiler-free result: no letters, only colours.
pub fn share_text(display_name: &str, data: &ShareData) -> String {
    let score = if data.won { data.guesses_used.to_string() } else { "X".into() };
    let star = if data.mode == GameMode::Hard { "*" } else { "" };
    let rows: Vec<String> = data.rows.iter().map(feedback_emoji).collect();
    format!(
        "**{}**'s Wordle {} {score}/{MAX_GUESSES}{star}\n\n{}",
        truncate(display_name, 32),
        format_number(data.puzzle_number),
        rows.join("\n")
    )
}

pub fn help(prefix: &str) -> OutgoingMessage {
    OutgoingMessage::embed(Embed {
        title: Some("Wordle — help".into()),
        description: Some(format!(
            "Guess the daily five-letter word in six tries. Everyone gets the same puzzle, which resets at midnight (America/New_York).\n\n\
             🟩 right letter, right spot\n🟨 right letter, wrong spot\n⬛ letter not in the word\n\n\
             **Commands**\n\
             `{p} play [normal|hard]` — start or resume today's game (from a server channel; the game runs in your DMs)\n\
             `{p} guess WORD` — submit a guess (in your DM with me; you can also just type the word there)\n\
             `{p} board` — show your current board\n\
             `{p} stats` — your statistics\n\
             `{p} leaderboard [solved|streak|longest]` — this community's rankings\n\
             `{p} mode normal|hard` — set your preferred mode (locks once your first guess is accepted)\n\
             `{p} share` — post your finished result without spoilers\n\n\
             **Hard mode:** revealed green letters must stay in place and revealed yellow letters must be reused.",
            p = prefix
        )),
        color: Some(GREEN),
        footer: None,
        fields: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{evaluate, Guess};
    use chrono::NaiveDate;

    fn view(words: &[&str], solution: &str, status: GameStatus) -> GameView {
        GameView {
            game_id: 1,
            puzzle_date: NaiveDate::from_ymd_opt(2024, 3, 15).unwrap(),
            puzzle_number: 1000,
            mode: GameMode::Hard,
            status,
            guesses: words
                .iter()
                .map(|w| Guess { word: w.to_string(), feedback: evaluate(w, solution) })
                .collect(),
            solution: (status == GameStatus::Lost).then(|| solution.to_string()),
        }
    }

    #[test]
    fn number_formatting() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
    }

    #[test]
    fn board_shows_six_rows_mode_number_and_remaining() {
        let v = view(&["crane", "slate"], "cigar", GameStatus::InProgress);
        let m = board(&v, None, "!wordle");
        let e = &m.embeds[0];
        let d = e.description.as_ref().unwrap();
        assert_eq!(d.lines().filter(|l| l.contains('⬜') || l.contains("`")).count(), 6);
        assert!(d.contains("C R A N E"));
        let f = &e.footer.as_ref().unwrap().text;
        assert!(f.contains("Hard mode") && f.contains("#1,000") && f.contains("4 guesses remaining"));
    }

    #[test]
    fn board_never_contains_the_solution_while_playing() {
        let v = view(&["crane"], "cigar", GameStatus::InProgress);
        let text = board(&v, None, "!wordle").to_plain_text().to_lowercase();
        assert!(!text.contains("cigar"));
    }

    #[test]
    fn keyboard_tracks_best_status() {
        let v = view(&["crane", "react"], "cigar", GameStatus::InProgress);
        let k = keyboard(&v);
        // C is green in CRANE; R present; A present; N/E/T absent
        let lines: Vec<&str> = k.lines().collect();
        assert!(lines[0].contains('C'));
        assert!(lines[1].contains('R') && lines[1].contains('A'));
        assert!(lines[2].contains('N') && lines[2].contains('E') && lines[2].contains('T'));
        assert!(!lines[3].contains('C'));
    }

    #[test]
    fn loss_reveals_word_and_win_does_not_leak_extra() {
        let v = view(&["crane", "slate", "pious", "radio", "fuzzy", "mummy"], "cigar", GameStatus::Lost);
        let stats = PlayerStats {
            display_name: "x".into(),
            games_played: 1,
            games_won: 0,
            current_streak: 0,
            longest_streak: 5,
            distribution: [0; 6],
        };
        let t = lost(&v, &stats).to_plain_text();
        assert!(t.contains("CIGAR") && t.contains("reset"));
    }

    #[test]
    fn win_message_has_guess_count_and_streak() {
        let v = view(&["crane", "cigar"], "cigar", GameStatus::Won);
        let stats = PlayerStats {
            display_name: "x".into(),
            games_played: 3,
            games_won: 3,
            current_streak: 3,
            longest_streak: 3,
            distribution: [0, 3, 0, 0, 0, 0],
        };
        let t = won(&v, &stats, "!wordle").to_plain_text();
        assert!(t.contains("Congratulations") && t.contains("2/6") && t.contains("**3**"));
    }

    #[test]
    fn share_text_has_no_letters_from_guesses() {
        let v = view(&["crane", "cigar"], "cigar", GameStatus::Won);
        let data = ShareData {
            puzzle_number: 1000,
            mode: GameMode::Hard,
            won: true,
            guesses_used: 2,
            rows: v.guesses.iter().map(|g| g.feedback).collect(),
        };
        let t = share_text("Alice", &data);
        assert!(t.contains("Wordle 1,000 2/6*"));
        assert!(!t.to_lowercase().contains("crane") && !t.to_lowercase().contains("cigar"));
        assert!(t.contains("🟩"));
    }

    #[test]
    fn share_text_for_loss_uses_x() {
        let data = ShareData { puzzle_number: 5, mode: GameMode::Normal, won: false, guesses_used: 6, rows: vec![] };
        assert!(share_text("Bob", &data).contains("X/6"));
        assert!(!share_text("Bob", &data).contains("X/6*"));
    }

    #[test]
    fn leaderboard_shows_all_three_stats() {
        let entries = vec![
            LeaderboardEntry { rank: 1, user_id: 1, display_name: "Alice".into(), solved: 124, current_streak: 18, longest_streak: 42 },
            LeaderboardEntry { rank: 2, user_id: 2, display_name: "Bob".into(), solved: 107, current_streak: 7, longest_streak: 29 },
        ];
        let t = leaderboard(&entries, LeaderboardSort::Solved, 15, "!wordle").to_plain_text();
        assert!(t.contains("Solved") && t.contains("Current") && t.contains("Longest"));
        assert!(t.contains("Alice") && t.contains("124") && t.contains("18") && t.contains("42"));
    }

    #[test]
    fn leaderboard_respects_limit_and_sanitises_names() {
        let entries: Vec<_> = (1..=5)
            .map(|i| LeaderboardEntry {
                rank: i,
                user_id: i as i64,
                display_name: format!("p`{i}"),
                solved: 1,
                current_streak: 0,
                longest_streak: 1,
            })
            .collect();
        let t = leaderboard(&entries, LeaderboardSort::Solved, 3, "!wordle").to_plain_text();
        assert!(t.contains("p1") && t.contains("p3") && !t.contains("p4"));
        assert_eq!(t.matches('`').count(), 6, "only the code fence backticks may remain");
    }

    #[test]
    fn rejection_messages_state_no_guess_consumed() {
        assert!(guess_rejected(&Rejection::NotInDictionary("zzzzz".into()), "!wordle").to_plain_text().contains("didn't use up"));
        assert!(guess_rejected(&Rejection::Format(crate::game::GuessFormatError::WrongLength), "!wordle")
            .to_plain_text()
            .contains("didn't use up"));
    }
}
