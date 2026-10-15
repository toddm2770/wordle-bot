-- Removes the Wordle bot's database and role (ALL Wordle history is deleted).
-- Stop the bot first. Run as a superuser:
--   docker exec -i <postgres-container> psql -U <superuser> -d postgres -v ON_ERROR_STOP=1 < scripts/drop-database.sql
DROP DATABASE IF EXISTS wordle_bot WITH (FORCE);
DROP ROLE IF EXISTS wordle_bot;
