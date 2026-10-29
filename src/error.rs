use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("today's puzzle is unavailable: {0}")]
    PuzzleUnavailable(String),
    #[error("player has not started a game from any community yet")]
    NoHomeCommunity,
    #[error("{0}")]
    Internal(String),
}
