/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        ink: {
          950: "#0b0d10",
          900: "#101318",
          850: "#151920",
          800: "#1b2029",
          700: "#28303c",
          600: "#3a4554",
          500: "#647184",
          400: "#94a0b2",
          300: "#c8d0dc",
          200: "#e7ebf1",
          100: "#f5f7fa",
        },
        signal: {
          400: "#7dd3fc",
          500: "#38bdf8",
          600: "#0ea5e9",
        },
        success: "#5eead4",
        warning: "#fbbf24",
        danger: "#fb7185",
      },
      fontFamily: {
        sans: ["ui-sans-serif", "system-ui", "-apple-system", "BlinkMacSystemFont", "Segoe UI", "sans-serif"],
        mono: ["JetBrains Mono", "ui-monospace", "SFMono-Regular", "monospace"],
      },
      boxShadow: {
        panel: "0 14px 38px rgba(0, 0, 0, 0.22)",
      },
    },
  },
  plugins: [],
};
