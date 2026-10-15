//! Pure Wordle rules: feedback, hard-mode validation, dictionary. No I/O.

use std::collections::HashSet;
use std::fmt;

pub const WORD_LEN: usize = 5;
pub const MAX_GUESSES: usize = 6;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tile {
    Absent = 0,
    Present = 1,
    Correct = 2,
}

impl Tile {
    pub fn emoji(self) -> &'static str {
        match self {
            Tile::Correct => "🟩",
            Tile::Present => "🟨",
            Tile::Absent => "⬛",
        }
    }

    fn digit(self) -> char {
        match self {
            Tile::Absent => '0',
            Tile::Present => '1',
            Tile::Correct => '2',
        }
    }

    fn from_digit(c: char) -> Option<Tile> {
        match c {
            '0' => Some(Tile::Absent),
            '1' => Some(Tile::Present),
            '2' => Some(Tile::Correct),
            _ => None,
        }
    }
}

pub type Feedback = [Tile; WORD_LEN];

pub fn feedback_to_string(fb: &Feedback) -> String {
    fb.iter().map(|t| t.digit()).collect()
}

pub fn feedback_from_str(s: &str) -> Option<Feedback> {
    let mut out = [Tile::Absent; WORD_LEN];
    let mut chars = s.chars();
    for slot in out.iter_mut() {
        *slot = Tile::from_digit(chars.next()?)?;
    }
    chars.next().is_none().then_some(out)
}

pub fn feedback_emoji(fb: &Feedback) -> String {
    fb.iter().map(|t| t.emoji()).collect()
}

pub fn is_win(fb: &Feedback) -> bool {
    fb.iter().all(|t| *t == Tile::Correct)
}

/// Standard Wordle scoring: greens first, then yellows are handed out left to
/// right while unmatched solution letters remain. Both inputs must be 5 ASCII
/// lowercase letters.
pub fn evaluate(guess: &str, solution: &str) -> Feedback {
    let g = guess.as_bytes();
    let s = solution.as_bytes();
    debug_assert!(g.len() == WORD_LEN && s.len() == WORD_LEN);

    let mut result = [Tile::Absent; WORD_LEN];
    let mut remaining = [0u8; 26];

    for i in 0..WORD_LEN {
        if g[i] == s[i] {
            result[i] = Tile::Correct;
        } else {
            remaining[(s[i] - b'a') as usize] += 1;
        }
    }
    for i in 0..WORD_LEN {
        if result[i] == Tile::Correct {
            continue;
        }
        let idx = (g[i] - b'a') as usize;
        if remaining[idx] > 0 {
            remaining[idx] -= 1;
            result[i] = Tile::Present;
        }
    }
    result
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum GameMode {
    #[default]
    Normal,
    Hard,
}

impl GameMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GameMode::Normal => "normal",
            GameMode::Hard => "hard",
        }
    }

    pub fn parse(s: &str) -> Option<GameMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(GameMode::Normal),
            "hard" => Some(GameMode::Hard),
            _ => None,
        }
    }
}

impl fmt::Display for GameMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guess {
    pub word: String,
    pub feedback: Feedback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuessFormatError {
    WrongLength,
    NotLetters,
}

impl fmt::Display for GuessFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GuessFormatError::WrongLength => write!(f, "Guesses must be exactly {WORD_LEN} letters."),
            GuessFormatError::NotLetters => write!(f, "Guesses may only contain the letters A-Z."),
        }
    }
}

pub fn normalize_guess(input: &str) -> Result<String, GuessFormatError> {
    let word = input.trim().to_ascii_lowercase();
    if !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(GuessFormatError::NotLetters);
    }
    if word.len() != WORD_LEN {
        return Err(GuessFormatError::WrongLength);
    }
    Ok(word)
}

/// A hard-mode rule the guess broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HardModeViolation {
    WrongPosition { position: usize, letter: char },
    MissingLetter { letter: char },
}

impl fmt::Display for HardModeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HardModeViolation::WrongPosition { position, letter } => {
                let ord = match position {
                    1 => "1st",
                    2 => "2nd",
                    3 => "3rd",
                    4 => "4th",
                    _ => "5th",
                };
                write!(f, "{ord} letter must be {}", letter.to_ascii_uppercase())
            }
            HardModeViolation::MissingLetter { letter } => {
                write!(f, "Guess must contain {}", letter.to_ascii_uppercase())
            }
        }
    }
}

/// NYT hard mode: every revealed green stays in place and every revealed
/// green/yellow letter must appear at least as many times as it has been
/// revealed in any single earlier guess. Gray letters are not banned and
/// yellow letters may be re-used in the same spot (matching NYT behaviour).
pub fn check_hard_mode(previous: &[Guess], guess: &str) -> Result<(), HardModeViolation> {
    let g = guess.as_bytes();
    let mut fixed: [Option<u8>; WORD_LEN] = [None; WORD_LEN];
    let mut required = [0u8; 26];

    for prev in previous {
        let mut seen = [0u8; 26];
        for (i, tile) in prev.feedback.iter().enumerate() {
            let letter = prev.word.as_bytes()[i];
            match tile {
                Tile::Correct => {
                    fixed[i] = Some(letter);
                    seen[(letter - b'a') as usize] += 1;
                }
                Tile::Present => seen[(letter - b'a') as usize] += 1,
                Tile::Absent => {}
            }
        }
        for (r, s) in required.iter_mut().zip(seen) {
            *r = (*r).max(s);
        }
    }

    for (i, want) in fixed.iter().enumerate() {
        if let Some(letter) = want {
            if g[i] != *letter {
                return Err(HardModeViolation::WrongPosition {
                    position: i + 1,
                    letter: *letter as char,
                });
            }
        }
    }

    let mut counts = [0u8; 26];
    for b in g {
        counts[(b - b'a') as usize] += 1;
    }
    for (idx, need) in required.iter().enumerate() {
        if counts[idx] < *need {
            return Err(HardModeViolation::MissingLetter {
                letter: (b'a' + idx as u8) as char,
            });
        }
    }
    Ok(())
}

/// Set of accepted guesses (lowercase, 5 letters).
#[derive(Clone, Debug, Default)]
pub struct Dictionary {
    words: HashSet<String>,
}

impl Dictionary {
    pub fn from_text(text: &str) -> Dictionary {
        let words = text
            .lines()
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|w| w.len() == WORD_LEN && w.bytes().all(|b| b.is_ascii_lowercase()))
            .collect();
        Dictionary { words }
    }

    pub fn contains(&self, word: &str) -> bool {
        self.words.contains(word)
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Tile::*;

    fn fb(s: &str) -> Feedback {
        feedback_from_str(s).unwrap()
    }

    fn guess(word: &str, solution: &str) -> Guess {
        Guess {
            word: word.into(),
            feedback: evaluate(word, solution),
        }
    }

    #[test]
    fn all_correct() {
        assert_eq!(evaluate("crane", "crane"), [Correct; 5]);
        assert!(is_win(&evaluate("crane", "crane")));
    }

    #[test]
    fn all_absent() {
        assert_eq!(evaluate("fuzzy", "crane"), [Absent; 5]);
    }

    #[test]
    fn mixed_feedback() {
        // solution CRANE, guess REACT: R present, E present, A green, C present, T absent
        assert_eq!(evaluate("react", "crane"), fb("11210"));
        assert_eq!(evaluate("crate", "crane"), fb("22202"));
    }

    #[test]
    fn repeated_letter_guess_has_more_than_solution() {
        // solution ABBEY has two Bs; guess BABES: B yellow, A yellow, B green, E green, S absent
        assert_eq!(evaluate("babes", "abbey"), fb("11220"));
        // solution CRANE has a single E; guess GEESE: only the last E is green
        assert_eq!(evaluate("geese", "crane"), fb("00002"));
        // solution LEVEL, guess EERIE: first E yellow... solution has 2 Es, guess 3
        assert_eq!(evaluate("eerie", "level"), fb("12000"));
    }

    #[test]
    fn green_takes_priority_over_earlier_yellow() {
        // solution ROBOT, guess FLOOR: second O is green (pos 4), first O takes the
        // remaining O as yellow, R is yellow.
        assert_eq!(evaluate("floor", "robot"), fb("00121"));
        // solution ALLOY, guess LLAMA: L(0) present, L(1) green, A present, M absent, A absent
        assert_eq!(evaluate("llama", "alloy"), fb("12100"));
    }

    #[test]
    fn repeated_letter_in_solution() {
        // solution SPEED, guess ERASE: E(0) present, R absent, A absent, S present, E(4) present
        assert_eq!(evaluate("erase", "speed"), fb("10011"));
        // solution SPEED, guess EEEEE: only positions 2 and 3 green
        assert_eq!(evaluate("eeeee", "speed"), fb("00220"));
    }

    #[test]
    fn feedback_string_round_trip() {
        let f = fb("20110");
        assert_eq!(feedback_to_string(&f), "20110");
        assert_eq!(feedback_emoji(&f), "🟩⬛🟨🟨⬛");
        assert!(feedback_from_str("2011").is_none());
        assert!(feedback_from_str("201103").is_none());
        assert!(feedback_from_str("20x10").is_none());
    }

    #[test]
    fn normalizes_guesses() {
        assert_eq!(normalize_guess("  CrAnE "), Ok("crane".to_string()));
        assert_eq!(normalize_guess("cran"), Err(GuessFormatError::WrongLength));
        assert_eq!(normalize_guess("cranes"), Err(GuessFormatError::WrongLength));
        assert_eq!(normalize_guess("cr4ne"), Err(GuessFormatError::NotLetters));
        assert_eq!(normalize_guess("crané"), Err(GuessFormatError::NotLetters));
        assert_eq!(normalize_guess(""), Err(GuessFormatError::WrongLength));
    }

    #[test]
    fn hard_mode_accepts_first_guess() {
        assert_eq!(check_hard_mode(&[], "zzzzz"), Ok(()));
    }

    #[test]
    fn hard_mode_requires_greens_in_place() {
        let prev = [guess("crane", "crate")];
        assert_eq!(
            check_hard_mode(&prev, "slate"),
            Err(HardModeViolation::WrongPosition { position: 1, letter: 'c' })
        );
        assert_eq!(check_hard_mode(&prev, "crate"), Ok(()));
    }

    #[test]
    fn hard_mode_requires_yellows_anywhere() {
        // solution TRAIN, guess ARISE -> A,R,I present(yellow/green); S,E absent
        let prev = [guess("arise", "train")];
        // R is at position 2 in ARISE, TRAIN has R at 2 -> green. A yellow, I yellow.
        assert_eq!(
            check_hard_mode(&prev, "xxxxx"),
            Err(HardModeViolation::WrongPosition { position: 2, letter: 'r' })
        );
        assert_eq!(
            check_hard_mode(&prev, "xrxxx"),
            Err(HardModeViolation::MissingLetter { letter: 'a' })
        );
        // yellow letters may stay in the same spot as before (NYT allows this)
        assert_eq!(check_hard_mode(&prev, "arxix"), Ok(()));
        assert_eq!(check_hard_mode(&prev, "train"), Ok(()));
    }

    #[test]
    fn hard_mode_does_not_ban_gray_letters() {
        let prev = [guess("crane", "slate")];
        // C, R, N are gray but reusing them is fine as long as A and E are kept
        // (A is green at pos 3, E green at pos 5)
        assert_eq!(check_hard_mode(&prev, "crate"), Ok(()));
        assert_eq!(check_hard_mode(&prev, "crnae"), Err(HardModeViolation::WrongPosition { position: 3, letter: 'a' }));
    }

    #[test]
    fn hard_mode_repeated_letter_counts() {
        // solution SPEED, guess ERASE reveals E twice (one yellow, one yellow)
        let prev = [guess("erase", "speed")];
        // one E is not enough, needs two (plus S)
        assert_eq!(
            check_hard_mode(&prev, "stone"),
            Err(HardModeViolation::MissingLetter { letter: 'e' })
        );
        assert_eq!(check_hard_mode(&prev, "speed"), Ok(()));
        assert_eq!(check_hard_mode(&prev, "sleet"), Ok(()));
    }

    #[test]
    fn hard_mode_does_not_over_require_when_gray_duplicate() {
        // solution CRANE, guess GEESE: one E revealed (green at pos 5), extra Es gray.
        let prev = [guess("geese", "crane")];
        // must keep E in last position; only ONE E required
        assert_eq!(check_hard_mode(&prev, "crane"), Ok(()));
        assert_eq!(
            check_hard_mode(&prev, "crank"),
            Err(HardModeViolation::WrongPosition { position: 5, letter: 'e' })
        );
    }

    #[test]
    fn hard_mode_combines_multiple_guesses() {
        let solution = "pious";
        let prev = [guess("slate", solution), guess("round", solution)];
        // SLATE: S present. ROUND: O present, U present
        // must include S, O, U
        assert_eq!(
            check_hard_mode(&prev, "mound"),
            Err(HardModeViolation::MissingLetter { letter: 's' })
        );
        assert_eq!(check_hard_mode(&prev, "pious"), Ok(()));
    }

    #[test]
    fn violation_messages() {
        assert_eq!(
            HardModeViolation::WrongPosition { position: 3, letter: 'r' }.to_string(),
            "3rd letter must be R"
        );
        assert_eq!(
            HardModeViolation::MissingLetter { letter: 'e' }.to_string(),
            "Guess must contain E"
        );
    }

    #[test]
    fn dictionary_filters_and_normalizes() {
        let d = Dictionary::from_text("Crane\nslate\nab\ntoolong\nca-ne\n\n");
        assert_eq!(d.len(), 2);
        assert!(d.contains("crane"));
        assert!(d.contains("slate"));
        assert!(!d.contains("ab"));
    }

    #[test]
    fn game_mode_parsing() {
        assert_eq!(GameMode::parse("HARD"), Some(GameMode::Hard));
        assert_eq!(GameMode::parse(" normal "), Some(GameMode::Normal));
        assert_eq!(GameMode::parse("easy"), None);
    }
}
