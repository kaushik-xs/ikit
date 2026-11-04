---
name: ikit-org-illustration
description: Draw one ikit org (registry, targets, workloads, service groups, start order, runs, keychain secret names) as an HTML page plus light and dark PNGs in org_illustrations/. Use when the user asks for an illustration, diagram, picture, map or PNG of an ikit org or its workloads, e.g. "illustrate the regere org", "make a png of org acme", "redraw the org diagram".
---

# ikit org illustration

Turns the live state of one ikit org into a single-page diagram and renders it to PNG.
Output goes to `org_illustrations/` at the repo root, which is git-ignored, because the
picture contains org-specific names, images and secret env names.

## Inputs

- **Org name.** If the user doesn't give one, list `~/.ikit/orgs/` (or `$IKIT_HOME/orgs/`)
  and ask which org. If there is only one, use it.

## Steps

1. **Collect state.** Run `.claude/skills/ikit-org-illustration/scripts/collect.sh <org>`.
   It reads `config.yaml`, `state.db` (read-only) and each `deploy/<workload>/compose.yaml`,
   and prints registry, logins, targets, profiles, storages, ZeroTier networks, workloads,
   services (with secret env *names*), hooks, workload networks, runs, locks, the backup
   count, each target's reverse proxy (`edge_json`), routes, redirects and auto-route
   settings, and per workload `depends_on` edges, Compose networks and published ports.
   Secret values live in the OS keychain; never try to read them.
   If something needs more detail, `cat ~/.ikit/orgs/<org>/workloads/<wl>.yaml` too.

2. **Write the page** to `org_illustrations/<org>-org.html`, starting from
   `.claude/skills/ikit-org-illustration/template.html`. The template is the layout and
   style reference, filled with made-up data for an org called `acme`; replace **every**
   value with collected data. Nothing from the template's example data may survive.
   Keep its system:
   - One `<svg viewBox="0 0 1400 H">` inside `.diagram`; header (eyebrow with org name and
     today's date, `<h1>` display name, one-paragraph lede) above it in HTML.
   - Org frame. Left column "Org-wide": Registry (services grouped app / platform / data,
     with versions; registry logins by host only), each Target (kind, connection type,
     status dot, last check, and the workloads on it or "no workloads yet"), and a dashed
     "Not set up yet" box for profiles, storages,
     ZeroTier networks, backups or MCP tokens that don't exist. Things that *do* exist get
     a real box instead.
   - Edge. In the Org-wide column, an "Edge defaults" box from the `edge:` section of
     `config.yaml`: the ACME email (`edge.acme_email`, the org's account for every
     target's certificates), base domain, host template and auto-route groups; leave out
     keys that aren't set and say "no ACME email" if it's missing. On each target box
     that has `edge_json`, a proxy line: kind, version, ACME CA (production or staging).
     A target without one gets "no reverse proxy" (or, if you checked the host and found
     one ikit hasn't adopted, say that). When routes or redirects exist, draw a
     "Routes" box per workload frame: host[/path] → service:port, TLS mode, and an arrow
     from the target's proxy line to it.
   - One workload frame per workload: name, status pill, env, target. Inside it one box
     per service group that exists (app blue, platform amber, data green); say which group
     is missing. One node per service: name, short image:tag, one useful fact (host port,
     volume, pinned version), secret count.
   - Edges: solid `.dep` arrows for `depends_on` between services in the same group; dashed
     `.call` arrows only where an env value clearly points at another service's host
     (e.g. `http://launchpad:3000/`). Don't draw every app → data edge: put "used by N
     apps" on each data node and one short arrow per data node. Add a small legend in
     the app group's header row.
   - Bottom row: Runs (count, ok/failed, one dot per run in order, latest result and time,
     hooks, lock), Compose networks (and external Docker / ZeroTier links), Deploy files.
   - Dashed purple keychain band: secret env names per service and the key pattern
     `org/<org>/workload/<wl>/<service>`. Names only, never values.
   - Arrows from the workload frame to the left column: "pins versions" to Registry,
     "runs on" to its Target.
   - Times in UTC with the date, e.g. "4 Nov 18:54 UTC".

   Layout for several workloads or many services: grow the viewBox height and stack
   workload frames vertically; widen nothing past 1400. Keep nodes 200×104 on a grid
   with ≥30px gaps, and route curves so they don't cross text. Keep `text` inside boxes
   with ≥16px padding; mono text at 12px fits about 28 characters in a 200px node.

3. **Render.** `.claude/skills/ikit-org-illustration/scripts/render.sh org_illustrations/<org>-org.html [height]`
   writes `<org>-org.png` (light) and `<org>-org-dark.png` at 2× with headless Chrome.
   `height` is the window height in CSS px (default 990); raise it if the diagram is
   taller, lower it if there is empty space at the bottom.

4. **Check the PNG.** Open the light PNG with the Read tool and look for overlapping text,
   arrows crossing labels, clipped boxes, collapsed spacing in mono lines (use
   `<tspan dx="22">` rather than repeated spaces), and blank space at the bottom. Fix
   and re-render until it is clean.

5. **Report** the three file paths and anything you simplified. Point out that the image
   shows secret env names (not values) before the user shares it.

## Notes

- Only local targets have `deploy/<wl>/compose.yaml` on this machine; for SSH targets the
  compose project is on the remote host, so take start order from the workload spec or
  say it's not shown.
- Don't commit anything in `org_illustrations/`.
