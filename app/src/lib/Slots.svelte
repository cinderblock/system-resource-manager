<script lang="ts">
  import type { Pool } from "./types";

  let { pool }: { pool: Pool } = $props();

  // One block per slot, so "4 of 4" reads at a glance. A pool over capacity (work that
  // rejoined after a daemon restart) shows the overflow rather than hiding it.
  const blocks = $derived(Math.max(pool.capacity, pool.used));
</script>

<div class="slots" role="img" aria-label="{pool.used} of {pool.capacity} {pool.name} in use">
  {#each { length: blocks } as _, i}
    <span class:filled={i < pool.used} class:over={i >= pool.capacity}></span>
  {/each}
</div>

<style>
  .slots {
    display: flex;
    gap: 2px;
    height: 8px;
  }
  span {
    flex: 1;
    border-radius: 4px;
    background: var(--track);
  }
  .filled {
    background: var(--series-1);
  }
  .over {
    background: var(--serious);
  }
</style>
