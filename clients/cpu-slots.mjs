/**
 * A machine-wide budget for heavy local commands.
 *
 * This is a working computer. Several Claude sessions, several projects, and a
 * person all share six physical cores, and nothing else stops a test suite in one
 * repo from launching a dozen browsers while a build runs in another.
 *
 * The reason to enforce this rather than be careful about it: overloading the
 * machine doesn't only make it unpleasant to use, it makes RESULTS WRONG. Under
 * contention an ordinary wait becomes a timeout, and a timeout reads exactly like a
 * regression. Measured 2026-08-11 in svdsa.org — 14 specs failed with two concurrent
 * Playwright runs and every one passed when run alone.
 *
 * The broker itself is `srmd`, from the system-resource-manager project. This file is
 * how JavaScript reaches it, and it stays at this path because repos import it from
 * here. A lease is an open connection to the daemon: when this process ends, however
 * it ends, the slots come back at that moment.
 *
 * ── Use from a script ───────────────────────────────────────────────────────
 *
 *   import { homedir } from "node:os";
 *   import { join } from "node:path";
 *   import { pathToFileURL } from "node:url";
 *   const { acquire } = await import(
 *     pathToFileURL(join(homedir(), ".claude/bin/cpu-slots.mjs")).href
 *   );
 *   const release = await acquire(3, "svdsa playwright");
 *   try { ... } finally { release(); }
 *
 * ── Use from a shell ────────────────────────────────────────────────────────
 *
 *   node ~/.claude/bin/cpu-slots.mjs run --slots 3 --label "svdsa tests" -- bun run test
 *   node ~/.claude/bin/cpu-slots.mjs run --slots 2 --low -- pnpm build
 *   node ~/.claude/bin/cpu-slots.mjs status
 *
 * Both are the same as `~/.srm/bin/srmctl`, which this hands over to.
 *
 * Callers that can't reach the broker carry on rather than break: this is a courtesy
 * layer, and a missing daemon must never make a repo untestable.
 */

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { connect } from "node:net";
import { availableParallelism, homedir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const WINDOWS = process.platform === "win32";
const HOME = process.env.SRM_HOME ?? join(homedir(), ".srm");
const ENDPOINT =
  process.env.SRM_ENDPOINT ??
  (WINDOWS ? "\\\\.\\pipe\\srmd" : join(HOME, "srmd.sock"));
const CTL = join(HOME, "bin", WINDOWS ? "srmctl.exe" : "srmctl");
const CONFIG = join(HOME, "config.toml");

/** How often to look for a daemon again after losing one mid-lease. */
const RECONNECT_MS = 2000;
/** Longer than RECONNECT_MS, so running work gets back in first. */
const REJOIN_DELAY_MS = 3000;

/** What the daemon last said, which beats reading the config. */
let knownCapacity;

/**
 * The size of the `cpu` pool. Synchronous, because callers use it in log lines, so
 * it can't ask the daemon. The fallback mirrors the daemon's own default.
 */
export function budget() {
  if (knownCapacity) return knownCapacity;
  try {
    const text = readFileSync(CONFIG, "utf8");
    const match = /\[resources\.cpu\]\s+capacity\s*=\s*(\d+)/.exec(text);
    if (match) return Number(match[1]);
  } catch {
    // No config, which is a fine answer: the default applies.
  }
  return Math.max(2, Math.floor(availableParallelism() / 2) - 2);
}

function open() {
  return new Promise((resolve, reject) => {
    const socket = connect(ENDPOINT);
    socket.once("connect", () => {
      socket.removeListener("error", reject);
      resolve(socket);
    });
    socket.once("error", reject);
  });
}

async function openOrStart() {
  try {
    return await open();
  } catch {
    spawnSync(CTL, ["daemon", "start"], { stdio: "ignore", windowsHide: true });
    return await open();
  }
}

function send(socket, message) {
  socket.write(`${JSON.stringify(message)}\n`);
}

/** Calls `onMessage` for each line, until it returns true. */
function listen(socket, onMessage) {
  let buffered = "";
  const onData = (chunk) => {
    buffered += chunk;
    for (;;) {
      const end = buffered.indexOf("\n");
      if (end === -1) return;
      const line = buffered.slice(0, end);
      buffered = buffered.slice(end + 1);
      if (onMessage(JSON.parse(line))) {
        socket.removeListener("data", onData);
        return;
      }
    }
  };
  socket.setEncoding("utf8");
  socket.on("data", onData);
}

/** The shape holders have always had here, whatever the daemon calls things. */
function holderView(lease) {
  return {
    pid: lease.pid,
    label: lease.label,
    slots: lease.amount,
    started: new Date(lease.granted_at ?? lease.requested_at).toISOString(),
  };
}

/**
 * Resolves with the grant, or `null` if the connection dropped first. Losing the daemon
 * while queued isn't the same as having no broker: treating it that way would run every
 * queued command the moment the daemon restarts, the exact pile-up this exists to stop.
 */
function waitInLine(socket, request, onWait) {
  return new Promise((resolve, reject) => {
    socket.once("error", () => resolve(null));
    socket.once("close", () => resolve(null));
    listen(socket, (message) => {
      if (message.type === "queued") {
        knownCapacity = message.capacity;
        onWait?.(message.amount, message.free, message.holders.map(holderView));
      } else if (message.type === "granted") {
        knownCapacity = message.capacity;
        resolve({ grantedAt: message.granted_at, waitedMs: message.waited_ms });
        return true;
      } else if (message.type === "error") {
        reject(new Error(message.message));
        return true;
      }
      return false;
    });
    send(socket, { type: "acquire", ...request });
  });
}

/**
 * Claim `want` slots, waiting until they're free. Returns a release function, with
 * `.waitedMs` on it.
 *
 * A request larger than the whole budget is clamped rather than deadlocking on slots
 * that can never exist.
 */
export async function acquire(want, label, { onWait } = {}) {
  const request = {
    amount: want,
    label: label ?? "unlabelled",
    pid: process.pid,
    cwd: process.cwd(),
    session: process.env.CLAUDE_CODE_SESSION_ID,
  };
  const requestedAt = Date.now();

  let socket;
  let grantedAt;
  let waitedMs;
  for (let rejoining = false; waitedMs === undefined; rejoining = true) {
    try {
      socket = await openOrStart();
    } catch {
      console.warn(`cpu-slots: no broker at ${ENDPOINT} — running unbudgeted.`);
      return Object.assign(() => {}, { waitedMs: 0 });
    }
    // Work that was already running is on its way back. Let it be counted first, or
    // this would see an empty pool and start on top of it.
    if (rejoining) await new Promise((resolve) => setTimeout(resolve, REJOIN_DELAY_MS));

    try {
      const granted = await waitInLine(socket, request, onWait);
      if (granted) ({ grantedAt, waitedMs } = granted);
    } catch (e) {
      console.warn(`cpu-slots: ${e.message} — running unbudgeted.`);
      socket.destroy();
      return Object.assign(() => {}, { waitedMs: 0 });
    }
  }

  let released = false;

  /**
   * The connection is the lease, so losing the daemon loses the count. The work is
   * still running, so get back in it as soon as there's a daemon to tell.
   */
  const keep = (held) => {
    held.removeAllListeners("error");
    held.removeAllListeners("close");
    held.on("error", () => {});
    held.once("close", () => {
      if (!released) setTimeout(rejoin, RECONNECT_MS).unref();
    });
    held.unref(); // A held lease must not be what keeps this process alive.
    socket = held;
  };

  const rejoin = async () => {
    if (released) return;
    try {
      // Only ever connect. Starting one here would undo `srmctl daemon stop`.
      const fresh = await open();
      if (released) return fresh.destroy();
      send(fresh, {
        type: "adopt",
        ...request,
        requested_at: requestedAt,
        granted_at: grantedAt,
      });
      keep(fresh);
    } catch {
      setTimeout(rejoin, RECONNECT_MS).unref();
    }
  };

  keep(socket);

  const release = () => {
    released = true;
    socket.destroy();
  };
  release.waitedMs = waitedMs;
  return release;
}

/** Human-readable wait notice, shared by the CLI and importing callers. */
export function describeWait(need, free, holders) {
  const lines = [`cpu-slots: waiting for ${need} slots (${free} of ${budget()} free)`];
  for (const c of holders) {
    const age = Math.round((Date.now() - Date.parse(c.started)) / 1000);
    lines.push(`  held by pid ${c.pid}  "${c.label}"  (${c.slots} slots, ${age}s)`);
  }
  return lines.join("\n");
}

/** Live holders of the `cpu` pool. */
export function claims() {
  const { stdout, status } = spawnSync(CTL, ["status", "--json"], {
    encoding: "utf8",
    windowsHide: true,
  });
  if (status !== 0) return [];
  const pool = JSON.parse(stdout).local.pools.find((p) => p.name === "cpu");
  return (pool?.holders ?? []).map(holderView);
}

/** Without `srmctl` there's no broker to wait for, so just do the work. */
function runUnbudgeted(argv) {
  const sep = argv.indexOf("--");
  const rest = argv.slice(sep + 1);
  if (sep === -1 || !rest.length) {
    console.error('cpu-slots: need "--" and a command to run');
    return 2;
  }
  console.error(`cpu-slots: ${CTL} is missing — running unbudgeted`);
  // Unshelled first: `shell: true` on Windows joins argv without escaping. `.cmd`
  // launchers can't start without a shell, and those are bare names like `npx`.
  const options = { stdio: "inherit", windowsHide: true };
  let result = spawnSync(rest[0], rest.slice(1), options);
  if (result.error && WINDOWS) {
    result = spawnSync(rest[0], rest.slice(1), { ...options, shell: true });
  }
  return result.status ?? 127;
}

function main(argv) {
  const result = spawnSync(CTL, argv, { stdio: "inherit", windowsHide: true });
  if (!result.error) return result.status ?? 1;
  if (argv[0] === "run") return runUnbudgeted(argv);
  console.error(`cpu-slots: ${CTL} is missing`);
  return 2;
}

// Only when run directly, so importing this file has no side effects. Compared as
// file URLs because a bare path comparison gets Windows separators and drive letter
// casing wrong.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  process.exit(main(process.argv.slice(2)));
