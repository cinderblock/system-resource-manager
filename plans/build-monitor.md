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
  Prometheus `GET /metrics` over HTTP. It always requires a token (`X-Srm-Token`, or a bearer token
  for Prometheus) and is meant to bind the tailnet address, which gives encryption and identity. Servers not
  on the tailnet can use the existing Caddy TLS + basic-auth pattern, which peer URLs support.
- **Discovery:** mDNS (`_srmd._tcp`) finds peers on the same LAN. mDNS does not
  cross subnets or the tailnet, so a static `[[peers]]` list covers everything else. That list is a natural thing for
  infrastructure tooling to generate later.
- **Pull, not push.** mDNS announces existence; state is pulled by whoever is interested, and a node
  with no watchers polls six times less often.
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
6. **Publish on GitHub, public,** provided it holds no personal info.
7. **This machine only for now.** Other machines come later, each as its own approved change.

## Plan / steps

1. [x] Agree architecture.
2. [x] `git init`; workspace; `srm-proto`.
3. [x] Broker core + daemon + `srmctl run/status`, with Job Object.
4. [x] `cpu-slots.mjs` shim, verified against svdsa's usage.
5. [x] Sampler, unbrokered detection, history.
6. [x] Network: snapshot/metrics, peers, mDNS, probes (tcp, http, node_exporter).
7. [ ] Tauri tray app: release build installed; needs a first real run. <- **current**
8. [x] Cutover: installed to `~/.srm/bin`, `cpu-slots.mjs` replaced, skill rewritten, README (2026-10-30, about 2:10 PM PDT, 21:10Z).

Deploying `srmd` to any server, listing real devices, opening a firewall port, or joining this
machine to anything is a separate per-change authorization. See Things not to do.

## Findings / gotchas

- `budget()` must stay synchronous for svdsa, so the shim can't ask the daemon for it. It reads
  `~/.srm/config.toml` directly and caches what the daemon reports on acquire.
- `node ... cpu-slots.mjs run` is what t3code's launcher calls, so exit-code passthrough is API.

- **Be invisible while working (user request, 2026-10-30).** Windows appearing and beeps were
  distracting while this was being built. The cause wasn't pinned down. Likely the test suites:
  hundreds of console programs per run, each able to open a window from a console-less parent.
  Tests and the shim now use `CREATE_NO_WINDOW` / `windowsHide`. Don't open previews or launch the
  tray app without asking.
- `cargo test` builds a package's binary only for that package's own integration tests, so
  `crates/srmd/tests/binary.rs` exists to keep the daemon fresh for `srmctl`'s tests. Before it,
  every fleet test ran against a stale daemon and failed with "nothing listening".
- The old broker doesn't record waiters, so a queued build looks like nothing is happening: a
  4-slot build sat invisible behind a 1-slot job for 10 minutes. Limitation 1, seen first-hand.
- The old broker can crash outright: `EPERM ... open 'C:\Users\me\.claude\compute\.mutex'` from
  `withMutex`. On Windows, opening a file another process is deleting fails with EPERM, not EEXIST,
  and it only handled EEXIST. It hit 3 runs in a row on 2026-10-30 while another session polled for
  4 slots, recreating the file twice a second, so any session can lose a command this way until
  cutover. The daemon has no such file. Don't patch this; cutover removes it.
- Before cutover, old-broker claims still running aren't counted by the daemon. They end on their
  own, and build tools among them appear under "outside the budget" meanwhile.
- SvelteKit 3: no `svelte.config.js` (options go to the `sveltekit()` plugin, not under `kit`), no
  `$lib`, and it needs TypeScript 6 (TypeScript 7 has no JS API).
- A daemon started through a pipe would inherit that pipe and hang the reader forever. See
  `std_handles` in `crates/proto/src/client.rs`.

- **Allowing job breakaway was a mistake.** `JOB_OBJECT_LIMIT_BREAKAWAY_OK` was set for `sccache`,
  but Cygwin/MSYS `spawn` adds `CREATE_BREAKAWAY_FROM_JOB` whenever the job permits it. So
  `srmctl run -- bash -c "cargo ..."` lost `--low` and its accounting, and showed 0.0 cores for 4
  slots while its `cargo` sat under "outside the budget". Loopback tests couldn't show this, because
  they run `node` directly. Breakaway is now forbidden, and the daemon also unions the reported
  list with the tree walk.
- A release-profile `srm-tray` takes about 22 minutes at 2 slots on a loaded machine. `srmd` and
  `srmctl` alone are far quicker, so rebuild only what changed.

- **The daemon must inherit nothing.** `CreateProcess` with inheritance on passes every inheritable
  handle in the process, not only the three standard ones. Clearing the flag on our own standard
  handles wasn't enough: shell -> node -> `srmctl` -> `srmd` carries the shell's pipe as a plain
  inherited handle, and the daemon then keeps it open for life, hanging whatever reads it. Seen as a
  test run stuck 30 minutes after finishing; a watchdog named `target\\debug\\srmd.exe` as the holder.
  `spawn_daemon` now calls `CreateProcessW` with `bInheritHandles = FALSE`.
- **Reconnecting clients must not start a daemon.** It undoes `srmctl daemon stop`, which is how
  that test's daemon outlived its test.
- CPU rate has to be measured across the client's own reports. Measured against the sampler's
  clock, a tick landing between two 2-second reports read as 0.0 cores.

- **Losing the daemon while queued must not mean "no broker".** Clients did that, so a restart
  released the whole queue at once. Queued clients now rejoin, 3 s later so running work is
  counted first. Found by thinking through a reinstall under load rather than by a test.

- **The stuck pipes had a second cause: frozen children.** `srmctl` starts a child suspended, joins
  it to the job, then resumes it. Killed in that gap, it left the child suspended forever, holding
  every inherited handle. Six `node -e setTimeout(()=>{},15000)` test processes were found hours
  old. The job is now kill-on-close until the child is resumed, and only then cleared. Check for
  these first if a pipe ever hangs again.
- Some runs that looked hung were just compiling: about 300 crates at 2 jobs
  on a loaded machine with output buffered by `grep`. Give tests a time limit, not builds.
- WebView2 ignores `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` under Tauri, because wry sets the
  arguments itself. Inspecting the hidden popup would need `additionalBrowserArgs` in the config.

## Progress log

- [x] Read existing skill + broker
- [x] Architecture agreed (2026-10-30)
- [x] Protocol, broker, sampler, history, `srmctl`, job object, shim, network layer: tested
- [x] Tray frontend reviewed in a browser with demo data
- [ ] Tray app run against a real daemon: built, not launched (user asked for no surprise windows)
- [x] Cutover: old `cpu-slots.mjs` and `SKILL.md` kept in `~/.srm/legacy`

## Open questions for the user

1. Does the tray popup look right? Click the tray icon: it can't be inspected from outside.

## Things not to do

- Don't make the broker a hard dependency: unreachable daemon -> run unbrokered + warn.
- Don't break the `cpu-slots.mjs` import API; repos load it by path from `~/.claude/bin`.
- Don't run file-based and daemon brokers side by side: split accounting is worse than none.
- Don't deploy to servers, change firewalls, or add infrastructure config without per-change approval.
  Everything multi-system is tested with two local daemons on loopback.
- Don't name anything `srm` on the command line (see `srmctl` above).
- Don't use `KILL_ON_JOB_CLOSE` on the build job.
