---
name: compute-budget
description: |-
  Anytime you have a large compute intensive task, like compiling a large project from scratch (not incremental), or running a large browser test suite, or other intensive task.
---
This is a working computer, not a build farm — several Claude
sessions, several projects and a person share six physical cores, and
nothing else stops two test suites from starting at once and overloading the system

Overloading it is not acceptable, and not only because it makes the
machine unusable: contention makes RESULTS WRONG. Under load an
ordinary wait becomes a timeout, and a timeout reads exactly like a
regression.

# compute-budget

One number for the whole machine, and a broker that makes commands wait
their turn instead of piling on.

| Thing | Path |
|---|---|
| CLI | `~/.srm/bin/srmctl` |
| Importable API | `~/.claude/bin/cpu-slots.mjs` |
| Budget | `~/.srm/config.toml` → `[resources.cpu] capacity = 4` |
| Daemon log | `~/.srm/srmd.log` |
| Source | `~/git/Personal Projects/system-resource-manager` |

The daemon, `srmd`, starts itself the first time anything needs it. A lease
is an open connection, so a crashed or killed run frees its slots at that
moment. If the daemon can't be reached, commands warn and run anyway.

**It blocks rather than refuses.** A queued command waits and says what
it's waiting for and who holds the slots. Nothing is ever lost to
contention, and nothing stalls silently.

## Using it

Wrap the command:

```bash
~/.srm/bin/srmctl run --slots 3 --label "svdsa playwright" -- bun run test
```

`node ~/.claude/bin/cpu-slots.mjs run ...` is the same thing, and takes the
same flags. Repos use that form because it degrades gracefully on machines
without `srmctl`.

Add `--low` to anything long: the whole process tree runs below normal
priority and yields to whatever the person is doing.

See who holds what, who's queued, and what's running outside the budget:

```bash
~/.srm/bin/srmctl status
```

Check whether your slot counts are right, from what past runs measured:

```bash
~/.srm/bin/srmctl history
```

Or claim from inside a config/script, which is better when the tool
already knows its own parallelism — the claim then matches what is
actually spent, and it covers every way of invoking the tool rather than
just one npm script:

```js
import { homedir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const { acquire } = await import(
  pathToFileURL(join(homedir(), ".claude/bin/cpu-slots.mjs")).href
);
const release = await acquire(workers, "myproject tests");
try { /* … */ } finally { release(); }
```

`svdsa.org` does it this way — `tests/compute-budget.ts`, wired in as
Playwright's `globalSetup`, claiming one slot per worker. Copy that file
as the template for other repos.

## How many slots

Ask what the command actually spends, not what it could use.

| Work | Slots |
|---|---|
| A browser test worker | 1 each — claim your worker count |
| A `cargo` build, check, or test | 4 — matches `build.jobs` (see below) |
| A production build / bundle | 2 |
| A typecheck, lint, or format pass | 1 |
| Something single-threaded | 1 |

Where a tool has its own configured parallelism, the claim must equal it.
Claiming 2 while spending 4 is worse than not claiming at all — the
broker then hands out slots it does not have, and reports a machine that
looks half idle while it is pegged. `srmctl status` marks a run spending
more than it claimed, and `srmctl history` shows the average.

A request bigger than the whole budget is clamped, so it can't deadlock
waiting for slots that will never exist.

## Also cap the tool's own parallelism

The budget stops runs colliding; it does not stop ONE run from being too
greedy, because most tools default to one worker per core:

- **Playwright** defaults to half the *logical* cores locally. Set
  `workers` explicitly (svdsa uses 3, overridable with `PW_WORKERS`).
- **Jest / Vitest** — `--maxWorkers`.
- **make / ninja** — don't use a bare `-j`; give it a number.
- **cargo** is already capped machine-wide: `~/.cargo/config.toml` sets
  `[build] jobs = 4`. Cargo turns that into a jobserver token pool that
  rustc's codegen threads also draw from, so it is a real cap and not
  just a process count — and `rust-analyzer`'s background `cargo check`
  inherits it too. Don't override it with `-j` or `CARGO_BUILD_JOBS`
  without a reason. It does *not* cover linker threads, `RUST_TEST_THREADS`
  at test runtime, or build scripts that shell out to `cmake -j`.

Do both. A capped run that still collides with another capped run is
back where it started.

## Beyond CPU

`cpu` is just the default pool. Any other pool is declared in
`~/.srm/config.toml`, and capacity 1 makes it an exclusive lock:

```bash
~/.srm/bin/srmctl run --pool camera-warehouse --label "yard stream" -- ...
```

## Things not to do

- **Don't start a second run "just to check something"** while one is
  going. That is the exact mistake this exists for. Use `status` first.
- **Don't background a heavy run and then start more work** that also
  needs the machine. Background runs still hold their slots — which is
  right — so plan on waiting.
- **Don't raise the capacity to make something finish sooner.** The number
  is the headroom the person using the machine needs. Raising it moves the
  cost onto them.
- **Don't bypass the broker for "just this once".** It only works if
  everything heavy goes through it, and `status` will show that you didn't.
- **Don't treat a failure under load as a real failure.** Re-run it
  alone before believing it, and say which you did when reporting.
