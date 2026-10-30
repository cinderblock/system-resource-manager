<script lang="ts">
  import { cores, duration } from "./format";
  import type { HistoryRow } from "./types";

  let { rows }: { rows: HistoryRow[] | null } = $props();

  /** Worth saying out loud, once there are enough runs to trust the average. */
  function verdict(row: HistoryRow): string | null {
    if (row.runs < 3 || row.pool !== "cpu") return null;
    if (row.avg_cores > row.avg_amount * 1.25) return "Spends more than it claims";
    if (row.avg_cores < row.avg_amount * 0.5) return "Claims more than it spends";
    return null;
  }
</script>

{#if rows === null}
  <div class="empty small">Loading…</div>
{:else if rows.length === 0}
  <div class="empty small">Nothing has finished yet.</div>
{:else}
  <p class="small muted">Measured over every finished run. Claimed slots should match cores used.</p>
  <table>
    <thead>
      <tr>
        <th>Label</th>
        <th class="n">Claimed</th>
        <th class="n">Used</th>
        <th class="n">Peak</th>
        <th class="n">Runs</th>
        <th class="n">Takes</th>
        <th class="n">Waits</th>
      </tr>
    </thead>
    <tbody>
      {#each rows as row (row.pool + row.label)}
        <tr>
          <td class="label">
            {row.label}
            {#if verdict(row)}<div class="small note">▲ {verdict(row)}</div>{/if}
          </td>
          <td class="n">{cores(row.avg_amount)}</td>
          <td class="n">{cores(row.avg_cores)}</td>
          <td class="n">{cores(row.peak_cores)}</td>
          <td class="n">{row.runs}</td>
          <td class="n">{duration(row.avg_held_ms)}</td>
          <td class="n">{duration(row.avg_wait_ms)}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<style>
  p {
    margin: 0 0 8px;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-variant-numeric: tabular-nums;
  }
  th {
    font-size: 11px;
    font-weight: 600;
    text-align: left;
    color: var(--muted);
  }
  td,
  th {
    padding: 5px 0 5px 8px;
    vertical-align: top;
  }
  td:first-child,
  th:first-child {
    padding-left: 0;
  }
  tr {
    border-top: 1px solid var(--grid);
  }
  .n {
    text-align: right;
    white-space: nowrap;
  }
  .label {
    word-break: break-word;
  }
  .note {
    color: var(--text-secondary);
  }
</style>
