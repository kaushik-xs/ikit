# Service resources (design)

Status: phase 1 implemented (November 2026): S3 buckets and SQS queues through the `aws`
provider, which also covers S3- and SQS-compatible servers at an `endpoint`. Phases 2 and 3 are
still proposals. Where phase 1 differs from the first draft of this design:

- The registry key is `needs`, not `resources`: a version's `resources` already holds CPU and
  memory limits.
- Backends and env bindings live in `state.db` (like storages), not `config.yaml`, and are
  managed with `ikit res backend add|list|show|rm|bind|unbind`.
- A need with `use` has no row of its own; it resolves to the owning service's resource when
  rendering. Adding the user without the owner is allowed; `plan` fails until the owner is there.
- Resources whose backend isn't known yet don't fail `sg add`: `plan` shows a blocker instead.
- External self-hosted servers (phase 3) work already, as `aws` backends with `--endpoint`.

A service often needs more than a container: a bucket for its files, a queue for its jobs, a
database of its own. Today ikit leaves these to the operator. This design lets a registry service
declare the resources it needs, and lets ikit create them, wire them into the service's env, and
track them in the org's state, with either a managed provider (AWS S3, SQS, …) or a self-hosted
one (RustFS, ElasticMQ, …).

## The problem today

Adding `simulation-service` and `simulation-worker` to `wl-acme-dev-002` needs:

- `STORAGE_BUCKET` and `SQS_QUEUE_URL` marked `(required)`, so someone creates a bucket and a queue
  by hand, then pastes their names in. Nothing records that they belong to the workload.
- AWS keys pasted in as secrets, copied from another service or another machine.
- A queue of its own. The obvious choice, `acme-dev-simulation-jobs`, is already
  polled by about three remote workers, so a laptop deploy would hand its jobs to them.
- `ikit destroy` and `ikit wl rm` know nothing about the bucket or queue, so they are left behind.

The `create-platform-dbs` shell hook is the same problem for databases: one Postgres database per
service, created by a script each workload has to carry.

## Goals

- A service declares what it needs; adding it to a workload is enough to get it.
- The same service definition runs on self-hosted backends on a laptop and managed ones in prod.
- Resources are in `state.db`, so they show in `plan`, travel with remote state, and are cleaned
  up deliberately rather than forgotten.
- Credentials are created or looked up by ikit and kept in the org's secret store, never pasted.
- Existing buckets and queues can be adopted without being recreated.

Not goals: a general infrastructure-as-code tool (VPCs, IAM beyond what a resource needs, DNS),
or hiding API differences. A service that speaks SQS gets an SQS-compatible queue; ikit does not
translate SQS into AMQP.

## Concepts

- **Resource**: something one service needs, with a name local to that service (`assets`,
  `jobs`), a `kind` and an `api`.
- **Kind**: `bucket`, `queue`, `database`. Later maybe `topic`, `cache`.
- **API**: the wire protocol the service's code speaks. This is the contract, not the vendor:

  | kind     | api          | managed                       | self-hosted              |
  |----------|--------------|-------------------------------|--------------------------|
  | bucket   | `s3`         | AWS S3, Cloudflare R2         | RustFS, MinIO, Garage    |
  | bucket   | `azure-blob` | Azure Blob Storage            | Azurite                  |
  | bucket   | `gcs`        | Google Cloud Storage          | fake-gcs-server          |
  | queue    | `sqs`        | AWS SQS                       | ElasticMQ                |
  | queue    | `amqp`       | Amazon MQ, CloudAMQP          | RabbitMQ                 |
  | database | `postgres`   | RDS, Cloud SQL, Azure PG      | a postgres data service  |

- **Backend**: an org-level place that provides one API, e.g. `aws-sqs` (SQS in ap-south-1 with
  profile `acme`) or `rustfs` (a RustFS service inside the workload).
- **Binding**: which backend serves an API for an env, with per-workload overrides.

## Declaring resources in the registry

A version's spec gets a `needs` list, next to `env` and `depends_on`. Env vars reach a
resource with `from: resource:<name>.<field>`, the same way `from: service:postgres.host` reaches
another service today (`resolve_ref` in `crates/ikit-provider/src/compose/render.rs` grows a
`resource:` branch).

```yaml
# simulation-service 0.0.0
needs:
  - name: assets
    kind: bucket
    api: s3
  - name: jobs
    kind: queue
    api: sqs
    options:
      visibility_timeout: 300
      dead_letter: { max_receive: 10 }
env:
  - { name: STORAGE_BUCKET,        from: "resource:assets.name" }
  - { name: SQS_QUEUE_URL,         from: "resource:jobs.url" }
  - { name: AWS_REGION,            from: "resource:jobs.region" }
  - { name: AWS_ACCESS_KEY_ID,     from: "resource:jobs.access_key_id",     secret: true }
  - { name: AWS_SECRET_ACCESS_KEY, from: "resource:jobs.secret_access_key", secret: true }
```

A service that works on another service's resource says so with `use`, instead of declaring its
own. The worker shares the service's queue and bucket:

```yaml
# simulation-worker 0.0.0
needs:
  - { name: assets, use: simulation-service.assets }
  - { name: jobs,   use: simulation-service.jobs }
```

`use` resolves like `service:` references: an instance name, or a registry service with exactly
one instance in the workload. Without the owner in the workload, `plan` fails and names it.

Fields each API exposes:

| api          | fields                                                                        |
|--------------|-------------------------------------------------------------------------------|
| `s3`         | `name`, `endpoint`, `region`, `url` (`s3://…`), `access_key_id`*, `secret_access_key`* |
| `sqs`        | `name`, `url`, `arn`, `endpoint`, `region`, `access_key_id`*, `secret_access_key`*, `dlq_url` |
| `amqp`       | `queue`, `vhost`, `url`*                                                      |
| `postgres`   | `name`, `host`, `port`, `user`, `password`*, `url`*                           |

`*` fields are secrets: they render into `secrets.compose.yaml`, never `compose.yaml`.
`endpoint` is empty for AWS itself and set for self-hosted backends (e.g. `http://rustfs:9000`),
so services that honour `AWS_ENDPOINT_URL` work against either.

A value set on the instance still wins over `from`, as it does today, so an operator can always
point one service at something ikit doesn't manage.

## Backends and bindings

Backends belong to the org and are kept in `state.db`:

```sh
ikit res backend add aws-s3    --api s3  --profile acme --region ap-south-1
ikit res backend add aws-sqs   --api sqs --profile acme --region ap-south-1
ikit res backend add nas-minio --api s3  --profile nas  --endpoint https://minio.lan:9000
ikit res backend bind dev  nas-minio aws-sqs
ikit res backend bind prod aws-s3 aws-sqs
```

- `provider` picks the driver; phase 1 has `aws`, which with `--endpoint` talks to any S3- or
  SQS-compatible server. `profile` reuses the existing `aws` profiles: keys on the profile, a
  named `~/.aws` profile, or the environment when omitted.
- An env's binding picks the backend per API for its workloads' new resources. A workload
  overrides one resource with `ikit res bind <service>.<resource> <backend>` while it is pending.
- Later, `mode: in-workload` (phase 2): the backend runs as a data service in each workload that
  needs it, added from the registry when the first resource bound to it is added.

The existing `storages` table stays for backups and remote state. A storage could later name a
bucket backend instead of its own provider settings; that is out of scope here.

## State

Three new tables in `state.db` (migration `0013_resources.sql`), so resources travel with remote
state like everything else:

```text
resource_backends   id, name, api, provider, profile, options_json {region, endpoint}, created_at
resource_bindings   env, api, backend_id
workload_resources  id, workload_id, service, name, api,
                    backend_id    (NULL until `res bind` or the first apply picks one),
                    external_id   (bucket or queue name),
                    outputs_json  (non-secret fields only known once it exists: url, arn, dlq_url),
                    origin        ('created' | 'adopted'),
                    status        ('pending' | 'ready'),
                    orphaned      (its service no longer needs it),
                    created_at, updated_at
```

Region, endpoint and credentials are read from the backend and its profile when rendering, so
they always match the backend; phase 1 stores no per-resource secrets. Per-service credentials
(later) would go to the org's secret store under `org/<org>/workload/<wl>/resource/<service>.<name>`.

`workloads/<wl>.yaml` gets a `resources:` map per service (id, backend, status), so the generated
spec shows what each service is wired to.

## Lifecycle

| command | what happens to resources |
|---|---|
| `ikit sg add` / `svc …` | Records resources as `pending`. Creates nothing yet. |
| `ikit plan` | Lists resources to create next to the compose changes: `+ queue wl-acme-dev-002-simulation-service-jobs (sqs via aws-sqs, for simulation-service.jobs)`. Values only known after creation render as `<created by ikit apply>`; a resource with no backend is a blocker. |
| `ikit apply` | Creates pending resources (or brings an existing one with that name in line), records each as soon as it exists, then renders compose with the real values. Safe to repeat. |
| `ikit svc rm` | Marks the service's own resources `orphaned`; nothing is deleted. |
| `ikit destroy` | Leaves resources alone: buckets and queues hold data. |
| `ikit wl rm` | Refused while the workload has resources ikit created, unless `--delete-resources` or `--keep-resources` (forgets them, leaves them in place). |
| `ikit res list` / `show` | Resources of a workload or the org, with backend, status and origin. |
| `ikit res adopt <svc>.<name> <id>` | Records an existing bucket/queue/database (`origin: adopted`). Adopted resources are never deleted by ikit, only forgotten. |
| `ikit res rm <svc>.<name> [--keep]` | Deletes one orphaned resource (a bucket must be empty), or with `--keep` forgets it. |
| `ikit res bind <svc>.<name> <backend>` | Puts a pending resource on another backend than the env's. |

Later: drift (`ikit status --drift`) also reports resources that are recorded but missing, or
whose settings (visibility timeout, DLQ) differ from the spec.

With remote state, every command that creates or deletes a resource is a write: it holds the org
lock, so two SREs applying at once cannot create two queues for one service. If an apply fails
after creating a resource but before pushing, the next apply finds it by name and tags and
records it, instead of creating a second one.

## Where provisioning runs

- **Managed backends** (`provider: aws`, later `azure`, `gcp`): the ikit CLI calls the cloud API
  with the backend's profile. ikit's `object_store` dependency can read and write objects but
  cannot create buckets or queues, so this needs the provider SDKs (`aws-sdk-s3`,
  `aws-sdk-sqs`, `aws-sdk-iam` for scoped keys), behind a cargo feature per provider.
- **In-workload self-hosted backends**: the backend runs inside the target's Docker network,
  which the CLI may not reach (an SSH target like `acme-vm`). ikit renders a one-shot init
  service per backend into the compose file, e.g. `rustfs-init` running the `mc`/`aws` CLI to
  create buckets and access keys, or `elasticmq-init` creating queues. Services with resources on
  that backend wait for it with `depends_on: { <backend>-init: { condition:
  service_completed_successfully } }`. The init service's inputs come from `state.db`; its
  outputs (generated keys) are generated by ikit beforehand and passed in as secrets, so ikit
  knows them without reading them back. The same shape maps to a Job on Kubernetes targets.
- **External self-hosted backends** (`mode: external`, e.g. a MinIO on the NAS): the CLI calls the
  backend's API directly, like a managed one, when it is reachable from where ikit runs.

Each driver implements one trait in `ikit-provider`:

```rust
#[async_trait]
pub trait ResourceDriver {
    fn api(&self) -> Api;
    /// What `ensure` would do, without doing it.
    async fn plan(&self, want: &ResourceSpec, have: Option<&Recorded>) -> Result<ResourcePlan>;
    /// Create or update; returns the external id, outputs and secret outputs.
    async fn ensure(&self, want: &ResourceSpec, have: Option<&Recorded>) -> Result<Ensured>;
    /// Look up an existing resource to adopt it.
    async fn adopt(&self, id: &str) -> Result<Ensured>;
    async fn delete(&self, have: &Recorded) -> Result<()>;
}
```

In-workload drivers return their work as an init service for the renderer instead of doing it.

## Naming and tags

- Default names: `<workload>-<service>-<resource>`, e.g. `wl-acme-dev-002-simulation-service-jobs`.
  S3 bucket names are global, so buckets get a short random suffix
  (`wl-acme-dev-002-simulation-service-assets-7f3k2a`), stored in `external_id`. A resource can
  set `name:` in the workload to override.
- Every managed resource is tagged `ikit:org`, `ikit:workload`, `ikit:service`, `ikit:resource`.
  `res adopt` and crash recovery look resources up by these tags, and anything ikit created can be
  found in the cloud console without ikit.
- A dead-letter queue is a property of the queue resource (`dead_letter:`), named `<queue>-dlq`,
  not a separate resource.

## Credentials

- **Phase 1**: one key pair per backend. For `aws` backends it is the profile's own credentials
  (or a named `~/.aws` profile); for self-hosted ones, a key ikit generates for the backend. The
  `access_key_id` and `secret_access_key` fields resolve to it. This matches what services do by
  hand today.
- **Later**: per-service scoped credentials. For AWS, an IAM user (or role, for targets with
  instance roles) whose policy allows only that service's bucket and queue ARNs; for RustFS and
  MinIO, a user with a policy on its buckets. Services don't change: the same
  `resource:jobs.access_key_id` reference resolves to the scoped key instead.

## Databases

`kind: database, api: postgres` replaces per-service database hooks like `create-platform-dbs`:

```yaml
needs:
  - { name: db, kind: database, api: postgres }
env:
  - { name: DATABASE_URL, from: "resource:db.url", secret: true }
```

With an in-workload backend bound to the workload's `postgres` service, the init step creates the
database and a role for the service. With `provider: aws` it creates a database inside an
existing RDS instance named by the backend (creating RDS instances themselves is out of scope).

## Phases

1. **Core model with AWS** (done). Registry `needs`, `from: resource:`, `use`, the tables,
   backends and bindings, `plan`/`apply`/`wl rm` handling, a new `ikit res` command
   (`list|show|adopt|rm|bind|backend`), S3 buckets (with versioning) and SQS queues (with DLQ)
   on AWS or a compatible server, tags, phase-1 credentials.
2. **Self-hosted.** In-workload backends with init services: RustFS (or MinIO) for `s3`,
   ElasticMQ for `sqs`; `database`/`postgres` on the workload's postgres; retire
   `create-platform-dbs`.
3. **More providers and APIs.** Azure Blob and GCS, `amqp` on RabbitMQ, per-service scoped
   credentials, drift for resource settings. (External S3/SQS-compatible servers already work.)

## Example: the simulation services in phase 1

```sh
ikit res backend add aws-s3  --api s3  --profile acme --region ap-south-1
ikit res backend add aws-sqs --api sqs --profile acme --region ap-south-1
ikit res backend bind dev aws-s3 aws-sqs

# Once per registry version: what the services need, and which env vars read it.
ikit registry version set simulation-service 0.0.0 \
  --need assets:s3 --need jobs:sqs:visibility_timeout=300,dead_letter=10 \
  --env-from STORAGE_BUCKET=resource:assets.name --env-from SQS_QUEUE_URL=resource:jobs.url \
  --env-from AWS_ACCESS_KEY_ID=resource:jobs.access_key_id \
  --env-from AWS_SECRET_ACCESS_KEY=resource:jobs.secret_access_key
ikit registry version set simulation-worker 0.0.0 \
  --need assets=simulation-service.assets --need jobs=simulation-service.jobs \
  --env-from STORAGE_BUCKET=resource:assets.name --env-from SQS_QUEUE_URL=resource:jobs.url

ikit sg add platform --services simulation-service,simulation-worker --wl wl-acme-dev-002
ikit plan
#  + bucket wl-acme-dev-002-simulation-service-assets-7f3k2a  (s3 via aws-s3, for simulation-service.assets)
#  + queue  wl-acme-dev-002-simulation-service-jobs  (sqs via aws-sqs, for simulation-service.jobs)
#  + simulation-service   ghcr.io/acme/simulation-service:latest
#  + simulation-worker    ghcr.io/acme/simulation-worker:latest
ikit apply
```

A queue created by hand before phase 1 is taken over with
`ikit res adopt simulation-service.jobs <queue url>`, and the existing
`acme-dev-simulation-b88b643e` bucket with `ikit res adopt simulation-service.assets
acme-dev-simulation-b88b643e`.

## Open questions

- Should a binding be per env only, or also per target (a laptop dev workload on RustFS, a dev
  workload on `acme-vm` on S3)?
- Resource `options` are per kind (visibility timeout, versioning, retention). Which belong in the
  registry spec and which can a workload override?
- Should `ikit destroy --purge` empty and delete buckets for throwaway dev workloads, or is
  `wl rm --delete-resources` enough?
- Per-service IAM needs `iam:*` on the profile. Is that acceptable for the team's AWS accounts, or
  should scoped credentials come from a pre-made role ikit only attaches policies to?
