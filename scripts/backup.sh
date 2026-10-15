#!/usr/bin/env bash
# Dump the Wordle database to /mnt/media/appdata/wordle-bot/backups (keeps 30 days).
# Usage: scripts/backup.sh <postgres-container> [superuser]
# Cron example (daily 04:15):
#   15 4 * * * /path/to/WordleBot/scripts/backup.sh <postgres-container> postgres
set -euo pipefail

CONTAINER="${1:?usage: backup.sh <postgres-container> [superuser]}"
PGUSER_NAME="${2:-postgres}"
DEST="${WORDLE_BACKUP_DIR:-/mnt/media/appdata/wordle-bot/backups}"

mkdir -p "$DEST"
FILE="$DEST/wordle_bot-$(date +%Y%m%d-%H%M%S).sql.gz"
docker exec "$CONTAINER" pg_dump -U "$PGUSER_NAME" --no-owner --clean --if-exists wordle_bot | gzip > "$FILE"
echo "wrote $FILE"
find "$DEST" -name 'wordle_bot-*.sql.gz' -mtime +30 -delete
