// Mirrors crates/proto/src/model.rs. Times are milliseconds since the epoch.

export interface ProcessSummary {
  pid: number;
  name: string;
  cores: number;
  rss_bytes: number;
}

export interface Usage {
  cores: number;
  peak_cores: number;
  rss_bytes: number;
  peak_rss_bytes: number;
  processes: number;
  cpu_seconds: number;
  top: ProcessSummary[];
}

export interface Lease {
  id: number;
  pool: string;
  amount: number;
  label: string;
  pid: number;
  cwd?: string;
  repo?: string;
  session?: string;
  low: boolean;
  requested_at: number;
  granted_at?: number;
  usage?: Usage;
}

export interface ProcessGroup {
  root_pid: number;
  name: string;
  cores: number;
  rss_bytes: number;
  processes: number;
  cwd?: string;
}

export interface Pool {
  name: string;
  capacity: number;
  used: number;
  holders: Lease[];
  waiters: Lease[];
}

export interface SystemStats {
  cpu_percent: number;
  cores_physical: number;
  cores_logical: number;
  mem_used_bytes: number;
  mem_total_bytes: number;
}

export interface NodeInfo {
  id: string;
  name: string;
  site?: string;
  os: string;
  version: string;
  started_at: number;
}

export type Health = "up" | "degraded" | "down" | "unknown";

export interface Asset {
  id: string;
  name: string;
  kind: string;
  site?: string;
  health: Health;
  observer: string;
  checked_at: number;
  latency_ms?: number;
  metrics?: Record<string, number>;
  detail?: string;
}

export interface NodeSnapshot {
  node: NodeInfo;
  taken_at: number;
  system: SystemStats;
  pools: Pool[];
  unbrokered: ProcessGroup[];
  assets: Asset[];
}

export interface Peer {
  url: string;
  source: "config" | "mdns";
  state: "online" | "unreachable" | "unauthorized";
  last_seen?: number;
  error?: string;
  snapshot?: NodeSnapshot;
}

export interface Fleet {
  local: NodeSnapshot;
  peers: Peer[];
  assets: Asset[];
}

export interface HistoryRow {
  pool: string;
  label: string;
  runs: number;
  avg_amount: number;
  avg_cores: number;
  peak_cores: number;
  peak_rss_bytes: number;
  avg_held_ms: number;
  avg_wait_ms: number;
  last_at: number;
}
