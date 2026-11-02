-- Bot-wide settings, e.g. the secret that shuffles the bot's own word list.
CREATE TABLE wordle_bot_settings (
    key        text        PRIMARY KEY,
    value      text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
