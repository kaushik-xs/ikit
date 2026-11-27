# ikit from zero: one new VM → full regere env (platform + app)

Built from `deploy/regere/README.md`, `deploy/regere/env-up.sh`, `docs/team.md`, `cheatsheet.md`
and the `--help` output of ikit 0.1.1.

## Overview

Two machines:

| Machine | Role | Needs |
|---|---|---|
| **Control machine** (your laptop) | Runs `ikit`. Holds org state and secrets (OS keychain) | Rust, ikit, jq, ssh, a desktop keychain |
| **VM** (new server) | Runs containers. ikit reaches it over SSH | Docker + compose plugin, SSH, ports 80/443 open |

Run ikit on the laptop, not on the VM. ikit stores secrets in the OS keychain (GNOME
Keyring/KWallet), and a headless VM has none, so commands that write secrets fail there.

What gets deployed: two linked workloads per env:

- **platform** (`wl-regere-dev-001`): postgres, redis, common-services, load-optimizer,
  message-flow, platform-gw, sockudo, yard-recom, simulation-service, simulation-worker ×3
- **app** (`wl-regere-dev-002`): postgres, redis, rabbitmq, authrs, core, launchpad,
  workflow-engine, decision-hub, dwigital-shell

---

## Before you start: blockers (checked 27 Nov 2026)

These stop a public deploy today. Clear them first.

| # | Blocker | What breaks | Clear it by |
|---|---|---|---|
| 1 | The deploy files live on branch `feat/standalone-platform-gw`, not `main` | A `main` clone has the old linked-workload `env-up.sh` and no `seed-platform-gw.sh` | Merge the branch, or `git checkout feat/standalone-platform-gw` (Step 2) |
| 2 | [Regere-AI/platform-gw#5](https://github.com/Regere-AI/platform-gw/pull/5) (IP allowlists) is still open | `seed-platform-gw` stops with "has no IP allowlists yet". Post-apply hooks fail hard, so the **platform apply fails** and `env-up.sh` (`set -e`) never deploys the app | Merge it and let `ghcr.io/regere-ai/platform-gw:latest` rebuild |
| 3 | [Regere-AI/launchpad#2](https://github.com/Regere-AI/launchpad/pull/2) (sends the gateway key) is still open | Everything deploys, but launchpad calls the gateway without a key: every platform call (load-optimizer, simulation, message-flow, ...) returns 401 | Merge it and let `kaushik91/launchpad:latest` rebuild |

---

## Step 1: Prepare the VM (once)

Size it for about 21 containers: 2 postgres, rabbitmq, 3 simulation workers, and
common-services with a 2 GB `/dev/shm`. Plan on **at least 8 vCPU, 16 GB RAM, 100 GB disk**.
That figure is an estimate, not a measurement.

SSH into the VM (Ubuntu assumed):

```bash
# Docker engine + compose plugin
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker $USER        # run docker without sudo
exit                                 # log out and back in so the group applies
```

SSH in again and check:

```bash
docker version && docker compose version
docker run --rm hello-world
```

Cap the container logs, or they grow until the disk is full (Docker's default `json-file`
driver has no limit):

```bash
echo '{"log-driver":"json-file","log-opts":{"max-size":"50m","max-file":"5"}}' \
  | sudo tee /etc/docker/daemon.json
sudo systemctl restart docker
```

`edge install` runs apt as root through `sudo -n`, so the SSH user needs **passwordless sudo**.
`seed-platform-gw` also needs `openssl` on the VM (Ubuntu ships it):

```bash
echo "$USER ALL=(ALL) NOPASSWD:ALL" | sudo tee /etc/sudoers.d/ikit   # or limit it to apt/systemctl
sudo -n true && echo "sudo ok"
which openssl
```

Turn on automatic security updates:

```bash
sudo apt-get install -y unattended-upgrades && sudo dpkg-reconfigure -plow unattended-upgrades
```

From the **laptop**, set up key-based SSH:

```bash
ssh-copy-id ubuntu@<VM_IP>
ssh ubuntu@<VM_IP> 'docker ps'       # must work with no password and no sudo
```

**DNS:** for a public deploy, create 3 A records pointing at the VM's public IP:

| Host | For |
|---|---|
| `dev.example.com` | UI (dwigital-shell) + `/rt` (sockudo) |
| `api.dev.example.com` | launchpad |
| `gw.dev.example.com` | platform-gw |

**Firewall / cloud security group:** allow 22, 80, 443 inbound.

---

## Step 2: Install ikit on the laptop

```bash
# build deps (keyring links libdbus)
sudo apt-get install -y build-essential pkg-config libdbus-1-dev jq git
curl https://sh.rustup.rs -sSf | sh     # if no Rust yet
source ~/.cargo/env

cargo install --git https://github.com/kaushik-xs/ikit ikit-cli --locked
ikit --version

ikit init                               # creates ~/.ikit; re-run after every ikit upgrade
```

Get the deploy files:

```bash
git clone https://github.com/kaushik-xs/ikit.git ~/ikit
cd ~/ikit
git checkout feat/standalone-platform-gw     # until it is merged into main (blocker 1)
```

---

## Step 3: Create the org

The org is the tenant boundary. It has its own state DB, registry, secrets and workloads.

```bash
ikit org create regere --display-name "Regere" --envs dev,qa,stg,prod
ikit org use regere
ikit org show
```

Workloads get names like `wl-regere-<env>-<nnn>`.

> ⚠️ A team org named `regere` already exists in S3 (`docs/join-regere-org.md`). If this VM
> should be managed by that team org, **do not create a new one**. Run `ikit identity create`,
> ask a key holder to grant you, then
> `ikit org clone s3://regere-ikit-backups/ikit/state/regere --aws-profile regere --region ap-south-1`,
> and skip to Step 5.

---

## Step 4: Load the service catalog and scripts

```bash
ikit registry import deploy/regere/registry.yaml     # every regere service spec
ikit registry list                                   # check it

for s in deploy/regere/scripts/*.sh; do ikit script add "$s"; done
ikit script list      # create-service-dbs, seed-app, seed-platform-gw
```

Private images (ghcr.io/regere-ai/*) need a login. Use a GitHub PAT with `read:packages`:

```bash
IKIT_REGISTRY_PASSWORD=<ghp_token> ikit registry login ghcr.io --username <github-user>
ikit registry logins
```

`ikit apply` logs the VM in before it pulls.

---

## Step 5: Attach the VM as a target

```bash
ikit target attach vm dev-vm --ssh ubuntu@<VM_IP>    # runs a connectivity test
ikit target test dev-vm
ikit target show dev-vm
```

Reverse proxy, needed for the public URLs (Caddy, automatic Let's Encrypt certificates):

```bash
ikit target edge install dev-vm caddy --email ops@example.com --open-firewall
ikit target edge status dev-vm
```

You can add `--staging` for a first trial run (untrusted certificates, no rate limits).

---

## Step 6: Cloud profile + resource backends (S3 buckets, SQS queue)

Several services declare `needs:` (buckets, the simulation queue). `ikit apply` creates them on
the backend bound to the env, so a backend must be bound first.

```bash
ikit profile fields aws                         # shows the accepted fields

# Option A: static keys (the secret is prompted, then stored in the keychain)
ikit profile add aws regere-aws --set region=ap-south-1 --set access_key_id=AKIA... \
  --secret secret_access_key

# Option B: reuse ~/.aws
ikit profile add aws regere-aws --set region=ap-south-1 --set aws_profile=regere

ikit res backend add aws-s3  --api s3  --profile regere-aws
ikit res backend add aws-sqs --api sqs --profile regere-aws
ikit res backend bind dev aws-s3 aws-sqs
ikit res backend list
```

No AWS? `deploy/regere/mock-backends.sh up` runs RustFS + ElasticMQ locally. That is for
testing only.

---

## Step 7: Secrets ikit cannot generate

ikit generates everything else (DB passwords, gateway keys, sockudo keys, the admin password).
You supply 3 values:

```bash
export SMTP_PASS='<zeptomail smtp password>'

# web-push key pair (or reuse the existing pair so browser subscriptions keep working)
eval "$(docker run --rm ghcr.io/regere-ai/message-flow:1.0.2 gen-vapid \
        | sed 's/^MF_CHANNELS__PUSH__/export /')"
echo $VAPID_PUBLIC_KEY      # check that it is set
```

Run this in the same shell as Step 8.

---

## Step 8: Bring up the env

### Public (real VM, real domains)

```bash
deploy/regere/env-up.sh --env dev --target dev-vm \
  --public-url  https://dev.example.com \
  --api-url     https://api.dev.example.com \
  --gateway-url https://gw.dev.example.com \
  --app-ips     <VM_PUBLIC_IP>
```

`--app-ips` lists the IPs allowed to use the platform-gw key. With app and platform on one VM,
launchpad calls the gateway's public hostname, so the call arrives from the VM's own public IP.
That only holds when the cloud supports hairpin traffic (a VM reaching its own public IP).
Most clouds do (AWS, GCP), but NAT routers and some providers don't. The gateway host isn't
mapped inside the app workload: ikit maps only the workload's own route hosts to the host.
Check it after deploy (Step 9).

### Local-only (no DNS, quick test)

```bash
deploy/regere/env-up.sh --env dev --target dev-vm
```

The UI and launchpad are published on `127.0.0.1` on the VM. Reach them with an SSH tunnel.

### What the script does, in order

1. Adds the library scripts.
2. Creates the platform workload, then the app workload.
3. Adds the platform data group (postgres, redis) and platform group, plus hooks `service-dbs`
   (post-data) and `gw-seed` (post-apply).
4. Adds the app data group (postgres, redis, rabbitmq) and app group, plus hooks `service-dbs`
   and `seed`.
5. Adds routes (or published ports) and sets URLs.
6. Runs `ikit apply` on the platform first, then the app.

At the end it prints the UI, Launchpad and Gateway URLs and the workload names.

Want to review before deploying? Add `--no-apply`, then:

```bash
ikit wl use wl-regere-dev-001     # platform
ikit render                       # compose file, secrets hidden
ikit plan
ikit apply
ikit wl use wl-regere-dev-002     # app
ikit plan && ikit apply
```

---

## Step 9: Verify

```bash
ikit wl list
ikit status --wl wl-regere-dev-001
ikit status --wl wl-regere-dev-002
ikit status --drift --all            # exits 2 on drift
ikit hook history --wl wl-regere-dev-002
ikit res list --all                  # buckets + queue created
ikit route list --wl wl-regere-dev-002
```

On the VM: `docker ps` should show about 21 containers.

Admin login:

- User: `tech@regere.ai`
- Password:

```bash
ikit --wl wl-regere-dev-002 svc secret show authrs SEED_ADMIN_PASSWORD --yes
```

Open `https://dev.example.com` in a browser.

Check the app → gateway path (blockers 2 and 3, and hairpin). From inside launchpad, the
gateway should answer, not time out:

```bash
ssh ubuntu@<VM_IP> 'docker exec wl-regere-dev-002-launchpad-1 \
  sh -c "wget -qSO- https://gw.dev.example.com/ 2>&1 | head -3"'
```

The `gw-seed` hook output in `ikit hook history --wl wl-regere-dev-001` should list the
services and the key. Then open a page in the UI that uses load-optimizer or simulation: a 401
or 403 means the key or IP allowlist is wrong.

---

## Step 10: Back up the org state (do not skip)

Right now all state and every secret lives **only on your laptop** (`~/.ikit` plus the
keychain). If the laptop dies, the generated DB passwords and keys are gone with it.

```bash
ikit identity create                         # this machine's age key
ikit storage add s3 --provider s3 --bucket <state-bucket> --prefix ikit --profile regere-aws
ikit org remote set --storage s3             # pushes state, moves secrets into org.age
ikit identity create --print-only            # recovery key: store the private half offline
ikit org secrets grant <recovery age1…> --as recovery
ikit org secrets check
```

Then set up DB backups:

```bash
ikit backup run postgres --wl wl-regere-dev-002 --storage s3 --keep 7
ikit backup schedule postgres --wl wl-regere-dev-002 --cron "0 2 * * *"   # prints a crontab line
```

`backup schedule` only prints the line. Install it on a machine that is **always on** and has
ikit plus the org: a laptop that sleeps at 02:00 takes no backups. Back up both workloads'
postgres (`wl-regere-dev-001` and `-002`), and test a `backup restore` once.

---

## Day 2 cheats

| Want | Cmd |
|---|---|
| Another env on the same VM | `ikit res backend bind qa aws-s3 aws-sqs`, then `env-up.sh --env qa --target dev-vm ...` (new hosts) |
| Upgrade a service | `ikit registry sync core --add` → `ikit svc upgrade core --wl <app>` → `ikit plan` → `ikit apply` |
| Change an env value | `ikit svc set launchpad --wl <app> KEY=val` → `ikit apply` |
| Re-run the seed | `ikit hook run seed --wl <app>` |
| Redo the env from scratch | `env-up.sh ... --app <app-wl> --platform <pf-wl> --reset` |
| Fix drift | `ikit apply --force` |
| Tear down | `ikit destroy --wl <wl>` (add `--volumes` only to **delete data**) → `ikit wl rm <wl>` |
| UI instead of CLI | `ikit tui` |

## Troubleshooting

| Symptom | Fix |
|---|---|
| `target test` fails | `ssh ubuntu@<VM_IP> docker ps` must work with no password or sudo; check docker group, re-login |
| Keychain / Secret Service error | Running ikit headless. Use the laptop, or pass `--identity-file` with an age key |
| Image pull `denied` | `ikit registry login ghcr.io` with a token that has `read:packages` |
| `plan` shows a resource blocker | No backend bound for the env: `ikit res backend bind <env> aws-s3 aws-sqs` |
| No certificate | DNS not pointing at the VM yet, or 80/443 closed. Fix it, then `ikit apply` again |
| decision-hub dead after a VM reboot | Known: it does not retry the broker. `ikit apply --force --wl <app>` |
| `newer state schema` | Upgrade ikit (`cargo install ... --force`), then `ikit init` |

---

## Known gaps

See also the blockers at the top.

- **High: every app image is `:latest`, with no digest pinned.** A later `apply`, or a restart
  after an image prune, can pull a different build without warning. **Fix:**
  `ikit registry version pin <svc> <version>` for each service (ikit requires it for prod), and
  move to version tags.
- **High: no monitoring or alerting.** Nothing tells you when a container dies, the disk
  fills, or a certificate fails to renew. **Fix:** at minimum run `ikit status --drift --all`
  from cron with an alert on exit 2, plus uptime checks on the 3 URLs.
- **Medium: decision-hub after a reboot.** Docker restarts every container at once
  (`unless-stopped`), without compose's start order. decision-hub doesn't retry RabbitMQ and
  stays broken until `ikit apply --force --wl <app>`. **Fix:** a retry in decision-hub, or a
  systemd unit that runs `apply --force` after boot.
- **Medium: images from personal Docker Hub accounts** (`kaushik91/*`, `godson326/sockudo`).
  Anonymous pulls are rate-limited and the images live outside the org's control. **Fix:** mirror
  them to `ghcr.io/regere-ai`.
- **Medium: the platform-gw key depends on hairpin traffic** (Step 8). **Fix:** on a cloud
  without it, have ikit map platform-gw's route host inside the app workload too.
- **Medium: secrets you supply are typed into a shell.** `SMTP_PASS` and the VAPID key land in
  shell history if you type them inline. **Fix:** `read -rs SMTP_PASS; export SMTP_PASS`, or
  source a `chmod 600` file.
- **High: the org has a single point of failure.** Until `org remote set` runs, the generated
  secrets exist on one laptop only. **Fix:** do Step 10 right after the first deploy, not later.
- **Medium: the org name collides with the existing team org.** The existing `regere` org lives
  in S3. A second local `regere` org means two diverging sources of truth. **Fix:** clone the
  team org, or pick a different name (`--prefix`).
- **Medium: there is no verified prebuilt binary.** No published release was found, so newbies
  have to build from source (needs Rust and libdbus). `release.yml` exists. **Fix:** push a
  `v0.1.1` tag, then document "download the tarball + check SHA256SUMS".
- **Low: the README's mock-run section is stale.** It lists the old platform/app numbering
  (`dev-002` = platform) and says the UI is at `/ui/app1`, while env-up now serves
  dwigital-shell at the root. **Fix:** refresh the README after the next mock run.

These steps have not yet been run end to end on a fresh VM. The flags were checked against
`--help`, but treat the first run as a dry run (`--no-apply` + `plan`).
