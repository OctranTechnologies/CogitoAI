/**
 * Runtime access to the design tokens.
 *
 * Monaco and xterm are configured from JavaScript, so they cannot use Tailwind
 * classes. Rather than letting each keep its own copy of the palette, they read
 * the same CSS custom properties the rest of the interface uses. A theme change
 * in `styles.css` therefore reaches the editor and the terminal too.
 *
 * Tokens are stored as bare RGB channels, so composing an alpha suffix is a
 * matter of appending `/<alpha>`, which the browser resolves to a colour
 * function. Values are read once per call and cached per theme, since a theme
 * swap changes the computed values.
 */

type Channel = string;

const cache = new Map<string, string>();

/** The fallback mirrors the dark theme in `styles.css`, used before first paint. */
const FALLBACK: Record<string, Channel> = {
  "surface-app": "10 12 15",
  "surface-panel": "15 18 23",
  "surface-elevated": "21 25 32",
  "surface-overlay": "26 31 39",
  "surface-sunken": "8 10 12",
  "surface-hover": "25 30 38",
  "surface-active": "32 39 49",
  "border-subtle": "23 27 33",
  "border-default": "32 39 49",
  "border-strong": "48 57 70",
  "text-primary": "232 236 242",
  "text-secondary": "196 205 218",
  "text-muted": "143 155 173",
  "text-faint": "100 112 131",
  "text-inverse": "8 10 12",
  accent: "56 189 248",
  "accent-strong": "125 211 252",
  success: "94 234 212",
  warning: "251 191 36",
  error: "251 113 133",
};

/** Drops the cache so the next read picks up a theme change. */
export function resetTokenCache(): void {
  cache.clear();
}

function readVariable(name: string): Channel {
  const cached = cache.get(name);
  if (cached) return cached;
  let value = FALLBACK[name] ?? "0 0 0";
  if (typeof window !== "undefined") {
    const computed = getComputedStyle(document.documentElement)
      .getPropertyValue(`--${name}`)
      .trim();
    if (computed) value = computed;
  }
  cache.set(name, value);
  return value;
}

/**
 * Returns a token as a CSS colour, optionally with an alpha channel.
 *
 * `color("accent")` yields `rgb(56 189 248)`, and `color("accent", 0.4)` yields
 * `rgb(56 189 248 / 0.4)`. Both are valid for canvas, Monaco, and xterm, none
 * of which can resolve a `var()` reference on their own.
 */
export function color(name: string, alpha?: number): string {
  const channels = readVariable(name);
  return alpha === undefined
    ? `rgb(${channels})`
    : `rgb(${channels} / ${alpha})`;
}

/** Returns a token mixed toward transparency, for a subtle fill or border. */
export function alpha(name: string, value: number): string {
  return color(name, value);
}

/**
 * Returns a token as a bare hex triplet without the leading `#`.
 *
 * Monaco's tokeniser rules take colour as a six-digit hex string and reject a
 * colour function, so this converts the channel form rather than duplicating the
 * value. Channels outside 0-255 are clamped, which keeps a malformed token from
 * producing an invalid theme.
 */
export function hex(name: string): string {
  const channels = readVariable(name)
    .split(/\s+/)
    .map((channel) => {
      const value = Number.parseFloat(channel);
      if (!Number.isFinite(value)) return 0;
      return Math.max(0, Math.min(255, Math.round(value)));
    })
    .slice(0, 3)
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
  return channels.length === 6 ? channels : "000000";
}
