/** @type {import('tailwindcss').Config} */
const token = (name) => `hsl(var(--${name}) / <alpha-value>)`;

export default {
  // T-092 / D-031: dark is the default and lives on :root; `.light` overrides.
  // `darkMode` stays class-based so a `dark:` variant still means "not .light".
  darkMode: ["class"],
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        // shadcn/ui token set plus D-031 surfaces and D-032 status hues,
        // all as hsl(var(--x)) — no color-mix(), no @property, both of which
        // are above the WebKitGTK 4.1 floor (D-023).
        border: { DEFAULT: token("border"), strong: token("border-strong") },
        input: token("input"),
        ring: token("ring"),
        background: token("background"),
        foreground: token("foreground"),
        panel: token("panel"),
        raised: token("raised"),
        overlay: token("overlay"),
        primary: { DEFAULT: token("primary"), foreground: token("primary-foreground") },
        secondary: { DEFAULT: token("secondary"), foreground: token("secondary-foreground") },
        muted: { DEFAULT: token("muted"), foreground: token("muted-foreground") },
        accent: { DEFAULT: token("accent"), foreground: token("accent-foreground") },
        // D-032: reserved, never decorative.
        destructive: { DEFAULT: token("destructive"), foreground: token("destructive-foreground") },
        caution: { DEFAULT: token("caution"), foreground: token("caution-foreground") },
        verified: { DEFAULT: token("verified"), foreground: token("verified-foreground") },
        info: { DEFAULT: token("info"), foreground: token("info-foreground") },
        syntax: { keyword: token("syn-keyword"), string: token("syn-string"), number: token("syn-number") },
      },
      borderRadius: {
        // D-031 §4: 4px — machined, not soft.
        lg: "var(--radius)",
        md: "calc(var(--radius) - 1px)",
        sm: "calc(var(--radius) - 2px)",
      },
      fontFamily: {
        // D-031 §3: bundled Inter and JetBrains Mono.
        sans: ["Inter Variable", "ui-sans-serif", "system-ui", "-apple-system", "Segoe UI", "sans-serif"],
        mono: ["JetBrains Mono Variable", "ui-monospace", "SFMono-Regular", "Menlo", "Consolas", "monospace"],
      },
      fontSize: {
        // D-031 §5: compact scale. 13px UI, 12.5px data.
        "2xs": ["11px", "16px"],
        xs: ["12px", "16px"],
        data: ["12.5px", "18px"],
        sm: ["13px", "18px"],
        base: ["14px", "20px"],
      },
      height: { row: "var(--row-h)" },
      minHeight: { row: "var(--row-h)" },
      boxShadow: {
        // D-031 §4: shadows only on floating layers; glow only on interaction.
        float: "0 12px 32px hsl(var(--shadow-float) / 0.5)",
        glow: "0 0 0 1px hsl(var(--ring) / 0.45), 0 0 12px hsl(var(--ring) / 0.25)",
        "inset-accent": "inset 2px 0 0 hsl(var(--primary))",
        "inset-caution": "inset 2px 0 0 hsl(var(--caution))",
      },
      transitionDuration: {
        // D-031 §6: interaction 120ms, overlays 180ms.
        interact: "120ms",
        overlay: "180ms",
      },
      letterSpacing: { label: "0.06em" },
    },
  },
  plugins: [],
};
