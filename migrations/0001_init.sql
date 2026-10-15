-- Wordle bot schema. Lives in its own database; never touches Fluxer's tables.
-- Fluxer IDs are 64-bit snowflakes with bit 63 always zero, so BIGINT is safe.

CREATE TABLE wordle_daily_puzzles (
    puzzle_date   date        PRIMARY KEY,
    puzzle_number integer     NOT NULL,
    solution      text        NOT NULL CHECK (solution ~ '^[a-z]{5}$'),
    source        text        NOT NULL,
    retrieved_at  timestamptz NOT NULL DEFAULT now()
);

-- One row per Fluxer account. Stats columns are a cache that is recomputed from
-- wordle_games inside the transaction that finishes a game (never incremented blindly).
CREATE TABLE wordle_players (
    user_id          bigint      PRIMARY KEY,
    display_name     text        NOT NULL,
    preferred_mode   text        NOT NULL DEFAULT 'normal' CHECK (preferred_mode IN ('normal', 'hard')),
    games_played     integer     NOT NULL DEFAULT 0 CHECK (games_played >= 0),
    games_won        integer     NOT NULL DEFAULT 0 CHECK (games_won >= 0),
    current_streak   integer     NOT NULL DEFAULT 0 CHECK (current_streak >= 0),
    longest_streak   integer     NOT NULL DEFAULT 0 CHECK (longest_streak >= 0),
    last_win_date    date,
    last_played_date date,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CHECK (games_won <= games_played)
);

-- Which guild leaderboards a player appears on. The puzzle is global, so a
-- player has one game per day regardless of how many guilds they belong to.
CREATE TABLE wordle_guild_players (
    guild_id  bigint      NOT NULL,
    user_id   bigint      NOT NULL REFERENCES wordle_players (user_id) ON DELETE CASCADE,
    joined_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX wordle_guild_players_user_idx ON wordle_guild_players (user_id);

CREATE TABLE wordle_games (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id       bigint      NOT NULL REFERENCES wordle_players (user_id) ON DELETE CASCADE,
    guild_id      bigint,
    puzzle_date   date        NOT NULL REFERENCES wordle_daily_puzzles (puzzle_date),
    puzzle_number integer     NOT NULL,
    mode          text        NOT NULL CHECK (mode IN ('normal', 'hard')),
    guesses_used  smallint    NOT NULL DEFAULT 0 CHECK (guesses_used BETWEEN 0 AND 6),
    status        text        NOT NULL DEFAULT 'in_progress' CHECK (status IN ('in_progress', 'won', 'lost')),
    started_at    timestamptz NOT NULL DEFAULT now(),
    completed_at  timestamptz,
    UNIQUE (user_id, puzzle_date),
    CHECK ((status = 'in_progress') = (completed_at IS NULL)),
    CHECK (status <> 'lost' OR guesses_used = 6)
);
CREATE INDEX wordle_games_user_finished_idx ON wordle_games (user_id, puzzle_date) WHERE status <> 'in_progress';

CREATE TABLE wordle_guesses (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    game_id           bigint      NOT NULL REFERENCES wordle_games (id) ON DELETE CASCADE,
    guess_number      smallint    NOT NULL CHECK (guess_number BETWEEN 1 AND 6),
    word              text        NOT NULL CHECK (word ~ '^[a-z]{5}$'),
    -- one digit per letter: 0 absent, 1 present (yellow), 2 correct (green)
    feedback          text        NOT NULL CHECK (feedback ~ '^[012]{5}$'),
    -- Fluxer message that carried the guess; makes gateway replays/retries idempotent
    source_message_id bigint UNIQUE,
    created_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (game_id, guess_number)
);
