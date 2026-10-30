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

## "Outside the budget" may list long-running agent hosts

Right after cutover it showed `node bin.js`, pid 17116, 155 processes, 11.3 GiB, 1.5 cores.
That looks like an agent host rather than a build. The rule is any build-tool name staying
above one core (`BUILD_TOOLS` in `crates/srmd/src/sampler.rs`), so `node` and `bun` match
long-lived servers. It went away by itself, but expect some noise until the rule is tuned.
