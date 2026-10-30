<script lang="ts">
  import Status from "./Status.svelte";
  import { bytes, clock, cores, duration } from "./format";
  import type { Asset, Fleet, NodeSnapshot, Peer } from "./types";

  let { fleet }: { fleet: Fleet } = $props();

  const now = $derived(fleet.local.taken_at);

  const kinds: Record<string, string> = {
    server: "Server",
    nas: "NAS",
    camera: "Camera",
    router: "Router",
    switch: "Switch",
    access_point: "Access point",
    service: "Service",
    other: "",
  };

  const sites = $derived.by(() => {
    const groups = new Map<string, Asset[]>();
    for (const asset of fleet.assets) {
      const site = asset.site ?? "Unassigned";
      groups.set(site, [...(groups.get(site) ?? []), asset]);
    }
    return [...groups.entries()].sort(([a], [b]) => a.localeCompare(b));
  });

  const gauges = (asset: Asset) =>
    Object.entries(asset.metrics ?? {})
      .filter(([name]) => name.endsWith("_percent"))
      .map(([name, value]) => `${name.replace("_percent", "")} ${value.toFixed(0)}%`)
      .join(" · ");

  const cpuPool = (s: NodeSnapshot) => s.pools.find((p) => p.name === "cpu");
  const running = (s: NodeSnapshot) => s.pools.flatMap((p) => p.holders);
  const name = (peer: Peer) => peer.snapshot?.node.name ?? new URL(peer.url).hostname;
</script>

{#snippet node(snapshot: NodeSnapshot, stale: boolean)}
  <div class="stats small secondary" class:stale>
    <span>CPU {snapshot.system.cpu_percent.toFixed(0)}%</span>
    <span>{bytes(snapshot.system.mem_used_bytes)} of {bytes(snapshot.system.mem_total_bytes)}</span>
    {#if cpuPool(snapshot)}
      <span>{cpuPool(snapshot)?.used} of {cpuPool(snapshot)?.capacity} slots</span>
    {/if}
  </div>
  {#each running(snapshot) as lease (lease.id)}
    <div class="small secondary work">
      {lease.label}
      {#if lease.usage}<span class="muted">using {cores(lease.usage.cores)} of {lease.amount}</span>{/if}
    </div>
  {/each}
{/snippet}

<section>
  <h2>Machines</h2>
  <div class="row">
    <div class="head">
      <span class="name">{fleet.local.node.name}</span>
      <span class="small muted">This machine</span>
    </div>
    {@render node(fleet.local, false)}
  </div>
  {#each fleet.peers as peer (peer.url)}
    <div class="row">
      <div class="head">
        <span class="name">{name(peer)}</span>
        {#if peer.state === "online"}
          <Status health="up" />
        {:else if peer.state === "unauthorized"}
          <Status health="degraded" />
        {:else}
          <Status health="down" />
        {/if}
      </div>
      {#if peer.state === "unauthorized"}
        <div class="small secondary">Refused our token. Check <code>network-token</code> on both machines.</div>
      {:else if peer.state === "unreachable"}
        <div class="small secondary">
          {#if peer.last_seen}
            Last seen {duration(now - peer.last_seen)} ago, at {clock(peer.last_seen).local}
            <span class="muted">({clock(peer.last_seen).utc})</span>
          {:else}
            Never reached
          {/if}
          {#if peer.error}<span class="muted">· {peer.error}</span>{/if}
        </div>
      {/if}
      {#if peer.snapshot}
        {@render node(peer.snapshot, peer.state !== "online")}
      {/if}
    </div>
  {/each}
  {#if fleet.peers.length === 0}
    <div class="empty small">
      No other machines yet. Add a <code>[[peer]]</code> to <code>~/.srm/config.toml</code>.
    </div>
  {/if}
</section>

{#each sites as [site, assets] (site)}
  <section>
    <h2>{site}</h2>
    {#each assets as asset (asset.id)}
      <div class="row">
        <div class="head">
          <span class="name">{asset.name}</span>
          <span class="small muted">{kinds[asset.kind]}</span>
          <span class="grow"></span>
          {#if asset.latency_ms !== undefined}
            <span class="small muted">{asset.latency_ms.toFixed(0)} ms</span>
          {/if}
          <Status health={asset.health} />
        </div>
        {#if asset.detail || gauges(asset)}
          <div class="small secondary">
            {asset.detail ?? ""}{asset.detail && gauges(asset) ? " · " : ""}{gauges(asset)}
          </div>
        {/if}
      </div>
    {/each}
  </section>
{/each}

{#if fleet.assets.length === 0}
  <div class="empty small">
    No devices yet. Add an <code>[[asset]]</code> to <code>~/.srm/config.toml</code> to watch a NAS,
    camera, router or switch.
  </div>
{/if}

<style>
  section {
    margin-bottom: 16px;
  }
  .row {
    padding: 6px 0;
    border-top: 1px solid var(--grid);
  }
  .head {
    display: flex;
    gap: 8px;
    align-items: baseline;
    justify-content: space-between;
  }
  .name {
    font-weight: 600;
  }
  .grow {
    flex: 1;
  }
  .stats {
    display: flex;
    gap: 12px;
  }
  .stale {
    opacity: 0.6;
  }
  .work {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
