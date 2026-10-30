// Run with `node --test clients/`, after `cargo build`. Each test gets a private
// daemon, so this never touches one that's in real use.
//
// The import is what real callers do, and svdsa.org's globalSetup is the contract:
// acquire + onWait + describeWait + a synchronous budget + release.waitedMs.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const windows = process.platform === "win32";
const exe = windows ? ".exe" : "";
const shim = join(here, "cpu-slots.mjs");

const home = mkdtempSync(join(tmpdir(), "srm-shim-"));
const endpoint = windows
  ? `\\\\.\\pipe\\srmd-shim-${process.pid}`
  : join(home, "srmd.sock");
const env = { ...process.env, SRM_HOME: home, SRM_ENDPOINT: endpoint };

// Environment first: the module reads it on import.
process.env.SRM_HOME = home;
process.env.SRM_ENDPOINT = endpoint;
const broker = await import(pathToFileURL(shim).href);

const ctl = (...args) =>
  spawnSync(join(home, "bin", `srmctl${exe}`), args, { env, encoding: "utf8", windowsHide: true });
const status = () => JSON.parse(ctl("status", "--json").stdout).local.pools[0];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(what, test) {
  const deadline = Date.now() + 15_000;
  for (;;) {
    const value = status();
    if (test(value)) return value;
    assert.ok(Date.now() < deadline, `never saw ${what}: ${JSON.stringify(value)}`);
    await sleep(50);
  }
}

before(() => {
  mkdirSync(join(home, "bin"));
  for (const name of [`srmd${exe}`, `srmctl${exe}`])
    copyFileSync(join(here, "..", "target", "debug", name), join(home, "bin", name));
  writeFileSync(join(home, "config.toml"), "[resources.cpu]\ncapacity = 3\n");
});

after(() => {
  ctl("daemon", "stop");
  for (let i = 0; i < 20; i++) {
    try {
      rmSync(home, { recursive: true });
      return;
    } catch {
      spawnSync("node", ["-e", "setTimeout(()=>{},50)"], { windowsHide: true });
    }
  }
});

test("budget is synchronous and reads the config before any daemon exists", () => {
  assert.equal(broker.budget(), 3);
});

test("acquire starts the daemon, claims, and release gives it back", async () => {
  const release = await broker.acquire(2, "svdsa playwright");
  assert.equal(typeof release, "function");
  assert.equal(typeof release.waitedMs, "number");
  const pool = status();
  assert.equal(pool.used, 2);
  assert.equal(pool.holders[0].label, "svdsa playwright");
  assert.equal(pool.holders[0].pid, process.pid);
  release();
  await until("the release", (p) => p.used === 0);
});

test("releasing twice is harmless", async () => {
  const release = await broker.acquire(1, "twice");
  release();
  release();
  await until("the release", (p) => p.used === 0);
});

test("a waiter hears who it's waiting for, in the shape callers expect", async () => {
  const first = await broker.acquire(3, "first");
  const seen = [];
  const waiting = broker.acquire(1, "second", {
    onWait: (need, free, holders) => {
      seen.push([need, free]);
      seen.notice = broker.describeWait(need, free, holders);
      seen.holder = holders[0];
    },
  });
  await until("the queue", (p) => p.waiters.length === 1);
  first();
  const second = await waiting;

  assert.deepEqual(seen[0], [1, 0]);
  assert.match(seen.notice, /waiting for 1 slots \(0 of 3 free\)/);
  assert.match(seen.notice, /held by pid \d+ {2}"first" {2}\(3 slots, \d+s\)/);
  assert.equal(typeof Date.parse(seen.holder.started), "number");
  second();
  await until("the release", (p) => p.used === 0);
});

test("an oversized claim is clamped", async () => {
  const release = await broker.acquire(99, "huge");
  assert.equal(status().used, 3);
  release();
  await until("the release", (p) => p.used === 0);
});

test("claims lists holders", async () => {
  const release = await broker.acquire(1, "listed");
  assert.deepEqual(broker.claims().map((c) => [c.label, c.slots]), [["listed", 1]]);
  release();
  await until("the release", (p) => p.used === 0);
});

test("a held lease doesn't keep the process alive, and ends with it", async () => {
  const code = `
    const { acquire } = await import(${JSON.stringify(pathToFileURL(shim).href)});
    await acquire(1, "exiter");
    console.log("held");
  `;
  const child = spawn(process.execPath, ["--input-type=module", "-e", code], {
    env,
    windowsHide: true,
  });
  const exited = new Promise((resolve) => child.on("exit", resolve));
  const result = await Promise.race([exited, sleep(10_000).then(() => "hung")]);
  assert.equal(result, 0);
  await until("the release", (p) => p.used === 0);
});

test("a lease rejoins after the daemon restarts", async () => {
  const release = await broker.acquire(2, "survivor");
  ctl("daemon", "stop");
  ctl("daemon", "start");
  const pool = await until("the lease adopted", (p) => p.used === 2);
  assert.equal(pool.holders[0].label, "survivor");
  release();
  await until("the release", (p) => p.used === 0);
});

test("the CLI passes through exit codes, as t3code's launcher needs", () => {
  const run = spawnSync(
    process.execPath,
    [shim, "run", "--slots", "1", "--low", "--label", "x", "--", "node", "-e", "process.exit(9)"],
    { env, windowsHide: true },
  );
  assert.equal(run.status, 9);
});

test("the CLI shows status", () => {
  const run = spawnSync(process.execPath, [shim, "status"], {
    env,
    encoding: "utf8",
    windowsHide: true,
  });
  assert.equal(run.status, 0);
  assert.match(run.stdout, /cpu: 0\/3 in use/);
});

test("with nothing installed, commands still run", () => {
  const bare = mkdtempSync(join(tmpdir(), "srm-bare-"));
  const missing = { ...process.env, SRM_HOME: bare };
  const run = spawnSync(
    process.execPath,
    [shim, "run", "--slots", "1", "--", "node", "-e", "process.exit(4)"],
    { env: missing, encoding: "utf8", windowsHide: true },
  );
  assert.equal(run.status, 4);
  assert.match(run.stderr, /running unbudgeted/);
  rmSync(bare, { recursive: true });
});

test("with nothing installed, acquire is a no-op rather than an error", async () => {
  const bare = mkdtempSync(join(tmpdir(), "srm-bare-"));
  const code = `
    const { acquire } = await import(${JSON.stringify(pathToFileURL(shim).href)});
    const release = await acquire(1, "x");
    release();
    console.log(release.waitedMs);
  `;
  const run = spawnSync(process.execPath, ["--input-type=module", "-e", code], {
    env: { ...process.env, SRM_HOME: bare, SRM_ENDPOINT: windows ? "\\\\.\\pipe\\srmd-none" : join(bare, "none") },
    encoding: "utf8",
    windowsHide: true,
  });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout.trim(), "0");
  rmSync(bare, { recursive: true });
});
