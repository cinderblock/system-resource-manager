import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Fleet, HistoryRow } from "./types";

export interface Source {
  subscribe(onFleet: (fleet: Fleet) => void, onLost: () => void): Promise<void>;
  history(): Promise<HistoryRow[]>;
}

const tauri: Source = {
  async subscribe(onFleet, onLost) {
    await listen<Fleet>("fleet", (event) => onFleet(event.payload));
    await listen("disconnected", () => onLost());
    const latest = await invoke<Fleet | null>("current");
    if (latest) onFleet(latest);
  },
  history: () => invoke<HistoryRow[]>("history"),
};

/** In a plain browser there's no daemon, which is only ever the design preview. */
export async function source(): Promise<Source> {
  if (isTauri() || !import.meta.env.DEV) return tauri;
  return (await import("./demo")).demo;
}
