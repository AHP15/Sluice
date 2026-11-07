import { create } from "zustand";
import { loadUiState, saveUiState } from "../lib/ipc";

/**
 * Local UI state (D-023): Zustand owns what is purely client-side. Anything
 * that comes from the Rust core belongs in TanStack Query instead, so there is
 * exactly one cache and no hand-written sync between the two.
 */

/** D-031 §1: dark is the default; light is a maintained secondary; `system`
 *  follows the OS. */
export type ThemeChoice = "dark" | "light" | "system";
/** D-031 §5: compact by default, one comfortable option. */
export type Density = "compact" | "comfortable";

interface UiState {
  theme: ThemeChoice;
  density: Density;
  editorPaneOpen: boolean;
  draftQuery: string;
  setTheme: (t: ThemeChoice) => void;
  cycleTheme: () => void;
  setDensity: (d: Density) => void;
  setEditorPaneOpen: (open: boolean) => void;
  setDraftQuery: (q: string) => void;
}

const THEME_ORDER: ThemeChoice[] = ["dark", "light", "system"];

export const useUiStore = create<UiState>((set) => ({
  theme: "dark",
  density: "compact",
  editorPaneOpen: true,
  draftQuery: "select 1;",
  setTheme: (theme) => set({ theme }),
  cycleTheme: () => set((s) => ({ theme: THEME_ORDER[(THEME_ORDER.indexOf(s.theme) + 1) % THEME_ORDER.length] })),
  setDensity: (density) => set({ density }),
  setEditorPaneOpen: (editorPaneOpen) => set({ editorPaneOpen }),
  setDraftQuery: (draftQuery) => set({ draftQuery }),
}));

/** Which palette a choice resolves to, given what the OS prefers. */
export function resolveTheme(choice: ThemeChoice, osPrefersLight: boolean): "dark" | "light" {
  if (choice === "system") return osPrefersLight ? "light" : "dark";
  return choice;
}

/**
 * Apply the theme and density as classes on <html>, so every surface —
 * including portals and the body background — reads the same tokens.
 */
export function applyAppearance(
  root: { classList: { toggle: (c: string, on: boolean) => void } },
  theme: "dark" | "light",
  density: Density,
): void {
  root.classList.toggle("light", theme === "light");
  root.classList.toggle("dark", theme === "dark");
  root.classList.toggle("comfortable", density === "comfortable");
}

/**
 * T-033 / D-014: what survives a restart, via the core's disposable cache
 * store. Deliberately NOT `draftQuery`: editor text can hold credentials
 * (R-030), and the cache dir sits outside the redaction path that history and
 * logs go through.
 */
interface PersistedUi {
  theme?: ThemeChoice;
  density?: Density;
  editorPaneOpen?: boolean;
}

function persisted(s: UiState): PersistedUi {
  return { theme: s.theme, density: s.density, editorPaneOpen: s.editorPaneOpen };
}

/** Hydrate from the cache, then write back (debounced) on change. */
export async function initUiPersistence(): Promise<void> {
  try {
    const saved = (await loadUiState()) as PersistedUi | null;
    if (saved && typeof saved === "object") {
      useUiStore.setState({
        ...(THEME_ORDER.includes(saved.theme as ThemeChoice) ? { theme: saved.theme as ThemeChoice } : {}),
        ...(saved.density === "compact" || saved.density === "comfortable" ? { density: saved.density } : {}),
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
