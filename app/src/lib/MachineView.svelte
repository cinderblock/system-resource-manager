<script lang="ts">
  import CpuChart, { type Sample } from "./CpuChart.svelte";
  import LeaseRow from "./LeaseRow.svelte";
  import Slots from "./Slots.svelte";
  import { bytes, cores } from "./format";
  import type { NodeSnapshot } from "./types";

  let { snapshot, samples }: { snapshot: NodeSnapshot; samples: Sample[] } = $props();

  const cpu = $derived(snapshot.pools.find((p) => p.name === "cpu"));
</script>

<section>
  <CpuChart {samples} budget={cpu?.capacity ?? 0} max={snapshot.system.cores_logical || 1} />
  <div class="small secondary stats">
    <span>{snapshot.system.cpu_percent.toFixed(0)}% of {snapshot.system.cores_logical} logical cores</span>
    <span>{bytes(snapshot.system.mem_used_bytes)} of {bytes(snapshot.system.mem_total_bytes)} memory</span>
  </div>
</section>

{#each snapshot.pools as pool (pool.name)}
  <section>
    <div class="pool-head">
      <h2>{pool.name}</h2>
      <span class="small secondary">{pool.used} of {pool.capacity} in use</span>
    </div>
    <Slots {pool} />
    {#if pool.holders.length === 0}
      <div class="empty small">Nothing running.</div>
    {/if}
    <div class="list">
      {#each pool.holders as lease (lease.id)}
        <LeaseRow {lease} now={snapshot.taken_at} />
      {/each}
    </div>
    {#if pool.waiters.length}
      <h2 class="queue">Waiting ({pool.waiters.length})</h2>
      {#each pool.waiters as lease (lease.id)}
        <LeaseRow {lease} now={snapshot.taken_at} />
      {/each}
    {/if}
  </section>
{/each}

{#if snapshot.unbrokered.length}
  <section>
    <h2>Outside the budget</h2>
    {#each snapshot.unbrokered as group (group.root_pid)}
      <div class="outside">
        <div class="line">
          <span class="warn">▲</span>
          <span class="name">{group.name}</span>
          <span class="secondary">{cores(group.cores)} cores</span>
        </div>
        <div class="small muted">
          pid {group.root_pid}, {group.processes} processes, {bytes(group.rss_bytes)}
          {#if group.cwd}<br />{group.cwd}{/if}
        </div>
      </div>
    {/each}
  </section>
{/if}

<style>
  section {
    margin-bottom: 16px;
  }
  .stats {
    display: flex;
    justify-content: space-between;
    margin-top: 2px;
  }
  .pool-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
  }
  .list {
    margin-top: 6px;
  }
  .queue {
    margin-top: 10px;
  }
  .outside {
    padding: 6px 0;
    border-top: 1px solid var(--grid);
  }
  .line {
    display: flex;
    gap: 6px;
    align-items: baseline;
  }
  .name {
    flex: 1;
    font-weight: 600;
  }
  .warn {
    color: var(--warning);
    font-size: 11px;
  }
</style>
