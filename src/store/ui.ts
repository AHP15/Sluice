import { create } from "zustand";
import { loadUiState, saveUiState } from "../lib/ipc";

/**
 * Local UI state (D-023): Zustand owns what is purely client-side. Anything
 * that comes from the Rust core belongs in TanStack Query instead, so there is
 * exactly one cache and no hand-written sync between the two.
 */
interface UiState {
  theme: "light" | "dark";
  editorPaneOpen: boolean;
  draftQuery: string;
  toggleTheme: () => void;
  setEditorPaneOpen: (open: boolean) => void;
  setDraftQuery: (q: string) => void;
}

export const useUiStore = create<UiState>((set) => ({
  theme: "dark",
  editorPaneOpen: true,
  draftQuery: "select 1;",
  toggleTheme: () => set((s) => ({ theme: s.theme === "dark" ? "light" : "dark" })),
  setEditorPaneOpen: (editorPaneOpen) => set({ editorPaneOpen }),
  setDraftQuery: (draftQuery) => set({ draftQuery }),
}));

/**
 * T-033 / D-014: what survives a restart, via the core's disposable cache
 * store. Deliberately NOT `draftQuery`: editor text can hold credentials
 * (R-030), and the cache dir sits outside the redaction path that history and
 * logs go through.
 */
interface PersistedUi {
  theme?: "light" | "dark";
  editorPaneOpen?: boolean;
}

function persisted(s: UiState): PersistedUi {
  return { theme: s.theme, editorPaneOpen: s.editorPaneOpen };
}

/** Hydrate from the cache, then write back (debounced) on change. */
export async function initUiPersistence(): Promise<void> {
  try {
    const saved = (await loadUiState()) as PersistedUi | null;
    if (saved && typeof saved === "object") {
      useUiStore.setState({
        ...(saved.theme === "light" || saved.theme === "dark" ? { theme: saved.theme } : {}),
        ...(typeof saved.editorPaneOpen === "boolean" ? { editorPaneOpen: saved.editorPaneOpen } : {}),
      });
    }
  } catch {
    // Disposable by contract: an unreadable cache is just defaults.
  }
  let last = JSON.stringify(persisted(useUiStore.getState()));
  let timer: ReturnType<typeof setTimeout> | undefined;
  useUiStore.subscribe((s) => {
    const next = JSON.stringify(persisted(s));
    if (next === last) return;
    last = next;
    clearTimeout(timer);
    timer = setTimeout(() => void saveUiState(JSON.parse(next)).catch(() => {}), 500);
  });
}
