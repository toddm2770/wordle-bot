#!/usr/bin/env bash
# Restore a dump made by backup.sh. Stop the bot first (docker compose stop wordle-bot).
# Usage: scripts/restore.sh <postgres-container> <backup.sql.gz> [superuser]
set -euo pipefail
CONTAINER="${1:?usage: restore.sh <postgres-container> <file.sql.gz> [superuser]}"
FILE="${2:?missing backup file}"
PGUSER_NAME="${3:-postgres}"
gunzip -c "$FILE" | docker exec -i "$CONTAINER" psql -U "$PGUSER_NAME" -d wordle_bot -v ON_ERROR_STOP=1
echo "restored $FILE"
