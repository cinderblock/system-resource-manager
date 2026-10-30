<script lang="ts">
  import { onMount } from "svelte";
  import type { Sample } from "../lib/CpuChart.svelte";
  import FleetView from "../lib/FleetView.svelte";
  import HistoryView from "../lib/HistoryView.svelte";
  import MachineView from "../lib/MachineView.svelte";
  import { source, type Source } from "../lib/source";
  import "../lib/theme.css";
  import type { Fleet, HistoryRow } from "../lib/types";

  /** Five minutes at the sampler's active rate. */
  const MAX_SAMPLES = 150;
  /** Lease changes also publish, and those aren't new CPU readings. */
  const MIN_SAMPLE_GAP_MS = 1000;

  type Tab = "machine" | "fleet" | "history";

  let backend = $state<Source | null>(null);
  let fleet = $state<Fleet | null>(null);
  let lost = $state(false);
  let tab = $state<Tab>("machine");
  let history = $state<HistoryRow[] | null>(null);
  let samples = $state<Sample[]>([]);

  function receive(next: Fleet) {
    fleet = next;
    lost = false;
    const { system, pools, taken_at } = next.local;
    if (taken_at - (samples.at(-1)?.at ?? 0) < MIN_SAMPLE_GAP_MS) return;
    const leased = pools
      .flatMap((p) => p.holders)
      .reduce((sum, lease) => sum + (lease.usage?.cores ?? 0), 0);
    const total = (system.cpu_percent / 100) * system.cores_logical;
    samples = [...samples, { at: taken_at, total, leased }].slice(-MAX_SAMPLES);
  }

  async function show(next: Tab) {
    tab = next;
    if (next === "history") history = (await backend?.history()) ?? [];
  }

  onMount(async () => {
    backend = await source();
    await backend.subscribe(receive, () => (lost = true));
  });

  const tabs: { id: Tab; label: string }[] = [
    { id: "machine", label: "This machine" },
    { id: "fleet", label: "Fleet" },
    { id: "history", label: "History" },
  ];

  const problems = $derived(
    fleet
      ? fleet.assets.filter((a) => a.health === "down" || a.health === "degraded").length +
          fleet.peers.filter((p) => p.state !== "online").length
      : 0,
  );
</script>

<main>
  <nav>
    {#each tabs as { id, label } (id)}
      <button class:active={tab === id} onclick={() => show(id)}>
        {label}
        {#if id === "fleet" && problems}<span class="count">▲ {problems}</span>{/if}
      </button>
    {/each}
  </nav>

  {#if lost}
    <div class="banner small">▲ Lost contact with srmd. Reconnecting…</div>
  {/if}

  <div class="body">
    {#if !fleet}
      <div class="empty small">Connecting to srmd…</div>
    {:else if tab === "machine"}
      <MachineView snapshot={fleet.local} {samples} />
    {:else if tab === "fleet"}
      <FleetView {fleet} />
    {:else}
      <HistoryView rows={history} />
    {/if}
  </div>
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  nav {
    display: flex;
    gap: 4px;
    padding: 8px 12px 0;
    border-bottom: 1px solid var(--grid);
  }
  button {
    padding: 6px 10px;
    border: 0;
    border-bottom: 2px solid transparent;
    background: none;
    color: var(--text-secondary);
    font: inherit;
    cursor: pointer;
  }
  button.active {
    border-bottom-color: var(--series-1);
    color: var(--text-primary);
    font-weight: 600;
  }
  .count {
    margin-left: 4px;
    font-size: 11px;
  }
  .banner {
    padding: 6px 12px;
    background: var(--raised);
  }
  .body {
    flex: 1;
    overflow-y: auto;
    padding: 12px;
  }
</style>
