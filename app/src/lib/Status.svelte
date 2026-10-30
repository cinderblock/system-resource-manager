<script lang="ts">
  import type { Health } from "./types";

  let { health }: { health: Health } = $props();

  // Shape and word as well as color: state is never color alone.
  const views: Record<Health, { mark: string; word: string; role: string }> = {
    up: { mark: "●", word: "Up", role: "good" },
    degraded: { mark: "▲", word: "Degraded", role: "warning" },
    down: { mark: "✕", word: "Down", role: "critical" },
    unknown: { mark: "?", word: "Unknown", role: "muted" },
  };
  const view = $derived(views[health]);
</script>

<span class="status">
  <span class="mark" style="color: var(--{view.role})">{view.mark}</span>
  <span class="secondary small">{view.word}</span>
</span>

<style>
  .status {
    display: inline-flex;
    align-items: baseline;
    gap: 4px;
    white-space: nowrap;
  }
  .mark {
    font-size: 11px;
  }
</style>
