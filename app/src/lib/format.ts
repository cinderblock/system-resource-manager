const GIB = 1024 ** 3;

export function bytes(n: number): string {
  const gib = n / GIB;
  return gib >= 1 ? `${gib.toFixed(1)} GiB` : `${(gib * 1024).toFixed(0)} MiB`;
}

export function duration(ms: number): string {
  const secs = Math.max(0, Math.floor(ms / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${String(secs % 60).padStart(2, "0")}s`;
  return `${Math.floor(secs / 3600)}h ${String(Math.floor(secs / 60) % 60).padStart(2, "0")}m`;
}

export function cores(n: number): string {
  return n.toFixed(1);
}

const local = new Intl.DateTimeFormat(undefined, {
  hour: "numeric",
  minute: "2-digit",
  hour12: true,
  timeZoneName: "short",
});

const utc = new Intl.DateTimeFormat("en-GB", {
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
  timeZone: "UTC",
});

/** Local first and labelled; UTC is the secondary reading. */
export function clock(at: number): { local: string; utc: string } {
  return { local: local.format(at), utc: `${utc.format(at)}Z` };
}
