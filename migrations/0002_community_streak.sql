-- Speeds up the community streak query (wins per community per day).
CREATE INDEX wordle_games_guild_wins_idx ON wordle_games (guild_id, puzzle_date) WHERE status = 'won';
