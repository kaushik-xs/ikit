# Scripts

The script store is an org-level library of shell, Python or SQL scripts that ikit runs against
any workload. A script declares the env vars it reads and where each value comes from. ikit
resolves those values from its own state at run time: targets, workloads, service instances,
published ports and keychain secrets. Scripts therefore carry no hostnames, container names,
passwords or CSV files of their own.

Typical uses:

- **Provisioning**: create a role and database per app service (`create-db.sh`).
- **Seeding**: seed tenants and admins once the stack is up (`init.sh`), as a `post-apply` hook.
- **Operations across workloads**: dump production and restore it into dev
  (`export_cp_dump_from_prod_to_dev.sh`), with `--from` and `--to`.

Scripts complement hooks. A hook belongs to one workload; a script belongs to the org and runs
against any workload, on demand or from a hook.

## How it is structured

| Piece | Where | What it does |
|---|---|---|
| Domain types | `crates/ikit-core/src/script.rs` | `Script`, `ScriptSpec`, `ScriptParam`, `RunsOn`, `Interpreter`, `SourcePath`, `CsvInput`, `ScriptRunRecord`. |
| Hook kind | `crates/ikit-core/src/hook.rs` | `HookAction::Script(ScriptAction { script, params })`. |
| Storage | `crates/ikit-state/migrations/sqlite/0016_scripts.sql`, `src/sqlite/scripts.rs` | Tables `scripts` (body, checksum, spec) and `script_runs` (history). |
| Header parsing | `crates/ikit-app/src/scripts/front_matter.rs` | Reads the `# ikit:` header on import. |
| Resolution | `crates/ikit-app/src/scripts/resolve.rs` | Turns sources into values; builds the CSV input; the base `IKIT_*` env. |
| Execution | `crates/ikit-app/src/scripts/engine.rs` | Builds the program fed to `sh -s`, including SQL variables and the client command for SQL scripts. |
| Use cases | `crates/ikit-app/src/scripts/mod.rs` | `script_add`, `script_preview`, `script_run`, history, the hook step. |
| CLI | `crates/ikit-cli/src/cmd/script.rs` | `ikit script ...`, and `ikit hook add --script` in `cmd/hook.rs`. |

### Storage and sync

`ikit script add` stores the whole file, header included, in the org's `state.db`. The state
syncs with the org's remote, so teammates get the same scripts. The checksum is the sha256 of
the file. Adding the file again reports `added`, `updated` or `unchanged`. `ikit script export`
prints the file back byte for byte.

The `0016` migration changes the state schema. Everyone sharing an org needs an ikit with this
feature before anyone pushes state that contains it.

### What happens on a run

1. **Preview.** ikit loads the script and the workload(s), resolves every param and builds the
   CSV input. It prints all of it with secrets shown as `<secret>`, then asks for confirmation.
2. **Lock and record.** The workload (`--to`, or the selected one) is locked and a run with
   action `script` is started. It shows in `ikit wl runs`.
3. **Execute.** ikit builds one shell program and feeds it on stdin to `sh -s` in the right
   place. The program:
   - writes the body to `scripts/<name>.sh` with a quoted heredoc, so nothing in it is expanded;
   - writes the CSV input to `scripts/<name>.input` the same way;
   - `export`s the base env and the params, each value single-quoted;
   - runs `<interpreter> scripts/<name>.sh <args> [input path] </dev/null`;
   - deletes the input file (and the temp folder, if it used one) and exits with the script's code.
4. **History.** A row goes into `script_runs` with the checksum, the params (secrets masked),
   the exit code and the last 4 KB of output. The run finishes `ok` or `failed`.

Secrets never appear on a command line, so `ps`, shell history and `docker inspect` do not
show them. They exist only in the ssh channel, the script's environment and the CSV file while
the script runs (mode 0600, deleted afterwards). Output is buffered and printed when the script
ends. A long script shows nothing until then.

### Where a script runs

| `runs_on` | Place | How ikit launches it | Folder |
|---|---|---|---|
| `target` | The workload's target host | `ssh <target> 'mkdir -p D && cd D && sh -s'`, or `sh` locally for a local target | The compose project folder `D` (`.ikit/<org>/<workload>` on ssh targets). The body stays in `scripts/` for reference. |
| `service:<name>` | Inside that service's running container | `docker compose -p <workload> exec -T <name> sh -s` (over ssh for ssh targets) | A temp folder, removed afterwards. The workload must be applied. |
| `local` | The machine running ikit | `sh -s` | A temp folder, removed afterwards. Uses your own ssh agent and `~/.ssh/config`. |

SQL scripts always run in a database service, `runs_on: service:<name>`. They run like any
service script, but the last step runs the container's own client instead of an interpreter.
See [SQL scripts](#sql-scripts).

The script's stdin is `/dev/null`. Scripts must not prompt. Pass flags such as `-y` in `args`,
and use `docker compose exec -T` inside scripts.

## Writing a script

Add a `# ikit:` header among the file's leading comments. The header runs through the comment
lines after it whose text is indented, so a regular comment that follows ends it.

```sh
#!/usr/bin/env bash
# ikit:
#   name: create-db
#   description: Create a role and database per app service
#   runs_on: target
#   params:
#     PGCONTAINER: service.postgres.container
#     PGSUPERUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
#     PGSUPERPASS: service.postgres.env.POSTGRES_PASSWORD
#   input:
#     csv:
#       columns: [DB_NAME, DB_USER, DB_PASSWORD]
#       services: app
#   args: ["-y"]
#   timeout: 600
#
# Usage: create-db.sh [-y] file.csv   <- a normal comment ends the header
```

| Key | Meaning |
|---|---|
| `name` | A slug. Required here or via `--name`. |
| `description` | Shown in `script list` and `script show`. |
| `runs_on` | `target`, `local` or `service:<name>`. Required here or via `--runs-on`. |
| `interpreter` | `sh`, `bash`, `python3` or `sql`. Defaults to `sql` for `.sql` files and `python3` for `.py` files, else to what the `#!` line says, else `sh`. |
| `database` | SQL scripts only: the database to run in. Defaults to the one the service's env configures. |
| `transaction` | SQL scripts only: `true` runs the whole script in one transaction. Default `false`. |
| `params` | Env var name to source. See below. |
| `input.csv` | A CSV built from the services' env. See below. |
| `args` | Arguments always passed, before any given at run time. |
| `timeout` | Seconds, default 600, at most 86400. `--timeout` overrides it. |

Unknown keys are rejected at import, so typos fail early. Files with Windows line endings are
refused.

### Params

Each param maps an env var name the script already reads to where its value comes from:

```yaml
params:
  PGSUPERPASS: service.postgres.env.POSTGRES_PASSWORD        # shorthand for { from: ... }
  PGSUPERUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
  LAUNCHPAD_PORT: { from: service.launchpad.port.3000, required: false }
  SEED_TENANT: { value: siemens_energy }                     # a fixed value
  API_TOKEN: { value: abc, secret: true }                    # masked in previews and history
```

- A missing value uses `default`. Without a default, a `required` param (the default) fails the
  run before anything executes, and an optional one is left unset.
- Values from a service's secret env are always masked. `secret: true` masks any other param.
- Names must be valid env names and must not start with `IKIT_`, which ikit sets itself.

### Sources

| Source | Value |
|---|---|
| `org`, `workload`, `env` | The org name, the workload name, its env (`dev`, `prod`, ...). |
| `phase`, `run_id` | The hook phase (empty on demand) and the run id. |
| `target.name` | The target's name. |
| `target.host` | The ssh host, or `127.0.0.1` for a local target. |
| `target.user`, `target.port` | The ssh user (unset if none) and port (22 by default). |
| `target.ssh` | `[user@]host`, for `ssh` and `scp`. Errors for a local target. |
| `target.ssh_args` | The options ikit passes to ssh, e.g. `-o BatchMode=yes -o ConnectTimeout=10 -p 2222`. Use it unquoted: `ssh $ARGS "$DEST"`. |
| `target.dir` | The workload's compose project folder on the target. |
| `service.<svc>.container` | `<workload>-<svc>-1`, the Compose container name. |
| `service.<svc>.host` | `<svc>`, its DNS name inside the workload's networks. |
| `service.<svc>.image`, `.version` | Its image and resolved version. |
| `service.<svc>.env.<VAR>` | Its env value: registry default, instance value or keychain secret. `from:` references in the registry are not resolved, so read the underlying var. |
| `service.<svc>.port.<port>` | The host port its container port is published on (unset if unpublished). |

Prefix any source except `org`, `phase` and `run_id` with `from.` to read the `--from`
workload. `to.` and no prefix both mean the workload the script runs against. An unknown
service, or `from.` without `--from`, is an error.

### Base env

Every script gets `IKIT_ORG`, `IKIT_WORKLOAD`, `IKIT_ENV`, `IKIT_TARGET`, `IKIT_PHASE`,
`IKIT_RUN_ID`, `IKIT_SCRIPT` and `IKIT_SCRIPT_CHECKSUM`. With an input it gets `IKIT_INPUT`, the
CSV file's path. With `--from` it also gets `IKIT_FROM_WORKLOAD`, `IKIT_FROM_SSH`,
`IKIT_FROM_SSH_ARGS`, `IKIT_FROM_DIR` and the same four `IKIT_TO_*` vars, where the target has
them.

### CSV input

```yaml
input:
  csv:
    header: db_name,db_user,db_password   # the default
    columns: [DB_NAME, DB_USER, DB_PASSWORD]
    services: app                          # optional: app, platform or data
    arg: true                              # also pass the path as the last argument (default)
```

Each service of the workload (in the group, if given) with a non-empty value for every column
becomes one row, sorted by service name. Services missing a column are listed as skipped in the
preview. A value containing a comma, quote or newline fails the run. `ikit script run --csv FILE`
replaces the generated rows with a file of your own.

## SQL scripts

A `.sql` file is a script of its own kind. It runs inside a Postgres or MySQL service of the
workload, with that container's client and credentials, the same way `--via exec` SQL hooks
do. No database port needs to be published, and no password leaves the container's env.

```sql
-- ikit:
--   name: app-role
--   description: Create the app's role and database
--   runs_on: service:postgres
--   database: postgres
--   params:
--     APP_USER: service.core.env.DB_USER
--     APP_PASSWORD: service.core.env.DB_PASSWORD
CREATE ROLE :"APP_USER" LOGIN PASSWORD :'APP_PASSWORD';
CREATE DATABASE :"APP_USER" OWNER :"APP_USER";
```

- **Header.** It uses `--` comments, with the same keys as `#` headers. `runs_on` must be
  `service:<name>`, and the service must be a database ikit knows: `postgres`, `postgresql`,
  `timescaledb`, `postgis`, `mysql` or `mariadb`, by registry name. SQL scripts take no
  `input.csv` and no `args`.
- **Params become SQL variables.** Each param and each `IKIT_*` value is set before the script
  runs. Postgres uses psql variables: `:'NAME'` is a quoted literal, `:"NAME"` a quoted
  identifier, and `:NAME` the raw text. MySQL uses user variables: `@NAME`. Quoting is done by
  ikit, so values with quotes, backslashes or newlines arrive intact.
- **Secrets.** Variables are written to a private file in a temp folder in the container,
  read before the script, and deleted afterwards. They never appear on a command line or as
  env vars.
- **Database.** The client connects to `database` if the header sets it, else to the one the
  container's env configures (`POSTGRES_DB`, or `MYSQL_DATABASE`). `ikit script run --database
  NAME` overrides both for one run.
- **Errors.** Postgres runs with `ON_ERROR_STOP=1`, and MySQL stops at the first error by
  default. The first failing statement fails the run.
- **Transactions.** `transaction: true` runs the whole script in one transaction
  (`psql --single-transaction`, or `START TRANSACTION`/`COMMIT` for MySQL), so a failure rolls
  everything back. Leave it off for statements that cannot run in a transaction, such as
  Postgres `CREATE DATABASE`. MySQL commits DDL implicitly either way.
- **Runs every time.** Unlike SQL hooks, a SQL script has no ledger. It runs whenever it is
  run, so write it to be safe to repeat (`IF NOT EXISTS`, `ON CONFLICT`). Use SQL hooks for
  migrations that must run once per database.
- **Output.** Query results are printed like any script output: `\pset`, `\echo` and other
  psql commands work.

## Commands

```bash
ikit script add FILE [--name N] [--runs-on WHERE] [--interpreter I] [--description D] [--param NAME=VALUE|NAME=@source]...
```

```bash
ikit script list
```

```bash
ikit script show NAME [--body]
```

```bash
ikit script export NAME > file.sh
```

```bash
ikit script run NAME [--to WL] [--from WL] [--param K=V|K=@source]... [--csv FILE] [--database DB] [--timeout SECS] [--dry-run] [-y] [-- ARGS...]
```

```bash
ikit script history [NAME] [--limit N]
```

```bash
ikit script rm NAME [-y]
```

```bash
ikit hook add HOOK --script NAME [--param K=V|K=@source]... [--phase pre-apply|post-apply|pre-destroy]
```

- **`--param`** on the command line takes a plain value, or `@source` to resolve one. It adds a
  param or replaces a declared one for that run (or that hook).
- **`run`** prints the preview and asks before running. Without a terminal it needs `-y`.
  `--dry-run` only prints the preview and needs no lock. `-o json` prints the preview or report.
  A script that exits non-zero makes `ikit` exit non-zero, after printing its output.
- **`history`** shows every workload's runs, or only `--wl`'s, including runs where it was the
  `--from` side.
- **`rm`** is refused while a hook runs the script.

### As a hook

A `script` hook runs in its phase like a shell hook. It fails the apply on error unless
`--warn-only` is given. In a first deploy's `pre-apply`, a `service:` script is skipped because
nothing runs yet. A hook cannot give `--from`, so scripts that read `from.` sources cannot be
hooks. The run shows in both `ikit hook history` (step `script:<name>`) and `ikit script history`
(with the hook's name).

## Examples

### create-db.sh

Add the header shown in [Writing a script](#writing-a-script) to
`yotta-infra/scripts/database/create-db.sh`. The body stays unchanged: it already reads
`PGCONTAINER`, `PGSUPERUSER` and `PGSUPERPASS`, takes `-y`, and takes the CSV path as its last
argument.

```bash
ikit script add scripts/database/create-db.sh
```

```bash
ikit script run create-db --dry-run
```

```bash
ikit script run create-db
```

The rows come from app services that carry `DB_NAME`, `DB_USER` and `DB_PASSWORD` (secret) in
their env. Services that only have a `DATABASE_URL` are skipped. Until the registry declares
those vars, pass the existing template:

```bash
ikit script run create-db --csv scripts/database/db-template.new-app-dev-platform.csv
```

### init.sh as a post-apply hook

Two changes make `new-app-dev/init.sh` portable. Compose commands use the workload's project,
and passwords come from params instead of the file:

```sh
#!/usr/bin/env bash
# ikit:
#   name: seed-app
#   runs_on: target
#   params:
#     SEED_ADMIN_PASSWORD: service.authrs.env.SEED_ADMIN_PASSWORD
#     CORE_DATABASE_URL: service.core.env.DATABASE_URL
set -euo pipefail
dc() { docker compose -p "$IKIT_WORKLOAD" -f compose.yaml -f secrets.compose.yaml "$@"; }
dc exec -T -e SEED_SERVICES='[...]' launchpad /app/seed
dc exec -T -e SEED_TENANT_ID=builder -e SEED_ADMIN_PASSWORD="$SEED_ADMIN_PASSWORD" authrs /app/seed
dc exec -T -e SEED_TENANT_ID=siemens_energy -e SEED_TENANT_DATABASE_URL="$CORE_DATABASE_URL" core /app/seed
```

```bash
ikit script add new-app-dev/init.sh
```

```bash
ikit hook add seed --script seed-app --phase post-apply
```

`SEED_ADMIN_PASSWORD` must be a secret env var of the `authrs` registry service for the source to
resolve.

### Production dump into dev

`export_cp_dump_from_prod_to_dev.sh` runs on your machine and talks to two hosts. Replace its
hardcoded hosts and paths with params:

```sh
#!/usr/bin/env bash
# ikit:
#   name: copy-prod-dump
#   runs_on: local
#   timeout: 3600
#   params:
#     SRC_SSH: from.target.ssh
#     SRC_SSH_ARGS: from.target.ssh_args
#     DEST_SSH: to.target.ssh
#     DEST_SSH_ARGS: to.target.ssh_args
#     DEST_DIR: to.target.dir
set -euo pipefail
ssh $SRC_SSH_ARGS "$SRC_SSH" 'cd ~ && bash prod-bkp.sh'
OUTDIR=$(ssh $SRC_SSH_ARGS "$SRC_SSH" 'ls -1dt ~/prod-backups/*/ | head -1')
scp -3 -r "$SRC_SSH:$OUTDIR" "$DEST_SSH:$DEST_DIR/dumps/"
ssh $DEST_SSH_ARGS "$DEST_SSH" "cd $DEST_DIR && ./tenant-restore.sh dumps/$(basename "$OUTDIR") $* -y"
```

```bash
ikit script run copy-prod-dump --from wl-regere-prod-001 --to wl-regere-dev-001 -- -g platform
```

The run is recorded on the dev workload, which stays locked while it runs. The prod workload is
only read.

### Walkthrough: databases for wl-regere-dev-001

These two scripts were run against `wl-regere-dev-001`'s postgres. In that workload, `authrs`,
`core`, `launchpad` and `workflow-engine` each keep their credentials in a secret
`DATABASE_URL`. They all connect as `postgres`, each to its own database.

`app-dbs.sql` reads the four URLs from the org's secrets and creates any role or database that
is missing. It is safe to re-run:

```sql
-- ikit:
--   name: app-dbs
--   description: Create each app's role and database from its DATABASE_URL (safe to re-run)
--   runs_on: service:postgres
--   database: postgres
--   params:
--     AUTHRS_URL: service.authrs.env.DATABASE_URL
--     CORE_URL: service.core.env.DATABASE_URL
--     LAUNCHPAD_URL: service.launchpad.env.DATABASE_URL
--     WORKFLOW_URL: service.workflow-engine.env.DATABASE_URL
--
-- One row per app: the user, password and database in its connection URL.
CREATE TEMP TABLE app_urls AS
SELECT app, m[1] AS usr, m[2] AS pwd, m[3] AS db
FROM (VALUES ('authrs', :'AUTHRS_URL'), ('core', :'CORE_URL'),
             ('launchpad', :'LAUNCHPAD_URL'), ('workflow-engine', :'WORKFLOW_URL')) AS u(app, url),
     regexp_match(url, '^postgres(?:ql)?://([^:]+):([^@]+)@[^/]+/([^?]+)') AS m;

SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', usr, pwd)
FROM app_urls WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = usr)
GROUP BY usr, pwd \gexec
SELECT format('CREATE DATABASE %I OWNER %I', db, min(usr))
FROM app_urls WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = db)
GROUP BY db \gexec

\pset footer off
SELECT app, usr AS role, db AS database FROM app_urls ORDER BY app;
```

`check-app-logins.sh` runs inside the postgres container and logs in with each URL:

```sh
#!/bin/sh
# ikit:
#   name: check-app-logins
#   description: Log in with each app's DATABASE_URL from inside postgres
#   runs_on: service:postgres
#   params:
#     AUTHRS_URL: service.authrs.env.DATABASE_URL
#     CORE_URL: service.core.env.DATABASE_URL
#     LAUNCHPAD_URL: service.launchpad.env.DATABASE_URL
#     WORKFLOW_URL: service.workflow-engine.env.DATABASE_URL
fail=0
for pair in "authrs AUTHRS_URL" "core CORE_URL" "launchpad LAUNCHPAD_URL" "workflow-engine WORKFLOW_URL"; do
  set -- $pair
  url=$(eval echo "\$$2")
  if out=$(psql "$url" -XAtc "select current_user || ' @ ' || current_database()" 2>&1); then
    echo "ok    $1: $out"
  else
    echo "FAIL  $1: $(echo "$out" | tail -1)"; fail=1
  fi
done
exit $fail
```

The commands, in order:

```bash
ikit wl use wl-regere-dev-001
```

```bash
ikit script add app-dbs.sql
```

```bash
ikit script add check-app-logins.sh
```

```bash
ikit script run app-dbs --dry-run
```

```bash
ikit script run app-dbs
```

```bash
ikit script run check-app-logins
```

```bash
ikit hook add app-dbs --script app-dbs --phase post-apply
```

```bash
ikit script history
```

The dry-run shows the four URLs as `<secret>`. The run prints one row per app (app, role,
database), and the check prints `ok  core: postgres @ core` and so on. As a post-apply hook,
`app-dbs` makes sure every app's database exists after each deploy.

## Limits

- The container name assumes Compose's `<workload>-<service>-1`. It is wrong for scaled
  services.
- Output appears only when the script ends. Long scripts need a larger `timeout`.
- A `script run` that changes state holds the org's remote lock while the script runs, like
  `hook run` and `apply`.
- Kubernetes targets are not supported.
- SQL scripts have no ledger: they run every time, unlike SQL hooks.
- Not yet built: MCP tools for scripts, streaming output, and params composed from several
  sources (for example a URL built from user, password and host).
