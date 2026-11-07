# Issues

## `srmctl status` shows zeros straight after the daemon starts

Run `srmctl daemon stop`, `daemon start`, then `status` at once: it prints
`CPU 0% of 0 cores, 0 MiB of 0 MiB memory`. Clients can connect before the first sample
lands, which takes a second or two. Harmless, but it reads as a broken daemon.

## Watch the daemon's handle count over days

`srmd` held about 870 handles after start, mostly `sysinfo`'s per-process handles, and 901
after a minute. That isn't evidence of a leak, but it was only measured for a minute.
Check `Get-Process srmd | Select HandleCount` after a few days of real use.

## Fleet view repeats a disk reading

A degraded asset shows `disk 93% full · disk 93% · cpu 12%`: the probe's warning and the
gauge say the same thing (`app/src/lib/FleetView.svelte`).

## Tray app and daemon stopped without a trace

`srm-tray` started 2026-10-30 5:35 PM PDT (00:35Z) and was gone by 2026-10-31 2:31 PM PDT
(21:31Z). `srmd` restarted at 9:26 PM PDT (04:26Z), and its log has no `stopping` line before
that, so it didn't exit normally. There was no reboot, Application log entry or WER report.
The T3 Code host restarted around 6:33 PM PDT (01:33Z), which may be related but isn't proven.
`IsProcessInJob` can't settle it: Explorer and every desktop app here are in a job too. A Rust
panic exits quietly, and `srm-tray` writes no log, so its next exit would be just as blank.

## Nothing starts the tray app for you

`scripts/install.ps1` copies `srm-tray.exe` but doesn't start it, there's no Start Menu entry,
and "Start at login" is off by default. So after any exit the only way back is running
`~/.srm/bin/srm-tray.exe` by hand, and an app with no icon gives no hint that it's missing.

## "Outside the budget" may list long-running agent hosts

Right after cutover it showed `node bin.js`, pid 17116, 155 processes, 11.3 GiB, 1.5 cores.
That looks like an agent host rather than a build. The rule is any build-tool name staying
above one core (`BUILD_TOOLS` in `crates/srmd/src/sampler.rs`), so `node` and `bun` match
long-lived servers. It went away by itself, but expect some noise until the rule is tuned.

## The frozen-child test still leaves a frozen child sometimes

`killing_the_wrapper_while_it_starts_a_child_leaves_nothing_frozen` (`crates/srmctl/tests/broker.rs`)
failed 2 of 4 runs on Noook on 2026-11-07 (master `51ffb62`, run inside `srmctl run`), with
`left: "1" right: "0"`. Each failure left a `node.exe -e setTimeout(...,'frozen-<pid>')` behind with
its thread `Suspended`, holding the test's inherited stdout open, so the `cargo test` pipe never
closed until that process was killed by hand. So the window that `93e8f2b` closed can still be hit.
CI skips this test until it's fixed.
