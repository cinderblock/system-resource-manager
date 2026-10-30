<script lang="ts">
  import { bytes, clock, cores, duration } from "./format";
  import type { Lease } from "./types";

  let { lease, now }: { lease: Lease; now: number } = $props();

  const held = $derived(lease.granted_at !== undefined);
  const since = $derived(lease.granted_at ?? lease.requested_at);
  const started = $derived(clock(since));
  const usage = $derived(lease.usage);
  // Worse than not claiming at all: the broker hands out slots that are already spent.
  const over = $derived(usage !== undefined && usage.cores > lease.amount * 1.25);
  const fill = $derived(usage ? Math.min(100, (usage.cores / lease.amount) * 100) : 0);
  const unit = $derived(lease.amount === 1 ? "slot" : "slots");
</script>

<details class="lease">
  <summary>
    <div class="line">
      <span class="label">{lease.label}</span>
      <span class="amount secondary">{lease.amount} {unit}</span>
      <span class="age muted">{held ? "" : "waiting "}{duration(now - since)}</span>
    </div>
    <div class="meta small muted">
      {#if lease.repo}<span>{lease.repo}</span>{/if}
      {#if lease.low}<span>low priority</span>{/if}
      {#if lease.session}<span>session {lease.session.slice(0, 8)}</span>{/if}
    </div>
    {#if usage}
      <div class="meter" class:over aria-hidden="true"><span style="width: {fill}%"></span></div>
      <div class="small secondary">
        Using {cores(usage.cores)} of {lease.amount} claimed
        {#if over}<span class="warn">▲ More than it claimed</span>{/if}
      </div>
    {/if}
  </summary>

  <div class="detail small">
    <div class="secondary">
      {held ? "Started" : "Queued"}
      {started.local} <span class="muted">({started.utc})</span> · pid {lease.pid}
    </div>
    {#if usage}
      <div class="secondary">
        {usage.processes} processes, {bytes(usage.rss_bytes)} · peak {cores(usage.peak_cores)} cores,
        {bytes(usage.peak_rss_bytes)} · {Math.round(usage.cpu_seconds)} CPU-seconds so far
      </div>
      {#each usage.top as process (process.pid)}
        <div class="process">
          <span class="value">{cores(process.cores)}</span>
          <span>{process.name}</span>
          <span class="muted">{bytes(process.rss_bytes)}</span>
        </div>
      {/each}
    {/if}
    {#if lease.cwd}<div class="muted path">{lease.cwd}</div>{/if}
  </div>
</details>

<style>
  .lease {
    padding: 6px 0;
    border-top: 1px solid var(--grid);
  }
  summary {
    list-style: none;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  .line {
    display: flex;
    gap: 8px;
    align-items: baseline;
  }
  .label {
    flex: 1;
    min-width: 0;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .amount,
  .age {
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .meta {
    display: flex;
    gap: 8px;
  }
  .meter {
    height: 4px;
    margin: 4px 0 2px;
    border-radius: 2px;
    background: var(--track);
    overflow: hidden;
  }
  .meter span {
    display: block;
    height: 100%;
    border-radius: 2px;
    background: var(--series-1);
    transition: width 0.4s;
  }
  .meter.over span {
    background: var(--serious);
  }
  .warn {
    margin-left: 6px;
    color: var(--text-primary);
  }
  .detail {
    margin-top: 6px;
    padding: 6px 8px;
    border-radius: 4px;
    background: var(--raised);
  }
  .process {
    display: flex;
    gap: 8px;
    margin-top: 2px;
  }
  .process span:nth-child(2) {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .value {
    width: 24px;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .path {
    margin-top: 4px;
    word-break: break-all;
  }
</style>
