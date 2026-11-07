import type { ReactNode } from "react";
import { AlertTriangle, CheckCircle2, Info, ShieldAlert } from "lucide-react";
import { cn } from "../../lib/utils";

/**
 * D-032: the four reserved status hues, plus neutral. Every status is hue +
 * icon + label — never colour alone — and nothing decorative ever uses these.
 *
 * - danger:   destructive actions, errors, production
 * - caution:  TLS opt-out, an open transaction, truncation, budget warnings
 * - verified: a guarantee the server enforces (read-only, committed)
 * - info:     notes
 */
export type Tone = "danger" | "caution" | "verified" | "info" | "neutral";

const TONE_TEXT: Record<Tone, string> = {
  danger: "text-destructive border-destructive/60 bg-destructive/10",
  caution: "text-caution border-caution/60 bg-caution/10",
  verified: "text-verified border-verified/60 bg-verified/10",
  info: "text-info border-info/50 bg-info/10",
  neutral: "text-muted-foreground border-border-strong bg-raised",
};

const TONE_ICON: Record<Exclude<Tone, "neutral">, typeof Info> = {
  danger: ShieldAlert,
  caution: AlertTriangle,
  verified: CheckCircle2,
  info: Info,
};

export function StatusPill({
  tone,
  children,
  title,
  icon = true,
  className,
}: {
  tone: Tone;
  children: ReactNode;
  title?: string;
  icon?: boolean;
  className?: string;
}) {
  const Icon = tone === "neutral" || !icon ? null : TONE_ICON[tone];
  return (
    <span
      title={title}
      className={cn(
        "inline-flex h-5 shrink-0 items-center gap-1 whitespace-nowrap rounded-sm border px-1.5 text-2xs font-semibold uppercase tracking-label",
        TONE_TEXT[tone],
        className,
      )}
    >
      {Icon && <Icon aria-hidden className="h-3 w-3" />}
      {children}
    </span>
  );
}

/** A full-width notice row (a TLS opt-out, a failed run). */
export function StatusBanner({
  tone,
  children,
  className,
  role,
}: {
  tone: Exclude<Tone, "neutral">;
  children: ReactNode;
  className?: string;
  role?: "alert" | "status" | "note";
}) {
  const Icon = TONE_ICON[tone];
  return (
    <div
      role={role ?? (tone === "danger" ? "alert" : "status")}
      className={cn("flex items-start gap-2 border-b px-3 py-1.5 text-xs", TONE_TEXT[tone], className)}
    >
      <Icon aria-hidden className="mt-px h-3.5 w-3.5 shrink-0" />
      <div className="min-w-0 flex-1 text-foreground">{children}</div>
    </div>
  );
}
