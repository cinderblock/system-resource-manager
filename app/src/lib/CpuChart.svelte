<script lang="ts" module>
  export interface Sample {
    at: number;
    /** Everything the machine is doing, in cores. */
    total: number;
    /** The part of that inside leases. */
    leased: number;
  }
</script>

<script lang="ts">
  import { clock, cores } from "./format";

  let {
    samples,
    budget,
    max,
  }: { samples: Sample[]; budget: number; max: number } = $props();

  const HEIGHT = 96;
  const TOP = 6;
  const BOTTOM = 4;
  const SPAN_MS = 5 * 60 * 1000;

  let width = $state(380);
  let hovering = $state<number | null>(null);

  const newest = $derived(samples.at(-1)?.at ?? 0);
  const x = (at: number) => width - ((newest - at) / SPAN_MS) * width;
  const y = (value: number) => TOP + (1 - Math.min(value, max) / max) * (HEIGHT - TOP - BOTTOM);
  const path = (pick: (s: Sample) => number) =>
    samples.map((s, i) => `${i ? "L" : "M"}${x(s.at).toFixed(1)} ${y(pick(s)).toFixed(1)}`).join("");

  const hovered = $derived(hovering === null ? null : (samples[hovering] ?? null));
  const latest = $derived(hovered ?? samples.at(-1));
  const time = $derived(hovered ? clock(hovered.at) : null);

  function pointer(event: PointerEvent) {
    const left = (event.currentTarget as SVGElement).getBoundingClientRect().left;
    const at = newest - ((width - (event.clientX - left)) / width) * SPAN_MS;
    let best = 0;
    samples.forEach((s, i) => {
      if (Math.abs(s.at - at) < Math.abs(samples[best].at - at)) best = i;
    });
    hovering = samples.length ? best : null;
  }
</script>

<figure aria-label="Cores in use over the last five minutes">
  <figcaption class="small">
    <span class="key"><i style="background: var(--series-1)"></i>Machine {latest ? cores(latest.total) : "–"}</span>
    <span class="key"><i style="background: var(--series-2)"></i>In leases {latest ? cores(latest.leased) : "–"}</span>
    <span class="muted when">
      {#if time}{time.local} <span>({time.utc})</span>{:else}Cores, last 5 minutes{/if}
    </span>
  </figcaption>

  <div class="plot" bind:clientWidth={width}>
    <svg
      {width}
      height={HEIGHT}
      role="img"
      aria-label="Line chart. The table below has the values."
      onpointermove={pointer}
      onpointerdown={pointer}
      onpointerleave={() => (hovering = null)}
    >
      <line class="budget" x1="0" x2={width} y1={y(budget)} y2={y(budget)} />
      <text class="budget-label" x="2" y={y(budget) - 3}>Budget {budget}</text>
      <path d={path((s) => s.total)} style="stroke: var(--series-1)" />
      <path d={path((s) => s.leased)} style="stroke: var(--series-2)" />
      {#if hovered}
        <line class="cross" x1={x(hovered.at)} x2={x(hovered.at)} y1="0" y2={HEIGHT} />
        <circle cx={x(hovered.at)} cy={y(hovered.total)} r="4" style="fill: var(--series-1)" />
        <circle cx={x(hovered.at)} cy={y(hovered.leased)} r="4" style="fill: var(--series-2)" />
      {/if}
    </svg>
  </div>

  <details class="small">
    <summary class="muted">Show as a table</summary>
    <table>
      <thead><tr><th>Local time</th><th>Machine</th><th>In leases</th></tr></thead>
      <tbody>
        {#each samples.slice(-8).reverse() as sample (sample.at)}
          <tr>
            <td>{clock(sample.at).local}</td>
            <td>{cores(sample.total)}</td>
            <td>{cores(sample.leased)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </details>
</figure>

<style>
  figure {
    margin: 0;
  }
  figcaption {
    display: flex;
    gap: 12px;
    margin-bottom: 4px;
    font-variant-numeric: tabular-nums;
  }
  .key {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    color: var(--text-secondary);
  }
  .key i {
    width: 12px;
    height: 3px;
    border-radius: 2px;
  }
  .when {
    margin-left: auto;
  }
  .plot {
    touch-action: none;
  }
  svg {
    display: block;
  }
  path {
    fill: none;
    stroke-width: 2;
    stroke-linejoin: round;
    stroke-linecap: round;
  }
  circle {
    stroke: var(--surface);
    stroke-width: 2;
  }
  .budget {
    stroke: var(--muted);
    stroke-dasharray: 3 3;
  }
  .budget-label {
    fill: var(--muted);
    font-size: 10px;
  }
  .cross {
    stroke: var(--muted);
  }
  table {
    width: 100%;
    margin-top: 4px;
    border-collapse: collapse;
    font-variant-numeric: tabular-nums;
  }
  th {
    font-weight: 600;
    text-align: left;
    color: var(--muted);
  }
</style>
