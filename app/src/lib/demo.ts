// Believable data for working on the interface in a browser. Dev builds only.

import type { Source } from "./source";
import type { Fleet, HistoryRow, Lease } from "./types";

const GIB = 1024 ** 3;
const now = () => Date.now();

function lease(partial: Partial<Lease> & Pick<Lease, "id" | "amount" | "label">): Lease {
  return {
    pool: "cpu",
    pid: 40000 + partial.id,
    low: false,
    requested_at: now() - 200_000,
    ...partial,
  };
}

function fleet(wobble: number): Fleet {
  const buildCores = 3.4 + wobble;
  return {
    local: {
      node: {
        id: "local",
        name: "workstation",
        site: "Office",
        os: "Windows 11 Pro N",
        version: "0.1.0",
        started_at: now() - 86_400_000,
      },
      taken_at: now(),
      system: {
        cpu_percent: 44 + wobble * 8,
        cores_physical: 6,
        cores_logical: 12,
        mem_used_bytes: 41 * GIB,
        mem_total_bytes: 64 * GIB,
      },
      pools: [
        {
          name: "cpu",
          capacity: 4,
          used: 4,
          holders: [
            lease({
              id: 1,
              amount: 3,
              label: "t3code build:desktop",
              repo: "t3code",
              session: "a1b2c3d4-0000",
              low: true,
              granted_at: now() - 134_000,
              usage: {
                cores: buildCores,
                peak_cores: 5.2,
                rss_bytes: 3.2 * GIB,
                peak_rss_bytes: 4 * GIB,
                processes: 14,
                cpu_seconds: 402,
                top: [
                  { pid: 1, name: "rustc tauri", cores: 1.9, rss_bytes: 1.1 * GIB },
                  { pid: 2, name: "rustc srm_proto", cores: 0.8, rss_bytes: 0.4 * GIB },
                  { pid: 3, name: "link", cores: 0.5, rss_bytes: 0.9 * GIB },
                ],
              },
            }),
            lease({
              id: 2,
              amount: 1,
              label: "svdsa typecheck",
              repo: "svdsa.org",
              granted_at: now() - 21_000,
              usage: {
                cores: 0.4,
                peak_cores: 1,
                rss_bytes: 0.6 * GIB,
                peak_rss_bytes: 0.7 * GIB,
                processes: 2,
                cpu_seconds: 9,
                top: [{ pid: 4, name: "node tsc.js", cores: 0.4, rss_bytes: 0.6 * GIB }],
              },
            }),
          ],
          waiters: [
            lease({
              id: 3,
              amount: 3,
              label: "svdsa playwright",
              repo: "svdsa.org",
              requested_at: now() - 47_000,
            }),
          ],
        },
        {
          name: "camera-warehouse",
          capacity: 1,
          used: 1,
          holders: [
            lease({
              id: 4,
              pool: "camera-warehouse",
              amount: 1,
              label: "yard timelapse",
              granted_at: now() - 900_000,
            }),
          ],
          waiters: [],
        },
      ],
      unbrokered: [
        {
          root_pid: 9920,
          name: "cargo",
          cores: 2.1,
          rss_bytes: 1.4 * GIB,
          processes: 6,
          cwd: "C:\\Users\\me\\git\\open-task",
        },
      ],
      assets: [],
    },
    peers: [
      {
        url: "http://build-box.tailnet.example:7654",
        source: "config",
        state: "online",
        last_seen: now(),
        snapshot: {
          node: {
            id: "build-box",
            name: "build-box",
            site: "Workshop",
            os: "Ubuntu 24.04",
            version: "0.1.0",
            started_at: now() - 900_000_000,
          },
          taken_at: now(),
          system: {
            cpu_percent: 62,
            cores_physical: 12,
            cores_logical: 24,
            mem_used_bytes: 71 * GIB,
            mem_total_bytes: 128 * GIB,
          },
          pools: [
            {
              name: "cpu",
              capacity: 10,
              used: 8,
              holders: [lease({ id: 5, amount: 8, label: "ghidra analysis", granted_at: now() - 3_600_000 })],
              waiters: [],
            },
          ],
          unbrokered: [],
          assets: [],
        },
      },
      {
        url: "http://laptop.tailnet.example:7654",
        source: "config",
        state: "unreachable",
        last_seen: now() - 5_400_000,
        error: "connection refused",
      },
    ],
    assets: [
      { id: "storage", name: "storage", kind: "nas", site: "Workshop", health: "degraded", observer: "workstation", checked_at: now(), latency_ms: 3, metrics: { disk_percent: 93, cpu_percent: 12 }, detail: "disk 93% full" },
      { id: "office-server", name: "office-server", kind: "server", site: "Workshop", health: "up", observer: "workstation", checked_at: now(), latency_ms: 2, metrics: { cpu_percent: 8, memory_percent: 41 } },
      { id: "cam-yard", name: "Yard camera", kind: "camera", site: "Workshop", health: "up", observer: "workstation", checked_at: now(), latency_ms: 6 },
      { id: "cam-door", name: "Back door camera", kind: "camera", site: "Workshop", health: "down", observer: "workstation", checked_at: now(), detail: "timed out" },
      { id: "usw-office", name: "Office switch", kind: "switch", site: "Office", health: "up", observer: "workstation", checked_at: now(), latency_ms: 1 },
      { id: "usg", name: "Gateway", kind: "router", site: "Office", health: "up", observer: "workstation", checked_at: now(), latency_ms: 1 },
    ],
  };
}

const rows: HistoryRow[] = [
  { pool: "cpu", label: "t3code build:desktop", runs: 38, avg_amount: 2, avg_cores: 3.6, peak_cores: 7.1, peak_rss_bytes: 5 * GIB, avg_held_ms: 212_000, avg_wait_ms: 41_000, last_at: now() - 60_000 },
  { pool: "cpu", label: "svdsa playwright", runs: 112, avg_amount: 3, avg_cores: 2.7, peak_cores: 4.4, peak_rss_bytes: 3 * GIB, avg_held_ms: 418_000, avg_wait_ms: 96_000, last_at: now() - 3_600_000 },
  { pool: "cpu", label: "cargo test", runs: 64, avg_amount: 4, avg_cores: 1.3, peak_cores: 4, peak_rss_bytes: 2 * GIB, avg_held_ms: 75_000, avg_wait_ms: 12_000, last_at: now() - 7_200_000 },
  { pool: "cpu", label: "svdsa typecheck", runs: 201, avg_amount: 1, avg_cores: 0.9, peak_cores: 1.1, peak_rss_bytes: 0.8 * GIB, avg_held_ms: 24_000, avg_wait_ms: 3_000, last_at: now() - 9_000_000 },
];

export const demo: Source = {
  async subscribe(onFleet) {
    let t = 0;
    const tick = () => onFleet(fleet(Math.sin((t += 0.4)) * 0.9));
    tick();
    setInterval(tick, 2000);
  },
  history: async () => rows,
};
