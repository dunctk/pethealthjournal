#!/bin/sh
# Starts the app, optionally under Litestream so every SQLite change is
# streamed to S3-compatible storage (Cloudflare R2 for the hosted service).
set -eu

DB=/persistent/pethealth.sqlite

if [ "${LITESTREAM_ENABLED:-false}" != "true" ]; then
  exec /usr/local/bin/pethealth
fi

for var in S3_ENDPOINT S3_BUCKET S3_ACCESS_KEY_ID S3_SECRET_ACCESS_KEY; do
  eval "value=\${$var:-}"
  if [ -z "$value" ]; then
    echo "LITESTREAM_ENABLED=true needs $var" >&2
    exit 1
  fi
done
export S3_REGION="${S3_REGION:-auto}"
export LITESTREAM_PATH="${LITESTREAM_PATH:-db/pethealth}"

# Fresh volume (new server, lost disk): pull the latest replica first.
# An existing database is never overwritten.
litestream restore -config /etc/litestream.yml -if-db-not-exists -if-replica-exists "$DB"

exec litestream replicate -config /etc/litestream.yml -exec /usr/local/bin/pethealth
