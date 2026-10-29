# Wordle Bot for Fluxer

A NYT-style daily Wordle bot for a self-hosted [Fluxer](https://fluxer.app) instance, written in Rust (Tokio, Reqwest, Serde, SQLx).

- Everyone gets the official NYT daily word, which rolls over at midnight `America/New_York`.
- Normal and hard mode (NYT hard-mode rules, including repeated letters), six guesses, 🟩🟨⬛ feedback.
- Games start from a server channel and are played privately in DMs, so nobody sees anyone else's guesses.
- Personal stats, spoiler-free sharing, and a per-community leaderboard with **Words Solved**, **Current Streak** and **Longest Streak**.
- Everything is stored in PostgreSQL, so restarts never lose a game and a replayed or duplicated message never counts twice.
- Outbound connections only: no published port and no Nginx Proxy Manager entry.

---

## Commands

**The everyday flow:** type `!wordle play` in a server channel once. The puzzle arrives in your DMs and nothing is posted in the channel. Type your guesses in the DM. When you finish, your spoiler-free grid is posted in the server automatically. On later days, just type a word in the same DM to play that day's puzzle.

| Command | Where | What it does |
|---|---|---|
| `!wordle play` / `!wordle play hard` | server channel or DM | Start or resume today's game (optionally in hard mode, until your first guess). The board is sent by DM. |
| *a five-letter word* (or `!wordle guess CRANE`) | DM | Submit a guess. If you have no game today, one starts automatically for the community you last played from. |
| `!wordle board` | anywhere | Your current board, sent by DM. |
| `!wordle stats` | anywhere | Played, win %, words solved, streaks, guess distribution. |
| `!wordle leaderboard [solved\|streak\|longest]` | server channel | This community's rankings, with the community streak at the top. |
| `!wordle community` (or `!wordle streak`) | server channel | The community streak: days in a row that at least one person here solved the Wordle, and who has solved today. |
| `!wordle mode normal\|hard` | anywhere | Set your preferred mode (reply by DM). |
| `!wordle channel` / `!wordle channel reset` | server channel | **Community owner (or the bot's owner) only.** Post all results in this channel, or go back to posting in the channel where each player last ran `play`. |
| `!wordle help` | anywhere | Command list, sent by DM. |

Private things (boards, help, usage hints) always go to DMs. If a DM can't be delivered, the bot posts a short notice in the channel without any private content, because that's the only way to tell the player. Guesses typed in a public channel are deleted (with Manage Messages) and not counted.

**Daily recap.** Shortly after midnight (America/New_York), the bot replaces the previous day's individual result posts with one compact recap per channel, for example:

> 📅 **Wordle #1,958 results**
> 👥 Community streak: **12 days**
> 👑 **2/6** · Alice
> **3/6** · Bob, Cara\*
> **X/6** · Dave

It also catches up at startup if the bot was offline at midnight. To keep individual posts instead, set `COMPACT_RESULTS=false`.

---|---|---|
| `!wordle play` | server channel | Start or resume today's game (in your preferred mode). The board arrives in your DMs. |
| `!wordle play hard` / `!wordle play normal` | server channel | Start today's game in a specific mode (only until your first guess is accepted). |
| `!wordle guess CRANE` | DM | Submit a guess. In the DM you can also just type `crane`. |
| `!wordle board` | anywhere | Show your current board (always sent to your DM). |
| `!wordle stats` | anywhere | Played, win %, words solved, streaks, guess distribution. |
| `!wordle leaderboard [solved\|streak\|longest]` | server channel | This community's rankings (default: words solved). |
| `!wordle community` (or `!wordle streak`) | server channel | The community streak: days in a row that at least one person here solved the Wordle, and who has solved today. |
| `!wordle mode normal\|hard` | anywhere | Set your preferred mode. It changes today's game only if you haven't guessed yet. |
| `!wordle share` | server channel | Post your finished grid: colours only, no letters. |
| `!wordle help` | anywhere | Command list. |

If someone types a guess in a public channel, the bot deletes it (with Manage Messages) and doesn't count it. If a player's privacy settings block DMs from the bot, it tells them how to fix that and never shows the board publicly.

---

## How it fits together

```
src/
  main.rs            startup: config -> Fluxer connectivity test -> DB + migrations -> gateway loop
  config.rs          environment variables (.env)
  game.rs            pure Wordle rules: scoring, hard mode, dictionary
  streaks.rs         pure streak maths, recomputed from game history
  puzzle.rs          WordProvider trait, NYT provider, PostgreSQL puzzle cache, midnight prefetch
  service.rs         game orchestration in SQL transactions (start, guess, stats, leaderboard, share)
  bot/               command parsing, message rendering, command handling
  fluxer/            Fluxer integration: endpoint discovery, REST client, gateway client, preflight
migrations/          SQL migrations (applied automatically on startup)
scripts/             create/drop database, backup/restore
tests/service.rs     database integration tests
```

### Design decisions

**The Fluxer integration is written directly instead of using a library.** Several community crates exist (`fluxer-rust`, `Fluxer-Rust` on git.gay, `refluxer`, `fluxer-rs`). They are young and change quickly, and their support for custom self-hosted endpoints varies. This bot needs only a handful of calls: discovery, `GET /v1/applications/@me`, opening a DM, sending and deleting a message, plus the gateway handshake, heartbeat and resume. Writing those against Fluxer's published API keeps the self-hosted URLs fully under your control and avoids depending on a crate's release schedule. Discord libraries are not used.

**Fluxer behaviour this code relies on** (checked against docs.fluxer.app):

- Discovery document at `GET {instance}/.well-known/fluxer` (no auth). Bots use `endpoints.api_public` for REST and `endpoints.gateway` for the WebSocket.
- REST is under `{api}/v1/...`, authenticated with `Authorization: Bot <token>`. The token format is `<application_id>.<secret>`.
- Gateway: `?v=1&encoding=json`, Hello (op 10) → Identify (op 2) with the **raw** token (no `Bot ` prefix) → Ready, then heartbeat (op 1/11) and Resume (op 6). Fluxer has **no intents**, so a bot receives message content without any toggle. Close codes 4004 (bad token), 4012 (bad version) and others are reported with a hint.
- Invites: `{webapp}/oauth2/authorize?client_id=<app id>&scope=bot&permissions=<mask>`. Installing a bot needs Manage Community (MANAGE_GUILD) or Administrator.

**Text commands only.** Fluxer slash commands and components are not used, because your installed version may not support them.

**The word provider is replaceable.** The game only talks to the `WordProvider` trait. `NytProvider` is one adapter. If NYT changes or closes the undocumented endpoint, add another provider (say, a local list) and switch it in `main.rs` without touching game code. The response is validated (five lowercase letters, matching `print_date`). Failures are retried with backoff, and players get a friendly "try again shortly" instead of an error.

**Stats come from history.** Every finished game is a row in `wordle_games`. When a game finishes, the player's cached stats (`games_won`, `current_streak`, `longest_streak`, ...) are recomputed from all their finished games in the same transaction, never just incremented. When stats are displayed, a streak whose last win is older than yesterday shows as 0, so a missed day resets the streak without a cron job.

**Concurrency and retries.** Each guess locks that player's game row (`SELECT ... FOR UPDATE`), so simultaneous guesses are applied one at a time and numbered 1-6 with no gaps. Each guess stores the Fluxer message ID under a `UNIQUE` constraint, so a gateway replay or retry of the same message is ignored. `UNIQUE (user_id, puzzle_date)` makes a second game for the same day impossible.

### Streak rules

| Situation | Current streak | Longest streak |
|---|---|---|
| Win today, won yesterday | +1 | max(longest, current) |
| Won yesterday, haven't played today yet | kept | unchanged |
| Lose (6 wrong guesses) | 0 immediately | unchanged |
| Missed a whole day | 0 once that day is over | unchanged |
| Started a game but didn't finish it before midnight | treated as a missed day | unchanged |
| Normal or hard mode win | both count toward the same streak | |

**Community streak.** Each community also has a shared streak: the number of consecutive days on which **at least one person** solved the puzzle from that community. A loss doesn't break it; only a full day with no solves does, and it survives until midnight while today is still open. A win counts for the community the game was started from. It's calculated from game history like personal streaks, and it appears in `!wordle community`, at the top of the leaderboard, in `!wordle share` posts, and in the win message ("you're the first to solve today").

### Database schema

The bot uses **its own database and role** (`wordle_bot`) and never touches Fluxer's tables. Fluxer IDs are 64-bit snowflakes, stored as `BIGINT`.

| Table | Purpose |
|---|---|
| `wordle_daily_puzzles` | One row per date: puzzle number, solution, source, retrieval time. The cache. |
| `wordle_players` | One row per Fluxer user: display name, preferred mode, cached stats, last win/played dates. |
| `wordle_guild_players` | Which communities a player has played from (drives per-community leaderboards). |
| `wordle_games` | One row per player per day: mode, guesses used, status, timestamps. `UNIQUE (user_id, puzzle_date)`. |
| `wordle_guesses` | Every accepted guess with its feedback (`0/1/2` per letter) and the source message ID. |

The puzzle is global, so a player has **one game per day** even if they're in several communities. Their stats appear on the leaderboard of every community they've played from. Migrations in `migrations/` run automatically at startup. The `wordle_bot` role must own its database, which `scripts/create-database.sql` sets up.

---

## Deployment on your Debian server

The steps below assume the project lives at `/mnt/media/appdata/wordle-bot/app` and the bot's persistent files at `/mnt/media/appdata/wordle-bot/{data,backups}`.

### 1. Copy the project to the server

From Windows (PowerShell), for example:

```powershell
scp -r C:\Users\tmyers\Desktop\WordleBot youruser@yourserver:/tmp/WordleBot
```

Then on the server:

```bash
sudo mkdir -p /mnt/media/appdata/wordle-bot/{app,data,backups}
sudo cp -r /tmp/WordleBot/. /mnt/media/appdata/wordle-bot/app/
sudo chown -R "$USER": /mnt/media/appdata/wordle-bot
cd /mnt/media/appdata/wordle-bot/app
```

(Skip the `target/` folder if you built on Windows. It isn't needed.)

### 2. Create the application and bot on *your* instance

Tokens only work on the instance that issued them, so do this at **https://fluxer.squeekienuts.net**, not fluxer.app.

1. Sign in to https://fluxer.squeekienuts.net with your account. The account must be fully registered, not a guest.
2. Open **User Settings → Applications** (the developer section) and create a **New Application**, e.g. "Wordle". Creating an application also creates its bot account. Your instance may ask for a CAPTCHA.
3. On the application's **Bot** section, copy the **bot token**. It is shown **once**; if you lose it, reset it there and the old one stops working immediately. It looks like `1234567890123456789.xxxxxxxx`. The part before the dot is the **application ID** (client ID).
4. Optional: turn off **Public bot** so only you can add it to communities.

Menu names can differ slightly between Fluxer versions. The application and token can also be managed via the API (`POST /v1/oauth2/applications`, `POST /v1/oauth2/applications/{id}/bot/reset-token`).

### 3. Invite the bot to your community

Open this URL while signed in as a user with **Manage Community** (or Administrator), replacing `APP_ID`:

```
https://fluxer.squeekienuts.net/oauth2/authorize?client_id=APP_ID&scope=bot&permissions=27648
```

`27648` = View Channel (1024) + Send Messages (2048) + Manage Messages (8192) + Embed Links (16384).
Use `permissions=19456` to leave out Manage Messages. The bot then can't delete guesses posted publicly; it only asks the player to delete them.

Choose your community and authorize. The bot also prints this exact invite URL in its startup log.

Players need to allow DMs from community members (Fluxer privacy settings) to play.

### 4. Find Fluxer's Docker network and PostgreSQL container

```bash
docker network ls                      # e.g. fluxer_default
docker ps --format '{{.Names}}\t{{.Image}}' | grep -i postgres
docker network inspect fluxer_default --format '{{range .Containers}}{{.Name}} {{end}}'
```

Note the network name and the PostgreSQL container/service name. The bot joins that network so it can reach PostgreSQL by name **without exposing PostgreSQL on the host**.

### 5. Create the bot's database and role

Pick a strong password (letters and digits are simplest, since URL-special characters must be encoded in `DATABASE_URL`). Then run, substituting your container and superuser (often `postgres`, or whatever `POSTGRES_USER` is in Fluxer's compose file):

```bash
docker exec -i <postgres-container> \
  psql -U <superuser> -d postgres -v ON_ERROR_STOP=1 -v wordle_password='YOUR_PASSWORD' \
  < scripts/create-database.sql
```

This creates the role `wordle_bot` and the database `wordle_bot`, and nothing else. It is safe to re-run (re-running also resets the password). The tables are created by the bot on first start.

> If you'd rather not use Fluxer's PostgreSQL at all, skip this step. Set `WORDLE_DB_PASSWORD` in `.env`, set the `DATABASE_URL` host to `wordle-db`, uncomment `depends_on` in `docker-compose.yml`, and start with `docker compose --profile standalone-db up -d`. Its data lives in `/mnt/media/appdata/wordle-bot/postgres`.

### 6. Configure `.env`

```bash
cp .env.example .env
chmod 600 .env
nano .env
```

Set at least:

```ini
FLUXER_BOT_TOKEN=1234567890123456789.your-secret
FLUXER_INSTANCE_URL=https://fluxer.squeekienuts.net
FLUXER_DOCKER_NETWORK=fluxer_default
DATABASE_URL=postgres://wordle_bot:YOUR_PASSWORD@<postgres-container>:5432/wordle_bot
```

**Which Fluxer endpoints to use:**

- **Default (recommended to start):** leave `FLUXER_API_URL` and `FLUXER_GATEWAY_URL` blank. The bot reads `https://fluxer.squeekienuts.net/.well-known/fluxer` and uses the same public API and gateway URLs your browser uses, through your existing reverse proxy. That's outbound only, so no new proxy entry is needed.
- **Internal (optional):** to skip the round trip through Nginx Proxy Manager, point the overrides at Fluxer's containers on the shared network, e.g. `FLUXER_API_URL=http://api:8080` and `FLUXER_GATEWAY_URL=ws://gateway:8080/`. Use the actual service names and ports from your Fluxer compose file. Run `--check` (below) after changing these.

`PUZZLE_TIMEZONE` defaults to `America/New_York`, the NYT rollover.

### 7. Build, test the connection, start

```bash
docker compose build
docker compose run --rm wordle-bot --check   # connectivity test, then exits
docker compose up -d
docker compose logs -f wordle-bot
```

`--check` runs the same startup test the bot runs every time it starts:

1. **Endpoints**: discovery, or your overrides.
2. **REST + token**: `GET /v1/applications/@me` must accept the token. Logs the application name and ID.
3. **Gateway**: Hello → Identify → Ready. Logs the bot user and how many communities it's in, plus the invite URL (with a warning if it's in none).
4. **PostgreSQL**: connects and applies migrations.

A healthy start ends with `gateway session ready` and `daily puzzle cached`. Then type `!wordle help` in a channel the bot can see.

### Restarts, logs and storage

- **Automatic restarts:** `restart: unless-stopped` brings the bot back after crashes and reboots (with Docker enabled at boot: `sudo systemctl enable docker`). If Fluxer is still starting, the bot retries its startup check for about a minute before exiting, and Docker restarts it.
- **Logs:** `docker compose logs -f wordle-bot` (or `--since 1h`). Logs rotate automatically: 5 files × 10 MB. For more detail set `RUST_LOG=debug,sqlx=warn` in `.env` and run `docker compose up -d`.
- **Persistent data:** all game state is in PostgreSQL. `/mnt/media/appdata/wordle-bot/data` is mounted read-only at `/data` for an optional custom dictionary (`WORDS_FILE=/data/words.txt`, one word per line, readable by everyone because the container runs as a non-root user). Backups go to `/mnt/media/appdata/wordle-bot/backups`:

```bash
scripts/backup.sh <postgres-container> <superuser>                # one-off
# daily at 04:15 via `crontab -e`:
15 4 * * * /mnt/media/appdata/wordle-bot/app/scripts/backup.sh <postgres-container> <superuser>
# restore (stop the bot first):
docker compose stop wordle-bot
scripts/restore.sh <postgres-container> /mnt/media/appdata/wordle-bot/backups/<file>.sql.gz <superuser>
docker compose start wordle-bot
```

### Updating

```bash
cd /mnt/media/appdata/wordle-bot/app
# copy in the new source (scp / git pull), then:
docker compose build
docker compose up -d          # recreates the container; unfinished games are kept
docker image prune -f         # optional: remove old image layers
```

New migrations are applied automatically on startup.

### Removing

```bash
docker compose down
docker exec -i <postgres-container> psql -U <superuser> -d postgres < scripts/drop-database.sql   # deletes all Wordle data
```

---

## Troubleshooting

| Log message | Cause and fix |
|---|---|
| `FLUXER_BOT_TOKEN is required` / `does not look like a bot token` | `.env` missing or not loaded, or the token was pasted with `Bot ` in front or truncated. |
| `cannot resolve Fluxer endpoints ... could not reach .../.well-known/fluxer` | DNS or TLS from inside the container, or the proxy doesn't route `/.well-known/fluxer`. Test from the same network with `docker run --rm --network <fluxer-net> curlimages/curl -sS https://fluxer.squeekienuts.net/.well-known/fluxer`. Or set `FLUXER_API_URL` and `FLUXER_GATEWAY_URL` explicitly. |
| `REST check failed ... HTTP 401 INVALID_TOKEN` | Wrong or reset token, or the token came from a different instance (fluxer.app). Copy a fresh one from **your** instance. |
| `REST check failed ... HTTP 404` | `FLUXER_API_URL` points at the wrong path. Public: `https://fluxer.squeekienuts.net/api`. Internal: `http://api:8080`. |
| `gateway ... closed the connection with code 4004` | Gateway rejected the token (see 401 above). |
| `gateway ... HTTP 404/400 instead of upgrading` | Wrong gateway path. Public: `wss://fluxer.squeekienuts.net/gateway`. Internal: `ws://gateway:8080/`. |
| `gateway ... HTTP 502/503/504` | The proxy can't reach Fluxer's gateway container, or WebSocket support is off for that proxy host. |
| `code 4012` | Gateway URL lost `v=1` (the bot adds it; check for a proxy that strips query strings). |
| `the bot is not in any community yet` | Open the logged invite URL and authorize it into your community. |
| `cannot connect to PostgreSQL` | Wrong host (must be the container/service name on the shared network), wrong password, database not created (step 5), or `FLUXER_DOCKER_NETWORK` is wrong. Special characters in the password must be URL-encoded. |
| `database migration failed ... permission denied` | The `wordle_bot` role doesn't own its database. Re-run `scripts/create-database.sql`. |
| `failed to send message ... MISSING_PERMISSIONS` | The bot lacks View Channel, Send Messages or Embed Links in that channel. If only Embed Links is missing, it falls back to plain text automatically. |
| `could not DM player ... CANNOT_SEND_MESSAGES_TO_USER` | That player blocks DMs from community members. They need to change their privacy settings. |
| `could not prefetch today's puzzle` / players see "couldn't be loaded" | NYT unreachable or changed format. The bot retries every 5 minutes; check that the server can reach `https://www.nytimes.com`. |
| Bot doesn't react at all | Check it's online in the member list, that the message starts with `COMMAND_PREFIX` (`!wordle`), and the logs at `RUST_LOG=debug,sqlx=warn`. |

---

## Development and tests

Requires Rust 1.80+.

```bash
cargo test --lib                     # 85 unit tests, no database needed
DATABASE_URL=postgres://user:pass@localhost/wordle_test cargo test   # + 23 database tests
```

The database tests (`tests/service.rs`) create a temporary database per test, so the role needs `CREATEDB`. They use a fake clock and a fake word provider, and cover:

- standard and repeated-letter feedback, plus hard-mode greens, yellows, repeated-letter counts and not banning gray letters
- invalid guesses not consuming attempts; wins, losses, and no replays of a finished day
- mode selection and locking after the first guess
- duplicate message IDs and 13 concurrent submissions (exactly six accepted, numbered 1-6)
- persistence across a simulated restart, puzzle caching (one upstream call), and provider outages
- the 23:59 → 00:00 New York rollover
- streak extension, yesterday's streak kept while today is open, missed-day reset, loss reset, longest-never-decreases, mixed modes, abandoned games
- leaderboard accuracy, sorting, ties, per-community isolation, and display-name changes

Run locally (outside Docker) with the variables from `.env` exported, then `cargo run --release -- --check` or `cargo run --release`.

## Known limitations

- The NYT endpoint is undocumented and could change. The provider abstraction is there for that reason.
- Only today's puzzle is ever fetched, so solutions can't leak early. A day the NYT never publishes can't be played.
- A game left unfinished at midnight closes. It doesn't count as played, but it breaks the streak like a missed day.
- The dictionary is a bundled five-letter list. The day's solution is always accepted even if it's missing from the list.
