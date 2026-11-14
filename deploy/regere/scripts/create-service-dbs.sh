#!/bin/sh
# ikit:
#   name: create-service-dbs
#   description: Create or update a role and database for every service with DB_NAME, DB_USER and DB_PASSWORD (safe to re-run)
#   runs_on: service:postgres
#   params:
#     PGUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
#   input:
#     csv:
#       columns: [DB_NAME, DB_USER, DB_PASSWORD]
#   timeout: 300
#
# Run it as a post-data hook, so databases exist before the services that use them start:
#   ikit hook add service-dbs --script create-service-dbs --phase post-data
#
# Each row comes from one service's env (registry defaults, instance values, generated
# secrets). The role's password is reset to DB_PASSWORD on every run, so a regenerated
# secret (`ikit svc set <svc> --generate DB_PASSWORD`) takes effect on the next apply.
# Passwords reach psql through the environment (\getenv), never a command line.
set -eu
csv="$1"
rows=0
# The header line is skipped; values never contain commas (ikit refuses those).
tail -n +2 "$csv" | {
  while IFS=, read -r db user pass; do
    [ -n "$db" ] || continue
    DB_NAME="$db" DB_USER="$user" DB_PASSWORD="$pass" \
      psql -X -q -v ON_ERROR_STOP=1 -d postgres <<'SQL'
\getenv db DB_NAME
\getenv usr DB_USER
\getenv pw DB_PASSWORD
SELECT format('CREATE ROLE %I LOGIN', :'usr')
  WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = :'usr') \gexec
SELECT format('ALTER ROLE %I LOGIN PASSWORD %L', :'usr', :'pw') \gexec
SELECT format('CREATE DATABASE %I OWNER %I', :'db', :'usr')
  WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = :'db') \gexec
SELECT format('ALTER DATABASE %I OWNER TO %I', :'db', :'usr') \gexec
SELECT format('GRANT ALL ON SCHEMA public TO %I', :'usr') \gexec
SQL
    # Migrations run as the owner; give it the public schema of its own database too.
    DB_USER="$user" psql -X -q -v ON_ERROR_STOP=1 -d "$db" <<'SQL'
\getenv usr DB_USER
SELECT format('ALTER SCHEMA public OWNER TO %I', :'usr') \gexec
SQL
    echo "ready: database $db owned by $user"
    rows=$((rows + 1))
  done
  echo "$rows database(s) ready"
}
