import { create } from "zustand";

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
