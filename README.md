# System Resource Manager

A shared working computer needs one thing that nothing provides: a way to stop two test suites
starting at once. Overloading a machine doesn't only make it unpleasant to use, it makes results
wrong, because under contention an ordinary wait becomes a timeout, and a timeout reads exactly like
a regression.

This is a machine-wide budget for heavy work, plus the view of what's using it, and of the other
machines and devices on the network.

| Piece | What it is |
| --- | --- |
| `srmd` | The daemon: broker, process sampler, history, optional network face. The same binary on every machine. |
| `srmctl` | The command line. |
| `cpu-slots.mjs` | How JavaScript reaches the broker, importable by path. |
| Tray app | Tauri 2 + SvelteKit. One more client, so closing it changes nothing. |

## Using it

Wrap anything heavy. It waits for its turn, says who it's waiting for, and passes through the exit
code:

```
srmctl run --slots 3 --label "svdsa tests" --low -- bun run test
```

`--low` runs the whole process tree below normal priority, so it yields to anything interactive.

```
srmctl status             what's running, queued, and outside the budget
srmctl watch              the same, live
srmctl history            what work claimed against what it spent
srmctl daemon start|stop|reload|status
```

From JavaScript, when a tool already knows its own parallelism:

```js
const { acquire } = await import(pathToFileURL(join(homedir(), ".claude/bin/cpu-slots.mjs")).href);
const release = await acquire(workers, "myproject tests");
try { /* ... */ } finally { release(); }
```

The daemon needs no setup. Whichever client first finds it missing starts it, and it keeps running.
If it can't be reached, commands run anyway with a warning: a courtesy layer must never be the reason
a repo can't build.

## How it works

**A lease is a connection.** Clients talk to `srmd` over a named pipe (`\\.\pipe\srmd`, or a Unix
socket), one JSON object per line. There's no release message: when a client exits, crashes or is
killed, its connection closes and its slots come back at that moment. If the daemon itself restarts,
running work keeps going and rejoins the count.

**The queue is fair, without wasting slots.** Requests are first come, first served, except that
smaller ones may pass a big one into free slots while the big one has waited less than the grace
period (30 s). After that it reserves: nothing behind it starts, and the running work drains. So a big
request can't starve, and slots don't sit idle during short waits.

**`srmctl run` wraps the command in a Windows Job Object.** Its priority limit reaches every process
in the tree, with no race against the ones started in the first milliseconds. It also gives an exact
process list and a CPU total that includes processes that have already exited, which a sampler can't
see, and short-lived compilers are most of a build. Nothing can leave the job: Cygwin and MSYS break
their children out of any job that permits it, which would lose everything run through `bash -c`. `srmctl history` compares this with what was
claimed, so wrong slot counts are visible.

**Work outside the budget shows up anyway.** The sampler spots build tools staying busy outside any
lease and lists them, with the tree they belong to.

## Configuration

Everything lives in `~/.srm/`: `config.toml`, `srmd.log`, `history.sqlite`, and the installed
binaries in `bin/`. Every setting has a default, so the file can be absent. `srmctl daemon reload`
applies changes, apart from the network listener, which needs a restart.

```toml
[node]
site = "Workshop"

[resources.cpu]
capacity = 4                   # Default: physical cores minus two.

# Any other pool is the same mechanism. Capacity 1 is an exclusive lock:
#   srmctl run --pool camera-warehouse -- ...
[resources.camera-warehouse]
capacity = 1

[broker]
backfill_grace_secs = 30
```

## Multi-system

A **node** is a machine running `srmd`. An **asset** is anything else on the network. There is one
daemon, not a thin variant: machines that do real work run it, and things that can't run it are probed
by any machine that can.

| Tier | For | Seen through | Installed on it |
| --- | --- | --- | --- |
| Agent | Machines that run builds | A full `srmd`, pulled by peers | `srmd` |
| Exporter | Servers | Their existing `node_exporter` | Nothing |
| Device | NAS, cameras, routers, switches | TCP or HTTP probes | Nothing |

**Nothing listens unless asked to.** Watching other machines takes no open ports; only being watched
does:

```toml
[network]
enabled = true                 # Serve GET /v1/snapshot and /metrics. Read-only.
listen = "100.64.0.1:7654"     # Bind the tailnet address: encryption and identity come with it.
mdns = true                    # Announce and browse on the local link.
poll_secs = 10

[[peer]]
url = "http://build-box.tailnet.example:7654"

[[peer]]                       # Behind a reverse proxy with TLS and basic auth.
url = "https://server.example.com/srm"
basic_auth = { username = "srm", password = "..." }

[[asset]]
id = "cam-yard"                # Two nodes using one id are merged, and up wins.
name = "Yard camera"
kind = "camera"                # server, nas, camera, router, switch, access_point, service, other
site = "Workshop"
probe = { type = "tcp", host = "10.0.0.5", port = 554 }

[[asset]]
id = "nas"
kind = "nas"
probe = { type = "http", url = "https://10.0.0.6/" }  # Below 500 is up, so 401 still counts.

[[asset]]
id = "storage"
kind = "server"
probe = { type = "node_exporter", url = "https://server.example.com/metrics", basic_auth = { username = "metrics", password = "..." } }
```

`enabled` generates a token at `~/.srm/network-token`. Copy it to the other nodes. Every request
needs it, as `X-Srm-Token` or as a bearer token, which is what Prometheus can send. `/metrics` gives
Prometheus per-pool gauges.

mDNS finds peers on the same link. It stops at the subnet and the tailnet, so `[[peer]]` reaches other
sites. State is pulled, and polling slows down sixfold when nobody is watching.

Remote leases, placing work on another machine, and SNMP or UniFi probes aren't built, though nothing
in the design prevents them. The network face stays read-only until there's a reason to change it.

## Developing

```
cargo test --workspace         # Rust, including real daemons over loopback
cargo build                    # `node --test clients/` needs the debug binaries
node --test clients/
cd app && bun install && bun run check
```

Tests use private daemons, through `SRM_HOME` and `SRM_ENDPOINT`, so they never touch one in real use.
The ignored mDNS test sends real multicast: `cargo test -p srmd -- --ignored mdns`.

`bun run dev` in `app/` shows the interface in a browser with made-up data. A debug Tauri build loads
that dev server, so the real app needs a release build:

```
powershell -File scripts/install.ps1
```

That builds everything and installs to `~/.srm/bin`, plus `cpu-slots.mjs` and the `compute-budget`
skill. The binaries can be replaced while they're running. Run `srm-tray.exe` to start the tray app;
no window opens, and Windows usually hides a new icon behind the `^` beside the clock. Drag it onto
the taskbar to keep it visible. Its right-click menu has "Start at login".

Windows first, with no WSL. Linux is supported for `srmd` and `srmctl`, because the servers run Ubuntu.

## License

MIT
