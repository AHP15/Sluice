import { useEffect, useRef } from "react";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { sql } from "@codemirror/lang-sql";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { tags } from "@lezer/highlight";

/**
 * T-092 / D-031: syntax colours come from their own desaturated tokens —
 * never the accent or a D-032 status hue, so code never reads as a status.
 * Every colour is a CSS variable, so the theme switch needs no re-mount.
 */
const tokenColour = (v: string) => `hsl(var(--${v}))`;
const sluiceHighlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.operatorKeyword, tags.modifier], color: tokenColour("syn-keyword") },
  { tag: [tags.string, tags.special(tags.string)], color: tokenColour("syn-string") },
  { tag: [tags.number, tags.bool, tags.null], color: tokenColour("syn-number") },
  { tag: [tags.comment, tags.lineComment, tags.blockComment], color: tokenColour("muted-foreground"), fontStyle: "italic" },
  { tag: [tags.typeName, tags.standard(tags.name)], color: tokenColour("syn-keyword") },
]);

const sluiceTheme = EditorView.theme({
  "&": { height: "100%", fontSize: "12.5px", backgroundColor: "transparent", color: tokenColour("foreground") },
  ".cm-scroller": {
    fontFamily: '"JetBrains Mono Variable", ui-monospace, SFMono-Regular, Menlo, monospace',
    lineHeight: "1.6",
  },
  ".cm-content": { caretColor: tokenColour("primary") },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: tokenColour("primary") },
  "&.cm-focused": { outline: "none" },
  ".cm-gutters": { backgroundColor: "transparent", border: "none", color: "hsl(var(--muted-foreground) / 0.6)" },
  ".cm-activeLine": { backgroundColor: "hsl(var(--foreground) / 0.03)" },
  ".cm-activeLineGutter": { backgroundColor: "transparent", color: tokenColour("muted-foreground") },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "hsl(var(--primary) / 0.25)",
  },
  ".cm-matchingBracket": { backgroundColor: "hsl(var(--primary) / 0.15)", outline: "1px solid hsl(var(--primary) / 0.5)" },
});

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
        sluiceTheme,
        syntaxHighlighting(sluiceHighlight),
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
