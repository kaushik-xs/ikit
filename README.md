# ikit

Infrastructure Kit: a Rust CLI to create, update, destroy and manage infrastructure components:
Docker Compose and Kubernetes workloads, cloud blob storage, database backups, idempotent SQL hooks,
ZeroTier networking, and an MCP server for agents.

See [docs/ikit-implementation-plan.pdf](docs/ikit-implementation-plan.pdf) for the full plan.
Done so far: Phase 0 (workspace, state store, `ikit init`, provider profiles), Phase 1 (service
registry, attached targets, workloads with service groups, orgs) and Phase 2 (Docker Compose
deploys to local and SSH targets, registry logins, an MCP server) Phase 3 (SQL, shell and HTTP
hooks; MCP tools that change definitions, gated per env), Phase 4 (blob storage, database
backups, digest pinning, registry sync, MCP deploys behind plan IDs and confirmation tokens) and
Phase 7 (drift detection, `registry outdated`, MCP hooks and prompts, CLI integration tests, a
release workflow). Phase 6 (ZeroTier) is in progress: networks, members, joins and MCP over HTTP
are done; the Compose sidecar and VM targets managed over ZeroTier IPs are not yet. Phases 5 and
6b (Kubernetes, creating targets) are still open.

## Usage

```bash
ikit init                                          # creates ~/.ikit; re-run after upgrading ikit
ikit org create acme --display-name "Acme Corp"    # own state.db; registry seeded with postgres, redis, ...
ikit org create globex --prefix gx --envs dev,uat,prod
ikit org use acme                                  # or --org globex / IKIT_ORG=globex per command

ikit registry list --category data
ikit registry show postgres
ikit registry add orders-api --category app --version 1.4.2 \
  --image ghcr.io/acme/orders-api:1.4.2 --port 8080:http \
  --env LOG_LEVEL=info --secret-env STRIPE_KEY --depends-on 'postgres@^16'
ikit registry version add orders-api 1.5.0 --image ghcr.io/acme/orders-api:1.5.0
ikit registry import registry.yaml [--replace]     # see crates/ikit-app/seeds/registry.yaml for the format
ikit registry export -f registry.yaml

ikit target attach vm laptop --local               # or --ssh ubuntu@10.147.17.5
ikit target attach k8s kind --context kind-kind
ikit target test laptop

ikit wl create --env dev --target laptop           # creates and selects wl-acme-dev-001
ikit wl list --all-orgs
ikit wl use gx-uat-001                             # also switches to org globex
ikit sg add data                                   # interactive picker
ikit sg add app --services 'orders-api@^1' --with-deps --set orders-api.LOG_LEVEL=debug
ikit svc upgrade postgres --to '^17'
ikit svc publish orders-api 8080:8080             # host:container
ikit wl show

ikit registry login ghcr.io --username octocat     # token from a prompt or IKIT_REGISTRY_PASSWORD
ikit render                                        # the compose file (secret values not shown)
ikit plan                                          # what apply would change on the target
ikit apply                                         # docker compose up --wait, locally or over SSH
ikit status
ikit status --drift [--all]                        # containers vs the applied files; exits 2 on drift
ikit apply --force                                 # re-apply with no plan changes (repairs drift)
ikit wl runs                                       # apply/destroy history
ikit destroy [--volumes]                           # removes containers; --volumes deletes data

ikit hook add schema --service postgres --source db/migrations   # SQL: a .sql file or a folder
ikit hook add schema --service postgres --source db/migrations --via direct   # over the published port
ikit hook add smoke --shell 'curl -fs localhost:8080/health' --service web
ikit hook add drain --phase pre-destroy --shell './drain.sh'      # on the target host
ikit hook add slack --http "POST https://hooks.slack.com/services/..." --body '{"text":"{{workload}} deployed"}'
ikit hook add ticket --mcp "npx -y @acme/mcp-server" --tool create_note \
  --args '{"text":"{{workload}} deployed"}' --secret-env ACME_TOKEN   # MCP tool call
ikit hook list | run [name] [--allow-changed] | history | rm <name>
ikit profile set aws acme --set region=eu-central-1 --secret secret_access_key

ikit storage add s3 --provider s3 --bucket backups --profile acme       # or azure | gcs | local
ikit storage add minio --provider s3 --bucket backups --endpoint http://localhost:9000 --profile local
ikit storage test s3
ikit backup run postgres [--storage s3] [--keep 7]
ikit backup list | restore <id> | prune postgres --keep 7 | schedule postgres --cron "0 2 * * *"

ikit registry version pin postgres 16.6            # deploy as image@sha256:… (required for prod)
ikit registry sync postgres [--add]                # newer version tags from the image registry
ikit registry outdated [--current]                 # services behind the registry; exits 2 if any

ikit mcp install claude                            # registers `ikit --org <org> mcp serve` with Claude Code
ikit mcp token create laptop                       # bearer token for HTTP, shown once
ikit mcp serve --http zt:7420                      # streamable HTTP on this machine's ZeroTier address
ikit mcp serve --http 127.0.0.1:7420               # or loopback only
ikit mcp token list | rm <name>

ikit profile add zerotier zt                       # prompts for the Central API token (or IKIT_SECRET_API_TOKEN)
ikit net zt create dev-net --profile zt            # private network, links the selected workload
ikit net zt attach shared 8056c2e21c000001 --profile zt   # an existing network
ikit net zt join dev-net --target box --authorize  # zerotier-cli join over SSH, then authorize
ikit net zt members dev-net | show dev-net | status [--target box]
ikit net zt authorize dev-net a1b2c3d4e5 | deauthorize dev-net a1b2c3d4e5
ikit net zt link dev-net | unlink                  # the selected workload (or --wl)
ikit net zt rm dev-net [--delete]                  # --delete also deletes it in Central
```

### Backups

- `backup run` dumps inside the service's container (`pg_dump -Fc`, `mysqldump`), compresses
  with zstd and uploads to a storage; the catalog keeps size and SHA-256. `restore` verifies the
  checksum, then `pg_restore --clean` (or `mysql`) replaces the data. Dumps are held in memory.
- Storage credentials come from a profile (`aws` for S3, `azure`, `gcp`) or the environment.
- `backup schedule` prints a crontab line; ikit never edits your crontab.

### Digest pinning

Envs in the org's `require_digest_envs` (default `[prod]`) refuse to deploy images that are not
pinned; `ikit plan` shows why. `ikit registry version pin` resolves the digest with
`docker buildx imagetools inspect`.

### Deploys

- The compose project for a workload is `orgs/<org>/deploy/<workload>/` for a local target and
  `~/.ikit/<org>/<workload>/` on an SSH target. `plan` diffs against those files, so it reflects
  what the target actually has.
- `compose.yaml` holds everything except secret values; they go to `secrets.compose.yaml`. Both
  files are owner-only (0600). `$` in values is escaped, so passwords reach containers verbatim.
- Env values come from the registry defaults, values set with `ikit svc set`, the keychain, and
  `from:` references (`service:<name>.host`, `.port`, `.port.<name>`, `.env.<VAR>`).
- Each apply or destroy holds a per-workload lock and is recorded in the run history. A workload
  must be destroyed before `ikit wl rm` removes it. Kubernetes targets arrive in Phase 5.

### Drift

`ikit status --drift` compares what runs on the target with the files last applied there:
missing, stopped and unhealthy containers, containers created from a different config (Compose's
`config-hash` label against `docker compose config --hash`), and extra containers in the project.
Definition changes that were never applied are counted separately (`ikit plan` shows them). It
exits 2 on drift, so cron or CI can alert; `ikit apply --force` recreates the containers.

### Hooks

- Phases: `pre-apply` (before files are written; in-service hooks are skipped on the first
  deploy), `post-apply` (services up and healthy) and `pre-destroy`. Hooks run in the order
  they were added, only when `apply` or `destroy` actually runs; `ikit hook run` runs them on
  demand.
- A failing SQL, shell or MCP hook fails the run; HTTP hooks only warn. Override with
  `--fail-on-error` / `--warn-only`.
- Shell hooks run with `sh -c` in a service's container (`--service`) or on the target host in
  the compose project folder, with `IKIT_ORG`, `IKIT_WORKLOAD`, `IKIT_ENV`, `IKIT_TARGET`,
  `IKIT_PHASE` and `IKIT_RUN_ID` set.
- HTTP hooks keep their URL (which usually carries a token) in the keychain. `{{workload}}`,
  `{{env}}`, `{{phase}}` and friends are replaced in the body and headers; a JSON body gets
  `content-type: application/json`.
- MCP hooks start an MCP server over stdio on the machine running ikit, call one tool and stop
  it. Placeholders are replaced in the JSON arguments' string values; the `IKIT_*` vars are set
  in the server's environment; `--secret-env NAME` values go to the keychain (read from the same
  variable in your shell, or prompted for). A result with `isError` counts as a failure.

SQL hooks:

- A SQL hook runs a `.sql` file, or a folder of them in name order, inside a database service
  (`postgres`, `mysql`) after every successful apply.
- By default (`--via exec`) SQL is piped through `docker compose exec` into the container's own
  client, using the container's credentials, so it works on SSH targets without publishing
  database ports. `--via direct` connects from your machine to the service's published port
  with the credentials ikit holds.
- Each script and its row in the database's `_ikit_hook_ledger` table commit together, so a
  failing script is rolled back and is retried on the next apply. Don't put `BEGIN`/`COMMIT`
  in scripts: ikit wraps each one in a transaction (MySQL DDL can't be rolled back).
- The ledger in the target database decides what has run: a script runs once, `R__*.sql`
  scripts re-run whenever they change, and editing an applied script is refused unless you
  pass `--allow-changed`. A wiped or restored database re-runs what it no longer has.
- `ikit plan` lists the scripts the next apply would run.

### ZeroTier

- A `zerotier` profile holds the Central API token (keychain) and an optional `api_url`.
- `net zt create` makes a private network with a managed route and address pool (default: the
  first free `10.147.x.0/24` in the org) and links the selected workload unless `--no-link`.
  A workload is linked to at most one network; a network can serve many workloads.
- `join`, `leave` and `status` run `zerotier-cli` here or on a VM target over SSH, falling back to
  `sudo -n` when the CLI cannot read the service's auth token as the current user.
- `authorize` works before a node joins, so a VM can be pre-authorized by its node ID.

### MCP

`ikit mcp serve` serves one org over stdio. Read tools: `workload_list`, `workload_show`,
`target_list`, `registry_list`, `registry_show`, `registry_outdated`, `hook_list`, `deploy_plan`,
`deploy_status`, `deploy_drift`, `workload_runs`. Prompts: `diagnose_workload`, `plan_upgrades`
and `review_plan`. Definition tools (`workload_create`, `service_add`, `service_set_env`,
`service_publish`, `hook_add_sql`) and deploy tools (`deploy_apply`, `deploy_destroy`) only work
where `orgs/<org>/mcp.yaml` allows them:

```yaml
mutate: true
mutate_envs: [dev, qa]
deploy: true
deploy_envs: [dev]       # never list prod unless you mean it
```

`zt_status` (read) lists the org's ZeroTier networks and members; `zt_authorize` needs
`zt_authorize: true` in `mcp.yaml`.

`ikit mcp serve --http HOST:PORT` serves the same tools over streamable HTTP at `/mcp`. Every
request needs `Authorization: Bearer <token>` from `ikit mcp token create`; only token hashes
are kept (`orgs/<org>/mcp-tokens.yaml`, owner-only) and `token rm` takes effect on the next
request. HOST must be loopback (`localhost`, `127.0.0.1`, `::1`) or ZeroTier: an address
ZeroTier assigned to this machine, `zt` (the first joined network), a network ID or an
interface name. Other interfaces, including `0.0.0.0`, are refused. With Claude Code:
`claude mcp add --transport http ikit-acme http://10.147.17.5:7420/mcp --header "Authorization: Bearer <token>"`.

`deploy_apply` needs the `plan_id` that `deploy_plan` returned and is refused if anything changed
since. `deploy_destroy` first returns a confirmation token (valid 5 minutes, single use) and
never deletes volumes. Secret values are never returned and cannot be set over MCP.

Version constraints: `latest`, an exact version (`1.4.2`), a prefix (`16` = 16.x, `16.4` = 16.4.x),
or any semver range (`^1.4`, `>=1.2, <2`). Deprecated versions are skipped unless pinned.

State lives in `~/.ikit` (override with `--home` or `IKIT_HOME`). Each org is fully separate:

```
~/.ikit/
├── context.yaml              # active org, and each org's active workload
└── orgs/<org>/
    ├── state.db              # the org's registry, targets, profiles and workloads
    ├── config.yaml           # display name, workload prefix (default wl-<org>), envs
    ├── mcp.yaml              # what MCP clients may do (optional)
    ├── mcp-tokens.yaml       # hashes of MCP HTTP bearer tokens
    └── workloads/<name>.yaml # spec generated after every change (not read back yet)
```

- Workloads are named `<prefix>-<env>-<nnn>`, e.g. `wl-acme-dev-001`. Prefixes are unique across
  orgs, so a name always identifies its org.
- `state.db` is copied to `state.db.bak-<version>` before each schema migration.
- A home created before orgs existed is moved into the `default` org by `ikit init`, keeping
  the `wl` prefix, so existing names don't change.
- Secrets (profile tokens and keys, service env vars marked `secret`) are kept in the OS keychain
  under the service `ikit`, namespaced per org (`org/<org>/...`), never in `state.db` or the
  spec files. Enter them at a prompt, or via
  `IKIT_SECRET_<FIELD>` for profiles and `IKIT_SECRET_<SERVICE>_<VAR>` for services.

## Layout

| Crate | Role |
|---|---|
| `ikit-core` | Domain model and validation, no I/O |
| `ikit-state` | `StateStore` trait and the SQLite backend (sqlx, migrations in `migrations/sqlite/`) |
| `ikit-provider` | Pure rendering of workloads to compose files, and the plan diff |
| `ikit-app` | Use cases shared by the CLI and the MCP server; keychain access, deploys |
| `ikit-net` | ZeroTier Central API client and `zerotier-cli` output parsing |
| `ikit-mcp` | MCP server (rmcp; stdio, or streamable HTTP with bearer tokens) over `ikit-app` |
| `ikit-cli` | The `ikit` binary: clap commands, prompts, table/JSON output |

## Development

Queries use sqlx's compile-time checked macros. Builds use the committed offline data in `.sqlx/`.
After changing a query or adding a migration, regenerate it:

```bash
./scripts/sqlx-prepare.sh
```

The script installs a SQLite-enabled `sqlx-cli` into `target/tools` if needed. CI runs it with `--check`.

Tests:

```bash
cargo test --workspace                                # unit tests and CLI tests, nothing needed
cargo test -p ikit-cli --test cli -- --ignored        # deploys real containers (needs Docker)
```

Releases: pushing a tag `vX.Y.Z` that matches the crate version runs
`.github/workflows/release.yml`, which builds macOS (arm64, x86_64) and Linux (x86_64, arm64)
binaries and publishes them with a `SHA256SUMS` file as a GitHub release.
