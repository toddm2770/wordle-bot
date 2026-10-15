-- Creates the Wordle bot's own role and database inside your existing PostgreSQL.
-- It does NOT touch Fluxer's databases, roles or tables.
--
-- Run it as a PostgreSQL superuser, passing the password as a psql variable:
--
--   docker exec -i <postgres-container> \
--     psql -U <superuser> -d postgres -v ON_ERROR_STOP=1 -v wordle_password='change-me' \
--     < scripts/create-database.sql
--
-- Safe to re-run: it only creates what is missing and resets the password.
-- The bot creates its tables itself on startup (SQL migrations in ./migrations).

SELECT format('CREATE ROLE wordle_bot LOGIN PASSWORD %L', :'wordle_password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'wordle_bot')
\gexec

SELECT format('ALTER ROLE wordle_bot WITH LOGIN PASSWORD %L', :'wordle_password')
\gexec

SELECT 'CREATE DATABASE wordle_bot OWNER wordle_bot ENCODING ''UTF8'' TEMPLATE template0'
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'wordle_bot')
\gexec

-- Only the bot's role (and superusers) may connect to the bot's database.
REVOKE ALL ON DATABASE wordle_bot FROM PUBLIC;
GRANT CONNECT, TEMPORARY ON DATABASE wordle_bot TO wordle_bot;

\connect wordle_bot
-- PostgreSQL 15+ already restricts the public schema; this makes older versions match.
ALTER SCHEMA public OWNER TO wordle_bot;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

\echo 'wordle_bot role and database are ready.'
