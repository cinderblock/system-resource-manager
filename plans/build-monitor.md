# Build monitor / resource broker

## Goal

Replace the `compute-budget` skill's file-based broker with a daemon that also gives live
insight into what is building: who holds CPU slots, who is waiting, what each claim actually
spends, and what heavy work is running *outside* the broker. Several Claude sessions plus the
user share 6 physical cores (12 logical), and contention both hurts interactivity and produces
false test failures.

Multi-system aware: machines announce their state, and non-compute network resources (NAS,
cameras, routers, switches) appear in the same view.

## Environment / context

- Machine: Windows 11, 6C/12T. Windows-first, no WSL dependency. Servers run Ubuntu.
- Old broker: `~/.claude/bin/cpu-slots.mjs` (CLI + importable `acquire()`), 360 lines.
  - Budget: `~/.claude/compute/config.json` -> `{ "slots": 4 }`.
  - Claims: one JSON per claimant in `~/.claude/compute/claims/`, pruned by PID liveness.
  - Mutex: exclusive-create `~/.claude/compute/.mutex`, stale after 30 s.
  - Waiters poll every 500 ms; `--low` sets BELOW_NORMAL priority on the child.
- Skill doc: `~/.claude/skills/compute-budget/SKILL.md`.
- Known consumers, all via `~/.claude/bin/cpu-slots.mjs` by path:
  - `svdsa.org/tests/compute-budget.ts`: Playwright globalSetup; imports `acquire`,
    `describeWait` and a **synchronous** `budget()`, and uses `release.waitedMs`.
  - `t3code/scripts/start-t3.ps1`: `node ... run --slots 2 --low --label ... -- vp run build:desktop`,
    and depends on the exit code passing through.
  - READMEs in several projects show the `run` form.
- `~/.cargo/config.toml` caps cargo at `build.jobs = 4` and shares `build-dir`; its comment
  references the old config path.
- Toolchain: rustc 1.99, tauri-cli 2.10.1, bun 1.4.2, node 26, WebView2 156.
- Existing fleet data: Prometheus + Grafana and node-exporter on the servers, and a self-hosted
  tailnet giving each machine a stable name.
- Reference for conventions: `claude-usage` (Tauri 2 + SvelteKit tray app).

## Limitations of the old broker (why change it)

1. **Waiters are invisible.** Only holders write a claim file; `status` can't show the queue.
2. **No fairness.** Every waiter polls; whoever wins the mutex after a release gets in.
   A 4-slot request can starve behind a stream of 1-slot requests.
3. **No insight into spend.** A claim says "3 slots" but nothing measures what the tree
   actually uses, so mis-sized claims go unnoticed.
4. **Unbrokered work is invisible.** A build run without the wrapper doesn't show up at all.
5. **Priority race.** `--low` sets priority after spawn; early grandchildren stay Normal.
6. **Leases tied to PID polling** rather than to a connection: works, but slow to free.

## Architecture

### Binaries (one Cargo workspace)

- **`srmd`**: the daemon. Same binary everywhere: broker + sampler locally, and an optional
  network face.
- **`srmctl`**: the CLI (`run`, `status`, `watch`, `history`, `daemon start|stop|status`). Named
  after systemd's `*ctl` deliberately: a bare `srm` collides with secure-delete's `srm` on
  Linux, and an agent typing that with a path would destroy files.
- **`cpu-slots.mjs`**: stays at `~/.claude/bin/`, same exports. A thin client over the pipe; its
  `run` delegates to `srmctl`. Repos load it by path, so none of them need to change.
- **Tray app**: Tauri 2 + SvelteKit (adapter-static) on Bun, matching `claude-usage`. Just
  another client: closing it never affects brokering.

### Local broker

- Transport: named pipe `\\.\pipe\srmd` (Unix socket on Linux), newline-delimited JSON.
  `first_pipe_instance` makes "exactly one daemon" atomic, so a stampede of auto-spawning clients
  resolves itself. The pipe has an explicit DACL so a daemon started from an elevated shell still
  serves unelevated clients.
- **A lease is the connection.** Connection closes (release, exit, crash, kill) -> slots free. No
  polling and no PID liveness checks.
- Scheduling: FIFO with a **bounded backfill**. While the head of the queue has waited less than
  `backfill_grace_secs` (default 30 s), smaller requests may pass it into free slots, so capacity
  isn't left idle. After that the head reserves: nothing new starts until it fits. Worst-case wait
  for a big request is bounded by the longest running job plus the grace period. Pure function,
  unit-tested.
- **`srmctl run` owns a Windows Job Object** around the child: created suspended, assigned,
  resumed. The job's priority-class limit applies to every process in it, closing the `--low` race
  outright. `srmctl` reports the job's exact PID list and cumulative CPU time to the daemon over
  the lease connection, so the daemon never needs a handle to the job, which sidesteps session and
  elevation boundaries. Job accounting includes processes that already exited, which a sampler
  would miss for short-lived `rustc`s. No `KILL_ON_JOB_CLOSE`: build tools legitimately leave
  servers behind (sccache, mspdbsrv, gradle).
- Daemon unreachable -> run unbrokered with a loud warning. Daemon dies mid-lease -> client keeps
  running and re-adopts its lease when one returns.

### Insight

- `sysinfo` sampling at 2 s while there are leases or watchers, 5 s when idle.
- Per lease: live cores, RSS, process count, top processes with useful names
  (`rustc --crate-name` etc), repo, Claude session.
- **Unbrokered:** heavy build-tool process trees not inside any lease.
- **History:** SQLite of finished leases, so `srmctl history` can say "claimed 4, used 1.3".

### Multi-system

A **Node** is a machine running `srmd`. An **Asset** is anything else on the network. Three tiers,
cheapest to observe last:

| Tier | For | How it is seen | Install needed |
|---|---|---|---|
| Agent | Machines that run builds (workstation, build-box, other PCs) | Full `srmd`, its snapshot pulled by peers | `srmd` |
| Exporter | Servers | A probe reads their existing `node_exporter` / Prometheus | Nothing |
| Device | NAS, cameras, routers, switches | A probe from any `srmd`: TCP, HTTP, later SNMP / UniFi | Nothing |

- `srmd` is the same code on every Agent; there is no separate thin client. Devices can't run
  anything, so they are represented by probes, and any node can host a probe.
- **Network face is off by default.** When enabled, `srmd` serves read-only `GET /v1/snapshot` and
  Prometheus `GET /metrics` over HTTP with a bearer token. It requires a token for any non-loopback
  bind and is meant to bind the tailnet address, which gives encryption and identity. Servers not
  on the tailnet can use the existing Caddy TLS + basic-auth pattern, which peer URLs support.
- **Discovery:** mDNS (`_srmd._tcp`) finds peers on the same LAN. mDNS does not
  cross subnets or the tailnet, so a static `[[peers]]` list covers everything else. That list is a natural thing for
  infrastructure tooling to generate later.
- **Pull, not push.** mDNS announces existence; state is pulled by whoever is interested, so a node
  with no watchers does no network work.
- **Generalised resources.** The broker's unit is a named pool, not "CPU". `cpu` is simply the
  default pool. `[resources.camera-warehouse] capacity = 1` gives an exclusive lock, which is where
  `workspace-contention`'s ports / cameras / COM devices can eventually move. Leases are connections,
  so a future authenticated remote lease is the same protocol over TCP. Not built yet: the network
  face stays read-only until there's a real need.

### Not in scope (noted so the design doesn't preclude them)

- Remote leases and "run this build on build-box" placement.
- SNMP and UniFi probe adapters (the probe enum is the extension point).
- TUI: `srmctl watch` covers the terminal.

## Decisions already made (don't re-ask)

Answered 2026-10-30:

1. **The daemon replaces the claims directory.** No need to keep the old implementation.
2. **Daemon is a separate program** from the tray app.
3. **Named pipe**, leases bound to the connection.
4. **Lives here** in `system-resource-manager`, `git init` now, GitHub repo later.
5. **Multi-system aware from the start**; user has no preference between same-code or thin client.
   Chosen: same `srmd` everywhere, probes for what can't run it.

## Plan / steps

1. [x] Agree architecture.
2. [ ] `git init`; workspace; `srm-proto`. <- **current**
3. [ ] Broker core + daemon + `srmctl run/status`, with Job Object.
4. [ ] `cpu-slots.mjs` shim, verified against svdsa's usage.
5. [ ] Sampler, unbrokered detection, history.
6. [ ] Network: snapshot/metrics, peers, mDNS, probes (tcp, http, prometheus).
7. [ ] Tauri tray app.
8. [ ] Cutover: install to `~/.srm/bin`, replace `cpu-slots.mjs`, rewrite the skill, README.

Deploying `srmd` to any server, listing real devices, opening a firewall port, or joining this
machine to anything is a separate per-change authorization. See Things not to do.

## Findings / gotchas

- `budget()` must stay synchronous for svdsa, so the shim can't ask the daemon for it. It reads
  `~/.srm/config.toml` directly and caches what the daemon reports on acquire.
- `node ... cpu-slots.mjs run` is what t3code's launcher calls, so exit-code passthrough is API.

## Progress log

- [x] Read existing skill + broker
- [x] Architecture agreed (2026-10-30)

## Open questions for the user

1. GitHub repo: visibility and when to create it? *Recommend: private until first use on a second
   machine.*
2. Which assets and peers go in the real config? Nothing is listed until you say so.

## Things not to do

- Don't make the broker a hard dependency: unreachable daemon -> run unbrokered + warn.
- Don't break the `cpu-slots.mjs` import API; repos load it by path from `~/.claude/bin`.
- Don't run file-based and daemon brokers side by side: split accounting is worse than none.
- Don't deploy to servers, change firewalls, or add infrastructure config without per-change approval.
  Everything multi-system is tested with two local daemons on loopback.
- Don't name anything `srm` on the command line (see `srmctl` above).
- Don't use `KILL_ON_JOB_CLOSE` on the build job.
