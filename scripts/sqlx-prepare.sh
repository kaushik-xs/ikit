#!/usr/bin/env bash
# Regenerates the sqlx offline query data in .sqlx/ after a migration or query change.
# Pass --check to verify .sqlx/ is up to date instead (used in CI).
set -euo pipefail

cd "$(dirname "$0")/.."

# A sqlx-cli built with the sqlite driver; installed here if the one on PATH lacks it.
tools="$PWD/target/tools"
if [ ! -x "$tools/bin/cargo-sqlx" ]; then
    cargo install sqlx-cli --version 0.8.6 --no-default-features --features sqlite,rustls --locked --root "$tools"
fi
export PATH="$tools/bin:$PATH"

db_dir="target/sqlx-dev"
mkdir -p "$db_dir"
rm -f "$db_dir"/dev.db*
export DATABASE_URL="sqlite://$PWD/$db_dir/dev.db"

cargo sqlx database create
cargo sqlx migrate run --source crates/ikit-state/migrations/sqlite
cargo sqlx prepare --workspace "$@" -- --all-targets
