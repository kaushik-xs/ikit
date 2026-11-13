# Host ports

Status: implemented (November 2026): the `ports:` config section, reserved ports, one
allocator behind `svc publish`, route upstream ports and `plan`, host ports that ikit picks for
you, a probe of what is actually listening on the target, port claims shared between orgs on
the same target, `ikit target ports`, port drift and an MCP tool.

## In short

A container listens on its own port (say `80`). To reach it from outside the container, the
port is **published** on the target's host at a **host port** (say `8080`). Two things cannot
use the same host port on one machine, so ikit keeps track of who has which.

ikit knows which host ports are taken on a target:

- ports published by this org's services (`ikit svc publish`),
- loopback ports it handed out to routes (`ikit route add`), and
- loopback ports other orgs' routes use on a shared proxy on that target, and
- whatever the target's host shows in use: processes listening on a port (sshd, a Postgres
  installed on the host, …) and ports Docker publishes for any container, of any org, and
- ports other orgs have **claimed** on that target, even if nothing runs on them yet.

It also keeps a list of ports it **never hands out**:

- **Built in:** `1-1023` (the privileged ports: SSH 22, HTTP 80, HTTPS 443, …) and `9993`
  (ZeroTier).
- **Yours:** whatever you list under `ports.reserved` in the org's `config.yaml`.

You can choose a host port yourself (`8080:80`) or give only the container port (`80`) and
let ikit pick a free one. Either way the mapping is stored on the service, so it stays the same
on every apply.

Ports you reserve are hard limits: `svc publish` refuses them and `plan` blocks a workload that
still publishes one. Built-in reservations are softer: ikit never picks those ports on its own,
but you can still publish one on purpose (for example `80:80` on a box with no proxy).

## Worked example: expose two frontends, keep the rest internal

A common shape: one workload runs a web UI, its API and a handful of backend services. Only
the UI and the API the browser calls need to be reached from outside the machine. Everything
else talks over the workload's network by service name.

Take a workload with these services:

| Service                             | Reached by                          | Host port? |
|-------------------------------------|-------------------------------------|------------|
| `client-ui-next`                    | the browser                         | yes        |
| `launchpad`                         | the browser and `client-ui-next`    | yes        |
| `authrs`, `core`, `workflow-engine` | other services only                 | no         |
| `postgres`, `redis`                 | other services only                 | no         |

Services inside one workload already reach each other by name on the workload's network,
with no host port involved:

```yaml
# client-ui-next's env
LAUNCHPAD_API_URL: http://launchpad:3000/   # server side: internal service URL
```

So you publish only the two services the browser needs, giving just the container port
(both listen on 3000 inside their containers):

```
$ ikit svc publish client-ui-next 3000
Picked host port 10000 for container port 3000 (10000:3000).

$ ikit svc publish launchpad 3000
Picked host port 10001 for container port 3000 (10001:3000).

$ ikit apply
```

The result:

```
browser ──► host:10000 ──► client-ui-next:3000
browser ──► host:10001 ──► launchpad:3000

client-ui-next                ──► http://launchpad:3000          (workload network)
authrs, core, workflow-engine ──► postgres:5432, redis:6379      (workload network)
```

`postgres`, `redis` and the backend apps get no host port, so nothing outside the machine can
reach them.

What the port feature does for you here:

- **No guessing.** You don't have to know whether 3000 is already taken by another org's app or
  a dev server on the host. ikit checks what is in use and hands out a free port.
- **The port stays.** 10000 and 10001 survive every `apply` and `svc upgrade`, so anything that
  points at them (a browser-side env, a firewall rule, a bookmark) keeps working.
- **No clashes with other orgs** sharing the machine: they won't be given 10000 or 10001, and
  you won't be given theirs.
- **One place to look.** `ikit target ports <target>` shows both ports and who holds them.

One thing to update by hand: a value the **browser** uses must point at the host port, not the
container port. If `client-ui-next` has `NEXT_PUBLIC_LAUNCHPAD_API_URL=http://localhost:3001/`
and launchpad's host port is now 10001, change it to `http://localhost:10001/`. Server-side
values like `LAUNCHPAD_API_URL=http://launchpad:3000/` don't change.

Choosing the ports yourself (`ikit svc publish launchpad 3001:3000`) still works; ikit then
checks the port is free instead of picking one.

### Same services, behind a domain

If the two frontends should be reached at `app.example.com` and `launchpad.example.com`
rather than `host:port`, use routes instead of `svc publish`. The target's proxy takes HTTPS on
443 and forwards to a loopback port that ikit also picks (from `edge.upstream_ports`, default
`20000-29999`):

```
$ ikit route add client-ui-next --host app.example.com --port 3000
$ ikit route add launchpad --host launchpad.example.com --port 3000
```

Nothing is published on a public interface then. Rule of thumb:

| How the service is reached     | What to run                          |
|--------------------------------|--------------------------------------|
| by domain name                 | `ikit route add`                     |
| by the machine's IP and a port | `ikit svc publish <container port>`  |
| only by other services         | nothing                              |

## Configuration

`orgs/<org>/config.yaml`; the whole section is optional:

```yaml
ports:
  range: [10000, 19999]               # host ports ikit hands out to services (default)
  reserved: [5432, 6379, 9000-9100]   # never published, never handed out
  probe: true                         # check what listens on the host (default)
```

- `range` is where picked host ports come from; ikit takes the lowest free one.
- The default `10000-19999` stays below Linux's ephemeral range (32768-60999), where the kernel
  hands out temporary ports for outgoing connections and Docker picks random host ports, so a
  port ikit picked is not taken by one of those later.
- `range` must start above 1023 and must not overlap `edge.upstream_ports` (default
  `20000-29999`, the range route upstreams come from), so service ports and route ports never
  collide.
- `reserved` takes single ports and spans; `9000-9100` needs no quotes.
- `probe: false` skips asking the host and stops writing this org's claim file; ikit then only
  knows its own records.

## Using it

### Let ikit pick the host port

Give only the container port:

```
$ ikit svc publish api 80
Picked host port 10000 for container port 80 (10000:80).
api publishes 10000:80. Run `ikit apply` to make it take effect.
```

| You write          | Meaning                                                        |
|--------------------|----------------------------------------------------------------|
| `80`               | container port 80, ikit picks the host port, every interface   |
| `127.0.0.1::5432`  | container port 5432, ikit picks the host port, this host only  |
| `53/udp`           | UDP; UDP and TCP ports are counted separately                  |
| `8080:80`          | you pick: host port 8080 to container port 80                  |
| `127.0.0.1:5432:5432` | you pick, on one address                                    |

Publish every port the registry declares for the service at once:

```
$ ikit svc publish traefik --auto                    # every interface
$ ikit svc publish postgres --auto --bind 127.0.0.1  # this host only
Picked host port 10001 for container port 5432 (127.0.0.1:10001:5432).
```

Publishing stays opt-in: `sg add` never publishes anything by itself.

A picked port **sticks**. Running the same command again keeps the port the service already
has (same container port, protocol and address), and `svc upgrade`, `plan` and `apply` never
change it. To move it, remove it and publish again:

```
$ ikit svc publish api 80 --remove 10000
```

ikit skips a port when any workload of this org on the target publishes or routes to it, when
another org's route on the target's proxy uses it, when the host shows it in use (see below), or
when it is reserved. If the range is full,
`svc publish` fails with `no free host port in 10000-19999 on vm-1; widen ports.range`.

Services in the same workload don't need published ports to reach each other: they use the
container port on the workload's network (`from: service:api.port`). Publish only what has to
be reached from outside the target.

### Reserve ports

Reserve the ports your hosts use for other things:

```yaml
# orgs/acme/config.yaml
ports:
  reserved: [5432, 9000-9100]   # a host Postgres, a monitoring agent
```

Publishing a reserved port fails straight away:

```
$ ikit svc publish api 5432:5432
error: `api` cannot publish host port 5432: it is reserved in config.yaml (ports.reserved: 5432)
```

A workload that published a port before it was reserved is caught by `plan`:

```
$ ikit plan
blocked: `api` publishes 9050:80, but host port 9050 is reserved in config.yaml
(ports.reserved: 9000-9100); move it with `ikit svc publish api <port>:80 --remove 9050 --wl wl-acme-dev-001`
```

`plan` also blocks a host port another of this org's workloads on the same target already
publishes or routes to, and one another org's route uses on the target's proxy:

```
blocked: `web` publishes 20000:80, but host port 20000 on `vm-1` is already the route upstream
of `site` of wl-acme-prod-001 (127.0.0.1:20000:80); move it with `ikit svc publish web <port>:80 ...`
```

Routes pick their loopback upstream port from `edge.upstream_ports` and skip every taken or
reserved port, so `route add` never lands on a port you reserved.

## How ikit sees what is in use on the host

Before it picks a port (`svc publish`, `route add`) and on every `plan` and `apply` of a
workload that publishes ports, ikit runs a short read-only script on the target (locally, or
over SSH for a remote VM):

- `ss -tulnp` lists every listening TCP and UDP socket, with the process name.
- `docker ps` lists the ports Docker publishes for every container, with its name and Compose
  project. This also covers containers whose ports `ss` cannot see.

A port in use is skipped when picking, and blocks `plan` when a service asks for it:

```
blocked: `api` publishes 5432:80, but host port 5432 on `vm-1` is already in use by container
`salestracker-db-1` (compose project `salestracker`) (0.0.0.0:5432); move it with
`ikit svc publish api <port>:80 --remove 5432 --wl wl-acme-dev-001`

blocked: `api` publishes 2222:22, but host port 2222 on `vm-1` is already in use by `sshd` on
the host (0.0.0.0:2222); ...
```

Good to know:

- The workload's **own** running containers never block it, because `apply` replaces them. So
  re-planning an applied workload is fine.
- `apply` probes again just before it deploys, so a port taken after `plan` still stops it with a
  clear message, not Docker's bind error.
- Process names need root or passwordless `sudo` on the host; without them ikit still sees the
  port and says "a process on the host".
- If the probe fails (no SSH, a timeout), `plan` shows a warning and continues with ikit's own
  records. If `ss` is missing, only container ports are checked; install `iproute2`.
- Kubernetes targets are not probed.

## Sharing a target with other orgs

The probe only sees ports that are in use **now**. Another org may have published a port and
not applied yet, or destroyed its workload and plan to bring it back. So each org leaves a note
on the host saying which ports are its own: a **claim file**.

- ikit writes the org's claim file whenever its ports on that target change (`svc publish`,
  `svc rm`, `route add`/`rm`, automatic routes, `wl rm`) and before every `apply`. The file lists
  every host port of every workload of the org on that target, running or not.
- Other orgs read it with the probe: a claimed port is skipped when picking and blocks `plan`:

  ```
  blocked: `api` publishes 31000:9000, but host port 31000 on `vm-1` is already claimed by org
  `globex` for `nginx` of wl-globex-dev-001 (31000:80); move it with ...
  ```

- Where the files live: `/var/lib/ikit/ports/<org>.json` (created as a shared directory, like
  `/tmp`, the first time ikit runs there with root or passwordless `sudo`). Without it, ikit
  writes `~/.local/state/ikit/ports/<org>.json` under the SSH user's home; with root, ikit also
  reads those home files of every user.
- Each org only writes its own file, so two orgs never overwrite each other. A file that cannot
  be written (the host is offline) only warns; the next change or `apply` writes it again.
- Your own org's file is never read back: your own records are the source of truth.

## See what is in use: `ikit target ports`

```
$ ikit target ports vm-1
┌───────┬───────┬───────────┬───────────┬──────────────────────────────────────┐
│ PORT  ┆ PROTO ┆ ADDRESS   ┆ KIND      ┆ HELD BY                              │
╞═══════╪═══════╪═══════════╪═══════════╪══════════════════════════════════════╡
│ 22    ┆ tcp   ┆ *         ┆ process   ┆ sshd                                 │
│ 5432  ┆ tcp   ┆ *         ┆ container ┆ salestracker-db-1 (salestracker)     │
│ 20000 ┆ tcp   ┆ 127.0.0.1 ┆ route     ┆ wl-acme-prod-001 / web (route)       │
│ 10000 ┆ tcp   ┆ *         ┆ service   ┆ wl-acme-dev-001 / api                │
│ 10001 ┆ tcp   ┆ *         ┆ org-claim ┆ org globex: wl-globex-dev-001 / web  │
└───────┴───────┴───────────┴───────────┴──────────────────────────────────────┘
Service ports 10000-19999: 9998 free; next: 10002. Route upstreams: 20000-29999.
Never handed out: 1-1023 (privileged ports (ssh, http, https, …)), 9993 (ZeroTier).
```

| KIND        | Meaning                                                           |
|-------------|-------------------------------------------------------------------|
| `service`   | published by one of this org's services (running or not)          |
| `route`     | loopback upstream of one of this org's routes                     |
| `org-route` | another org's route on the target's shared proxy                  |
| `org-claim` | claimed by another org (see above), running or not                |
| `container` | Docker publishes it for a container ikit does not manage          |
| `process`   | a process listening on the host (name shown with root or sudo)    |

`ADDRESS` `*` means every address. `-o json` gives the same as JSON. The MCP server has the same
view as the `target_ports` tool; a client limited to some workloads sees the org's other
workloads as "another workload".

## Port drift

`ikit status --drift` reports a `port` item when a host port the workload publishes (or routes
to) is now held by something else on the target, a host process or another container:

```
nginx  port  host port 10000 (10000:80) is in use by `sshd` on the host (0.0.0.0:10000), not by this workload
```

The workload's own containers holding its ports is the normal case and is not drift.

## Phases

1. ~~Allocator, `ports:` config, reserved ports.~~ Done.
2. ~~Automatic host ports: `svc publish api 80`, `--auto`, sticky ports.~~ Done.
3. ~~Host probe: listening sockets and container ports on the target.~~ Done.
4. ~~Cross-org claims: one claim file per org on the host.~~ Done.
5. ~~`ikit target ports <target>`, port drift, MCP tool.~~ Done.

Kubernetes targets are out of scope: they have no host ports in this sense.
