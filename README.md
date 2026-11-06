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
Phase 6 (ZeroTier: networks, members, joins, VM targets reached at their ZeroTier address, a
Compose sidecar, MCP over HTTP) and Phase 7 (drift detection, `registry outdated`, MCP hooks and
prompts, CLI integration tests, a release workflow). Phases 5 and 6b (Kubernetes, creating
targets) are still open.

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

ikit org remote set --storage s3                  # share the org: state goes to <prefix>/state/<org>
ikit org clone s3://backups/ikit/state/acme --aws-profile me --region eu-central-1   # a teammate joins
ikit org status | pull [--discard] | unlock --force | remote show | remote rm
ikit org history | restore <gen> [--with-secrets] | push   # push: retry what a failed command left
ikit identity create | show                       # this machine's key for encrypted org secrets
ikit org secrets init [--recipient <key>]         # keychain -> secrets/org.age (remote set does it too)
ikit org secrets list | grant <key> --as alice [--env prod] | revoke <key|label> [--env prod]
ikit org secrets split prod [--recipient <key>] | merge prod | check | forget-keychain

ikit registry version pin postgres 16.6            # deploy as image@sha256:… (required for prod)
ikit registry sync postgres [--add]                # newer version tags from the image registry
ikit registry outdated [--current]                 # services behind the registry; exits 2 if any

ikit mcp install claude                            # registers `ikit --org <org> mcp serve` with Claude Code
ikit mcp token create laptop                       # bearer token for HTTP, shown once
ikit mcp token create ci --workload wl-acme-dev-001   # a token that only reaches that workload
ikit mcp serve --workload wl-acme-dev-001          # the whole server limited to it (stdio or HTTP)
ikit mcp install claude --workload wl-acme-dev-001 # registers it as `ikit-wl-acme-dev-001`
ikit mcp serve --http zt:7420                      # streamable HTTP on this machine's ZeroTier address
ikit mcp serve --http 127.0.0.1:7420               # or loopback only
ikit mcp token list | rm <name>

ikit profile add zerotier zt --set org_id=<org>    # prompts for a service account API key (or IKIT_SECRET_API_TOKEN)
ikit net zt create dev-net --profile zt            # private network, links the selected workload
ikit net zt attach shared 8056c2e21c000001 --profile zt   # an existing network
ikit net zt join dev-net --target box --authorize  # zerotier-cli join over SSH, then authorize
ikit net zt members dev-net | show dev-net | status [--target box]
ikit net zt authorize dev-net a1b2c3d4e5 | deauthorize dev-net a1b2c3d4e5
ikit net zt link dev-net [--sidecar] | unlink      # the selected workload (or --wl)
ikit net zt sidecar on|off                         # a ZeroTier container in the workload
ikit target attach vm box --zt dev-net --node a1b2c3d4e5 --user ubuntu   # SSH at its ZeroTier address
ikit target use-zt edge dev-net                    # move an SSH target onto its ZeroTier address
ikit net zt rm dev-net [--delete]                  # --delete also deletes it in Central

ikit net docker attach regere-dev-platform-network --group app   # join a network ikit does not own
ikit net docker list | detach regere-dev-platform-network [--group app]

ikit target edge install box caddy --email ops@acme.com   # or ferron; picked interactively if omitted
ikit target edge install box                       # another org: joins the proxy already there
ikit target edge status box | reload box | upgrade box [--version 2.11.8]
ikit target edge uninstall box [--purge]           # this org leaves; the last one removes ikit
ikit route add web --host acme.com                 # https://acme.com → web's `http` port
ikit route add api --host acme.com --path /api --port 8080
ikit route add admin --tls internal                # host from edge.host_template; proxy's own CA
ikit route redirect www.acme.com https://acme.com
ikit route auto on --groups app                    # a route per app service with an `http` port
ikit route list | rm acme.com/api | unredirect www.acme.com | auto off

ikit res backend add aws-s3 --api s3 --profile acme --region eu-central-1
ikit res backend add elasticmq --api sqs --endpoint http://elasticmq.lan:9324   # SQS-compatible
ikit res backend bind dev aws-s3 elasticmq         # where dev workloads' buckets and queues go
ikit registry version set sim 1.0.0 --need jobs:sqs:visibility_timeout=300,dead_letter=10 \
  --env-from SQS_QUEUE_URL=resource:jobs.url        # the service declares what it needs
ikit registry version set sim-worker 1.0.0 --need jobs=sim.jobs   # shares sim's queue
ikit res list [--all] | show sim.jobs              # created by the next `ikit apply`
ikit res bind sim.jobs aws-sqs                     # this one elsewhere (before it exists)
ikit res adopt sim.assets acme-dev-simulation-b88b643e   # use an existing bucket or queue
ikit res rm sim.jobs [--keep]                      # after `ikit svc rm sim`
ikit wl rm wl-acme-dev-002 --delete-resources      # or --keep-resources
ikit res backend iam-policy aws-sqs                # boundary + provisioning policies to apply
ikit res backend set aws-sqs --per-service-keys --boundary arn:aws:iam::123456789012:policy/ikit-acme-boundary
ikit res keys [--all] | rm sim [--keep]            # each service's own IAM user and key
ikit res rotate sim                                # new key; the next apply retires the old one
```

### Backups

- `backup run` dumps inside the service's container (`pg_dump -Fc`, `mysqldump`), compresses
  with zstd and uploads to a storage; the catalog keeps size and SHA-256. `restore` verifies the
  checksum, then `pg_restore --clean` (or `mysql`) replaces the data. Dumps are held in memory.
- Storage credentials come from a profile (`aws` for S3, `azure`, `gcp`) or the environment. An
  `aws` profile can hold keys itself or name a profile in `~/.aws/credentials` with
  `--set aws_profile=<name>` (static keys only; SSO and assume-role profiles are not supported).
- `backup schedule` prints a crontab line; ikit never edits your crontab.

### Resources

Design and later phases: [docs/resources.md](docs/resources.md).

- A registry version lists the buckets (`s3`) and queues (`sqs`) its service `needs`, and env vars
  read them with `from: resource:<name>.<field>`: `name`, `url`, `arn`, `region`, `endpoint`,
  `dlq_url` (queues), and the backend's `access_key_id` / `secret_access_key`, which go to the
  secrets file. A need with `use: <service>.<resource>` shares another service's resource.
- Adding the service records each resource as pending, named `<workload>-<service>-<resource>`
  (buckets get a random suffix; their names are global). `ikit plan` lists what it will create;
  values only known afterwards (a queue's URL) show as `<created by ikit apply>`.
- `ikit apply` creates pending resources on their backend before deploying, tagged `ikit:org`,
  `ikit:workload`, `ikit:service` and `ikit:resource`, and records each as soon as it exists.
  A queue with `dead_letter` gets `<queue>-dlq`. Creating is safe to repeat.
- Backends belong to the org and provide one API: `--provider aws` is AWS itself, or any
  S3- or SQS-compatible server (RustFS, MinIO, Garage, ElasticMQ) with `--endpoint`, which the
  target must reach too. Credentials come from the backend's `aws` profile, as for storages.
- An env's binding picks the backend for its workloads' new resources; `res bind` overrides one
  resource. Once a resource exists it stays on its backend.
- Removing a service orphans its resources; `res rm` deletes one (a bucket must be empty) or,
  with `--keep`, forgets it. Adopted resources are never deleted, only forgotten. `wl rm` is
  refused while resources ikit created exist, unless `--delete-resources` or `--keep-resources`.
- By default services get the backend profile's keys. With `--per-service-keys` (AWS only),
  `apply` gives each service an IAM user `<workload>-<service>` under `/ikit/<org>/`, with one
  inline policy covering exactly its resources (shared ones included), and keeps its access key
  in the org's secret store (`org.age` when the org has one). Every such user carries the
  backend's `--boundary` policy; `res backend iam-policy` prints that boundary and the IAM policy
  the provisioning identity needs (users only under the org's path, only with the boundary).
  ikit doesn't create either policy. `res rotate` adds a second key; the apply that redeploys the
  service deletes the old one.

### Remote state

Setting up, joining, offboarding and recovering a shared org: [docs/team.md](docs/team.md).

- `org remote set` makes an org shared: its `state.db`, `config.yaml` and MCP files go to
  `<storage prefix>/state/<org>/` as numbered snapshots (`snapshots/000042.tar.zst`), with
  `HEAD.json` naming the current one. The local org folder becomes a cache of HEAD.
- Commands that change state take `lock.json` (a 10-minute lease, renewed while they run), bring
  the cache up to HEAD, run, and push a new generation if anything changed. HEAD is replaced
  only by a conditional write, so a writer whose lock was broken cannot overwrite newer state.
  Read-only commands take no lock and only pull. A waiting writer gives up after
  `--lock-timeout` seconds (default 60) and names the holder.
- Local changes that were never pushed block pulling over them; `org pull --discard` drops them.
- The remote must be reachable without the org's secrets: use an `aws` profile with
  `aws_profile`, a GCS credentials file, or the environment. `ikit mcp serve` refuses remote
  orgs.
- `remote.yaml` (where the remote is) and `sync.json` (the generation the cache is at) stay local.
- Every generation is kept as `snapshots/<gen>-<id>.tar.zst` plus `history/<gen>.json` (who,
  when, which command). `org history` lists them; `org restore <gen>` makes one current again as
  a new generation, keeping today's secrets and readers unless `--with-secrets`. Snapshots older
  than 90 days and outside the newest 50 generations are pruned after each push.
- A pull that changes who can read the org's secrets says so, naming who gained or lost access.
- `ikit mcp serve` works on remote orgs: each tool call pulls first, and tools that change state
  take the lock and push. Calls on one server run one at a time.

### Encrypted secrets

- `ikit org secrets init` (and `org remote set`) moves an org's secrets from the keychain into
  `secrets/org.age`, encrypted with [age](https://age-encryption.org/v1) to the public keys in
  `recipients/org.txt`. Both files travel with the remote state; nothing is ever stored in clear.
  The keychain copies stay until `ikit org secrets forget-keychain`.
- `org secrets split prod` gives an environment its own file (`secrets/prod.age`,
  `recipients/prod.txt`) for a smaller group: secrets of `prod` workloads and their hooks live
  there; `grant|revoke --env prod` manage it; `merge prod` folds it back into `org.age`.
- Each person runs `ikit identity create` once; the private key stays in the keychain and the
  printed `age1…` key is what a teammate grants. SSH keys work too: grant an `ssh-ed25519` public
  key and pass `--identity-file ~/.ssh/id_ed25519` (unencrypted keys only). CI sets
  `IKIT_AGE_IDENTITY` to a key from `ikit identity create --print-only`.
- Only a current recipient can change the secrets or who can read them. A revoked key cannot
  read later versions, but may have kept a copy (and old snapshots), so rotate what it could read.
- Keep a recovery key: `ikit identity create --print-only`, store the private key offline, and
  `ikit org secrets grant <its public key> --as recovery`. `org secrets check` exits 1 when a
  secrets file has no `recovery…` key or fewer than 2 people (keys not labelled `recovery…` or
  `ci…`); run it in CI.

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
  `from:` references (`service:<name>.host`, `.port`, `.port.<name>`, `.env.<VAR>`, and
  `.url`, the service's first route).
- Several workloads can share a target: each is its own Compose project with its own network.
  `plan` blocks a host port that another applied (or failed) workload on the same target
  already publishes, unless both bind different addresses. Ports taken by other orgs or by
  anything else on the host still fail at `docker compose up`.
- Each apply or destroy holds a per-workload lock and is recorded in the run history. A workload
  must be destroyed before `ikit wl rm` removes it. Kubernetes targets arrive in Phase 5.

### Reverse proxy and routes

- `target edge install` puts Caddy or Ferron on a Linux VM target's host (not in a container),
  as a systemd service that owns ports 80 and 443. It needs root or passwordless `sudo` there.
  ikit installs from the official apt (Debian/Ubuntu) or dnf (RHEL/Fedora) repository and
  replaces the main config with one that includes `/etc/ikit/edge/sites/*.conf`, keeping the
  original as `<config>.ikit-orig`. `--open-firewall` allows 80/443 in ufw or firewalld.
- A proxy that is already installed is adopted: ikit appends its include line to the existing
  config and leaves the rest alone, and `uninstall` only takes that line out again. Another
  server on 80/443 (nginx, a container) makes `install` refuse.
- The ACME email belongs to the org: `--email` saves it as `edge.acme_email` in the org's
  `config.yaml`, later installs on other targets reuse it, and every site this org renders uses
  it for its certificates.
- One proxy serves every org on the host. `/etc/ikit/edge/host.json` records the server and the
  orgs that joined; a second org's `install` joins it (`--exclusive` keeps others off). Each
  workload has its own site file whose header names its org, workload, domains and upstream
  ports, so `plan` blocks a domain another org already serves, and the write on apply re-checks
  it under the host's lock. `uninstall` takes this org off; the last org stops a proxy ikit
  installed (`--purge` removes the package).
- `upgrade` updates the package for everyone on the host, checks the config with the new
  version and restarts it; if the config is rejected or the service does not come back, the
  previous version is reinstalled.
- `route add` sends a domain (and an optional path prefix, stripped before proxying) to a service
  port. ikit publishes that port on `127.0.0.1` at an upstream port from `edge.upstream_ports`
  (default 20000-29999), so it is only reachable through the proxy. Without `--host`, the
  domain comes from `edge.host_template` (default `{service}.{workload}.{base_domain}`) and
  `edge.base_domain` in the org's `config.yaml`. TLS modes: `acme` (default; the domain must
  resolve to the host), `internal` (the proxy's own CA, for private or ZeroTier-only hosts) and
  `off`. Envs in `edge.require_tls_envs` (default `[prod]`) refuse `off`.
- `route auto on` keeps a route for every service in the chosen groups that has a registry port
  named `http` (or `--port-name`); routes follow `sg add`, `svc rm` and `svc upgrade`, and one
  removed by hand is not added back. `edge.auto_routes` in `config.yaml` turns this on for new
  workloads. Manual routes always win.
- `plan` shows site changes next to the Compose changes, and warns about domains that do not
  resolve. `apply` switches the proxy over once the containers are healthy (a config the proxy
  rejects keeps the previous site); `destroy` takes the site down before stopping containers.
  `status --drift` reports a stopped proxy or a missing site file.

### Networks

- Each service group in use gets its own network, owned by the workload's Compose project
  (`<workload>_app`, `_platform`, `_data`) and removed with it. A service joins its group's
  network and the ones below it: app → app, platform, data; platform → platform, data;
  data → data only. Services reach each other by service name.
- `net docker attach <network> --group <group>` has a group join a Docker network the workload
  does not own (another Compose project's, or one made with `docker network create`) as
  `external: true`. ikit never creates or removes it, and `plan` is blocked while it is missing
  on the target. On it, each service also answers to `<workload>-<service>`, which stays
  unique when several workloads share the network.

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

- ikit talks to the ZeroTier Central API v2 (New Central). A `zerotier` profile holds a service
  account's API key (keychain), the `org_id`, an optional `network_group` for new networks
  (default: the org's only group, or a new `ikit` group when it has none) and an optional
  `api_url` (default `https://central.zerotier.com/api/v2`). Legacy v1 tokens are not accepted.
- `net zt create` makes a private network with a managed route and address pool (default: the
  first free `10.147.x.0/24` in the org) and links the selected workload unless `--no-link`.
  A workload is linked to at most one network; a network can serve many workloads.
- `join`, `leave` and `status` run `zerotier-cli` here or on a VM target over SSH, falling back to
  `sudo -n` when the CLI cannot read the service's auth token as the current user.
- `authorize` works before a node joins, so a VM can be pre-authorized by its node ID.
- VM targets can be reached over SSH at their ZeroTier address, so their SSH port never has to
  be public. `target attach vm <name> --zt <network> --node <id>` authorizes the node (it may
  join later) and uses the address Central assigns it; `target use-zt <name> <network>` joins an
  existing SSH target over its current connection and switches it to its ZeroTier address.
  `target test` first checks the node is authorized in Central and picks up a changed address.
  A network can't be removed while a target is reached through it.
- `net zt link --sidecar` (or `net zt sidecar on`) adds an `ikit-zerotier` service to the
  workload's compose file: `zerotier/zerotier` on the host network with its identity in a named
  volume. `apply` authorizes it, named after the workload, before post-apply hooks run, so
  published ports are reachable at the target's ZeroTier address. Use it on Linux VMs that do
  not run ZeroTier themselves (it needs `/dev/net/tun`). `destroy --volumes` deletes its
  identity and deauthorizes it; turning the sidecar off leaves the node authorized.

### MCP

`ikit mcp serve` serves one org over stdio. Read tools: `workload_list`, `workload_show`,
`target_list`, `registry_list`, `registry_show`, `registry_outdated`, `hook_list`, `deploy_plan`,
`deploy_status`, `deploy_drift`, `workload_runs`, `route_list`. Prompts: `diagnose_workload`,
`plan_upgrades` and `review_plan`. Definition tools (`workload_create`, `service_add`,
`service_set_env`, `service_publish`, `hook_add_sql`, `route_add`, `route_remove`) and deploy tools (`deploy_apply`, `deploy_destroy`) only work
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

Access can be limited to some workloads: a token with `ikit mcp token create <name> --workload
<wl>` (repeat or comma-separate), or the whole server with `ikit mcp serve --workload <wl>`.
Over HTTP a request reaches the workloads both allow. Other workloads look as if they did not
exist, lists and `registry_show`'s `used_by` leave them out, `target_list` and `zt_status`
show only the scoped workloads' targets and networks, and `workload_create` and `zt_authorize`
are refused. A scope only narrows: `mcp.yaml` still decides what may be done in each env.
`ikit mcp token list` shows each token's workloads.

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
    ├── recipients/<file>.txt # public keys that can read secrets/<file>.age (org, or an env)
    ├── secrets/<file>.age    # the org's secrets, age-encrypted (after `org secrets init`)
    ├── remote.yaml           # where the org's remote state lives (optional, never synced)
    ├── sync.json             # the remote generation the local copy is at
    └── workloads/<name>.yaml # spec generated after every change (not read back yet)
```

- Workloads are named `<prefix>-<env>-<nnn>`, e.g. `wl-acme-dev-001`. Prefixes are unique across
  orgs, so a name always identifies its org.
- `state.db` is copied to `state.db.bak-<version>` before each schema migration.
- A home created before orgs existed is moved into the `default` org by `ikit init`, keeping
  the `wl` prefix, so existing names don't change.
- Secrets (profile tokens and keys, service env vars marked `secret`) are kept in the OS keychain
  under the service `ikit`, namespaced per org (`org/<org>/...`), or in the org's encrypted
  `secrets/org.age` once it has one; never in `state.db` or the spec files. Enter them at a prompt, or via
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
