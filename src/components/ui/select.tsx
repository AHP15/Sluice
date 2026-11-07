/**
 * A themed dropdown, in place of the native `<select>`.
 *
 * The native control's popup is drawn by the OS toolkit (GTK under the Linux
 * webview), so no stylesheet reaches it: it ignores the theme, the density and
 * the fonts. This one is a button plus a listbox, with the keyboard model of a
 * native select — arrows, Home/End, type-to-find, Enter/Space to choose,
 * Escape or Tab to leave — and the ARIA combobox/listbox roles.
 *
 * The list is portalled to <body> with fixed positioning, so it is never
 * clipped by a scrolling or `overflow: hidden` ancestor (a grid cell, a
 * dialog body). Only CSS the WebKitGTK floor supports (D-023).
 */
import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronDown } from "lucide-react";

import { cn } from "../../lib/utils";

export interface SelectOption<V extends string = string> {
  value: V;
  label: string;
}

const sizes = {
  sm: "h-6 px-1.5 text-data",
  md: "h-8 px-2.5 text-sm",
} as const;

export function Select<V extends string = string>({
  value,
  options,
  onChange,
  onDismiss,
  size = "md",
  mono = false,
  autoFocus,
  defaultOpen = false,
  disabled,
  className,
  placeholder,
  "aria-label": ariaLabel,
  id,
}: {
  value: V;
  options: readonly SelectOption<V>[];
  onChange: (value: V) => void;
  /** The list closed without a choice (Escape, Tab, a click elsewhere). */
  onDismiss?: () => void;
  size?: keyof typeof sizes;
  mono?: boolean;
  autoFocus?: boolean;
  defaultOpen?: boolean;
  disabled?: boolean;
  className?: string;
  placeholder?: string;
  "aria-label"?: string;
  id?: string;
}) {
  const listId = useId();
  const triggerRef = useRef<HTMLButtonElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const [open, setOpen] = useState(defaultOpen);
  const selectedIndex = options.findIndex((o) => o.value === value);
  const [active, setActive] = useState(Math.max(selectedIndex, 0));
  const [pos, setPos] = useState<{ left: number; top: number; width: number; up: boolean } | null>(null);
  const typed = useRef({ text: "", at: 0 });

  const current = options[selectedIndex];

  const place = () => {
    const r = triggerRef.current?.getBoundingClientRect();
    if (!r) return;
    const below = window.innerHeight - r.bottom;
    const up = below < 220 && r.top > below;
    setPos({ left: r.left, top: up ? r.top : r.bottom, width: r.width, up });
  };

  const openList = () => {
    if (disabled) return;
    setActive(Math.max(selectedIndex, 0));
    place();
    setOpen(true);
  };

  const close = (dismissed: boolean) => {
    setOpen(false);
    if (dismissed) onDismiss?.();
  };

  const choose = (i: number) => {
    const o = options[i];
    if (!o) return;
    setOpen(false);
    triggerRef.current?.focus();
    if (o.value !== value) onChange(o.value);
    else onDismiss?.();
  };

  useLayoutEffect(() => {
    if (open) place();
  }, [open]);

  // Keep the active option in view, and follow layout changes while open.
  useEffect(() => {
    if (!open) return;
    listRef.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView?.({ block: "nearest" });
  }, [open, active]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (triggerRef.current?.contains(t) || listRef.current?.contains(t)) return;
      close(true);
    };
    const onMove = () => place();
    document.addEventListener("mousedown", onDown);
    window.addEventListener("resize", onMove);
    window.addEventListener("scroll", onMove, true);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("resize", onMove);
      window.removeEventListener("scroll", onMove, true);
    };
    // close/place read only refs and props current at call time.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const findTyped = (key: string) => {
    const now = Date.now();
    typed.current = { text: now - typed.current.at < 700 ? typed.current.text + key : key, at: now };
    const q = typed.current.text.toLowerCase();
    const from = open ? active : Math.max(selectedIndex, 0);
    for (let k = 0; k < options.length; k++) {
      // A repeated single letter cycles; a longer prefix stays put if it matches.
      const i = (from + (q.length === 1 ? k + 1 : k)) % options.length;
      if (options[i].label.toLowerCase().startsWith(q)) return i;
    }
    return -1;
  };

  const onKeyDown = (e: KeyboardEvent) => {
    const last = options.length - 1;
    switch (e.key) {
      case "ArrowDown":
      case "ArrowUp": {
        e.preventDefault();
        if (!open) return openList();
        const step = e.key === "ArrowDown" ? 1 : -1;
        setActive((a) => Math.min(last, Math.max(0, a + step)));
        return;
      }
      case "Home":
      case "End":
        if (!open) return;
        e.preventDefault();
        setActive(e.key === "Home" ? 0 : last);
        return;
      case "Enter":
      case " ":
        e.preventDefault();
        if (open) choose(active);
        else openList();
        return;
      case "Escape":
        if (open) {
          e.preventDefault();
          e.stopPropagation();
          close(true);
        } else {
          onDismiss?.();
        }
        return;
      case "Tab":
        if (open) close(true);
        return;
      default:
        if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
          const i = findTyped(e.key);
          if (i < 0) return;
          e.preventDefault();
          // Closed, like a native select: type-to-find changes the value.
          if (open) setActive(i);
          else if (options[i].value !== value) onChange(options[i].value);
        }
    }
  };

  return (
    <>
      <button
        ref={triggerRef}
        id={id}
        type="button"
        role="combobox"
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-activedescendant={open ? `${listId}-${active}` : undefined}
        data-value={value}
        disabled={disabled}
        autoFocus={autoFocus}
        className={cn(
          "focus-glow inline-flex min-w-0 items-center justify-between gap-1.5 rounded border border-input bg-raised text-left text-foreground transition-[border-color,box-shadow,background-color] duration-interact hover:border-border-strong hover:bg-accent disabled:pointer-events-none disabled:opacity-50",
          open && "border-ring",
          sizes[size],
          mono && "font-mono",
          className,
        )}
        onClick={() => (open ? close(true) : openList())}
        onKeyDown={onKeyDown}
      >
        <span className={cn("truncate", !current && "text-muted-foreground")}>{current?.label ?? placeholder ?? ""}</span>
        <ChevronDown
          aria-hidden="true"
          className={cn(
            "h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform duration-interact",
            open && "rotate-180",
          )}
        />
      </button>
      {open &&
        pos &&
        createPortal(
          <ul
            ref={listRef}
            id={listId}
            role="listbox"
            aria-label={ariaLabel}
            tabIndex={-1}
            style={{
              position: "fixed",
              left: pos.left,
              minWidth: pos.width,
              ...(pos.up ? { bottom: window.innerHeight - pos.top + 4 } : { top: pos.top + 4 }),
            }}
            className={cn(
              "z-50 max-h-64 overflow-auto rounded border border-border-strong bg-panel py-1 shadow-float",
              size === "sm" ? "text-data" : "text-sm",
              mono && "font-mono",
            )}
            // Keep focus on the trigger so its keyboard handling stays live.
            onMouseDown={(e) => e.preventDefault()}
          >
            {options.map((o, i) => {
              const selected = i === selectedIndex;
              return (
                <li
                  key={o.value}
                  id={`${listId}-${i}`}
                  data-index={i}
                  data-value={o.value}
                  role="option"
                  aria-selected={selected}
                  className={cn(
                    "flex cursor-default select-none items-center gap-2 whitespace-nowrap py-1 pl-2 pr-3",
                    i === active ? "bg-primary text-primary-foreground" : "text-foreground",
                  )}
                  onMouseEnter={() => setActive(i)}
                  onClick={() => choose(i)}
                >
                  <Check aria-hidden="true" className={cn("h-3.5 w-3.5 shrink-0", !selected && "invisible")} />
                  <span className="truncate">{o.label}</span>
                </li>
              );
            })}
          </ul>,
          document.body,
        )}
    </>
  );
}
