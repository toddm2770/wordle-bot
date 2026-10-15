pub mod bot;
pub mod config;
pub mod error;
pub mod fluxer;
pub mod game;
pub mod puzzle;
pub mod service;
pub mod streaks;

/// Bundled five-letter dictionary (override at runtime with WORDS_FILE).
pub const DEFAULT_WORDS: &str = include_str!("../data/words.txt");

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
