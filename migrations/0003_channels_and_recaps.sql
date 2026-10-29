-- Where results are posted, DM-only play on later days, and daily recaps.

-- The server channel a game's result is posted to (if no results channel is set).
ALTER TABLE wordle_games ADD COLUMN channel_id bigint;

-- The community/channel a player last ran `play` from. Lets them start later
-- days straight from their DM.
ALTER TABLE wordle_players
    ADD COLUMN home_guild_id   bigint,
    ADD COLUMN home_channel_id bigint;

-- Per-community settings, e.g. a dedicated Wordle results channel.
CREATE TABLE wordle_guild_settings (
    guild_id           bigint      PRIMARY KEY,
    results_channel_id bigint,
    updated_by         bigint      NOT NULL,
    updated_at         timestamptz NOT NULL DEFAULT now()
);

-- Individual result posts, so they can be replaced by a compact recap later.
CREATE TABLE wordle_result_posts (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    guild_id    bigint      NOT NULL,
    puzzle_date date        NOT NULL,
    channel_id  bigint      NOT NULL,
    message_id  bigint      NOT NULL UNIQUE,
    game_id     bigint      UNIQUE REFERENCES wordle_games (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX wordle_result_posts_day_idx ON wordle_result_posts (guild_id, puzzle_date, channel_id);

-- One recap per community, day and channel; makes the recap job idempotent.
CREATE TABLE wordle_recaps (
    guild_id    bigint      NOT NULL,
    puzzle_date date        NOT NULL,
    channel_id  bigint      NOT NULL,
    message_id  bigint,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, puzzle_date, channel_id)
);
