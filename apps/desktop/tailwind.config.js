/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    // Every value resolves to a CSS custom property declared in `src/styles.css`.
    // Changing the palette happens there and nowhere else. Channel-form colours
    // keep Tailwind's `/opacity` modifier working.
    colors: {
      transparent: "transparent",
      current: "currentColor",
      inherit: "inherit",
      app: "rgb(var(--surface-app) / <alpha-value>)",
      panel: "rgb(var(--surface-panel) / <alpha-value>)",
      elevated: "rgb(var(--surface-elevated) / <alpha-value>)",
      overlay: "rgb(var(--surface-overlay) / <alpha-value>)",
      sunken: "rgb(var(--surface-sunken) / <alpha-value>)",
      hover: "rgb(var(--surface-hover) / <alpha-value>)",
      active: "rgb(var(--surface-active) / <alpha-value>)",
      scrim: "rgb(var(--surface-scrim) / <alpha-value>)",
      line: "rgb(var(--border-subtle) / <alpha-value>)",
      "line-strong": "rgb(var(--border-default) / <alpha-value>)",
      "line-stronger": "rgb(var(--border-strong) / <alpha-value>)",
      primary: "rgb(var(--text-primary) / <alpha-value>)",
      secondary: "rgb(var(--text-secondary) / <alpha-value>)",
      muted: "rgb(var(--text-muted) / <alpha-value>)",
      faint: "rgb(var(--text-faint) / <alpha-value>)",
      inverse: "rgb(var(--text-inverse) / <alpha-value>)",
      accent: "rgb(var(--accent) / <alpha-value>)",
      "accent-strong": "rgb(var(--accent-strong) / <alpha-value>)",
      "accent-dim": "rgb(var(--accent-dim) / <alpha-value>)",
      success: "rgb(var(--success) / <alpha-value>)",
      warning: "rgb(var(--warning) / <alpha-value>)",
      error: "rgb(var(--error) / <alpha-value>)",
      info: "rgb(var(--info) / <alpha-value>)",
    },
    borderRadius: {
      none: "0px",
      sm: "var(--radius-sm)",
      md: "var(--radius-md)",
      lg: "var(--radius-lg)",
      xl: "var(--radius-xl)",
      full: "var(--radius-full)",
    },
    boxShadow: {
      none: "none",
      panel: "var(--shadow-panel)",
      overlay: "var(--shadow-overlay)",
    },
    fontFamily: {
      // No webfont is fetched: the interface must render offline and open fast.
      sans: [
        "ui-sans-serif",
        "system-ui",
        "-apple-system",
        "BlinkMacSystemFont",
        "Segoe UI",
        "sans-serif",
      ],
      mono: ["JetBrains Mono", "ui-monospace", "SFMono-Regular", "Menlo", "monospace"],
    },
    // Compact scale. A developer tool favours density over generous line height,
    // so the steps below the default 16px are the ones actually used.
    fontSize: {
      "2xs": ["10px", { lineHeight: "14px" }],
      xs: ["11px", { lineHeight: "16px" }],
      sm: ["12px", { lineHeight: "18px" }],
      base: ["13px", { lineHeight: "20px" }],
      md: ["14px", { lineHeight: "20px" }],
      lg: ["16px", { lineHeight: "24px" }],
      xl: ["20px", { lineHeight: "28px" }],
    },
    // NOTE: `spacing` is intentionally not set at the top level. Declaring it
    // there replaces Tailwind's default scale outright, which silently deletes
    // every `p-*`, `m-*`, `gap-*`, and `min-h-*` utility. The named steps below
    // live in `extend.spacing` so they merge with the defaults instead.
    transitionDuration: {
      fast: "var(--duration-fast)",
      DEFAULT: "var(--duration-base)",
    },
    transitionTimingFunction: {
      standard: "var(--ease-standard)",
    },
    extend: {
      // Merged into the default 4px scale rather than replacing it. These are
      // the named steps the shell repeats, so layout reads as intent rather than
      // as a magic number.
      spacing: {
        gutter: "12px",
        control: "28px",
        "control-sm": "24px",
        "control-lg": "32px",
        bar: "56px",
        rail: "24px",
        // Icon sizes. Declared as spacing keys so the `size-*` utility resolves
        // them, which keeps every glyph on the same small set of steps.
        icon: "14px",
        "icon-xs": "10px",
        "icon-sm": "12px",
        "icon-md": "14px",
        "icon-lg": "16px",
        "icon-xl": "20px",
        "icon-2xl": "24px",
      },
      transitionProperty: {
        colors: "background-color, border-color, color, fill, stroke, opacity",
      },
      keyframes: {
        "fade-in": {
          from: { opacity: "0" },
          to: { opacity: "1" },
        },
        "scale-in": {
          from: { opacity: "0", transform: "translateY(-2px) scale(0.99)" },
          to: { opacity: "1", transform: "translateY(0) scale(1)" },
        },
        "slide-up": {
          from: { opacity: "0", transform: "translateY(4px)" },
          to: { opacity: "1", transform: "translateY(0)" },
        },
      },
      animation: {
        "fade-in": "fade-in var(--duration-base) var(--ease-standard)",
        "scale-in": "scale-in var(--duration-fast) var(--ease-standard)",
        "slide-up": "slide-up var(--duration-base) var(--ease-standard)",
      },
    },
  },
  plugins: [],
};
