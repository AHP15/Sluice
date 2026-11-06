import { useEffect, useRef } from "react";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { sql } from "@codemirror/lang-sql";

/**
 * Query editor pane (T-002, D-023).
 *
 * CodeMirror 6 rather than Monaco: far lighter, and its worker model behaves in
 * a Tauri asset context where Monaco's is awkward.
 *
 * A thin hand-rolled wrapper rather than a React binding package — the editor is
 * a long-lived imperative object and this keeps the dependency surface small.
 */
export function QueryEditor({
  value,
  onChange,
  onRun,
}: {
  value: string;
  onChange: (next: string) => void;
  onRun?: () => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  // Kept in a ref so the run keybinding never closes over a stale callback.
  const onRunRef = useRef(onRun);
  onRunRef.current = onRun;

  useEffect(() => {
    if (!host.current) return;

    const state = EditorState.create({
      doc: value,
      extensions: [
        lineNumbers(),
        highlightActiveLine(),
        history(),
        sql(),
        keymap.of([
          {
            key: "Mod-Enter",
            preventDefault: true,
            run: () => {
              onRunRef.current?.();
              return true;
            },
          },
          ...defaultKeymap,
          ...historyKeymap,
        ]),
        EditorView.updateListener.of((u) => {
          if (u.docChanged) onChange(u.state.doc.toString());
        }),
        EditorView.theme({
          "&": { height: "100%", fontSize: "13px" },
          ".cm-scroller": { fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace" },
        }),
      ],
    });

    const v = new EditorView({ state, parent: host.current });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
    // Mount once; external value changes are reconciled below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Reconcile only when the incoming value genuinely diverges, so typing does
  // not fight the controlled prop.
  useEffect(() => {
    const v = view.current;
    if (!v) return;
    const current = v.state.doc.toString();
    if (current !== value) {
      v.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    }
  }, [value]);

  return <div ref={host} className="h-full min-h-0 overflow-hidden" />;
}
