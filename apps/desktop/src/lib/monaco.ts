import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";
import { alpha, color, hex } from "./tokens";

// The desktop shell is an offline application, so Monaco must be served from
// the local bundle. The default loader fetches the editor from a CDN, which
// would leave code viewing and diffing broken without network access.
loader.config({ monaco });

// The code viewers are strictly read-only, so only syntax tokenizers are
// registered. Language *services* (completion, diagnostics, formatting) all run
// in web workers, which are unnecessary here and would roughly double the
// desktop bundle for no benefit. Registering the tokenizer definitions keeps
// highlighting on the main thread.
import "monaco-editor/esm/vs/languages/definitions/rust/register";
import "monaco-editor/esm/vs/languages/definitions/typescript/register";
import "monaco-editor/esm/vs/languages/definitions/javascript/register";
import "monaco-editor/esm/vs/languages/definitions/ini/register";
import "monaco-editor/esm/vs/languages/definitions/css/register";
import "monaco-editor/esm/vs/languages/definitions/scss/register";
import "monaco-editor/esm/vs/languages/definitions/less/register";
import "monaco-editor/esm/vs/languages/definitions/html/register";
import "monaco-editor/esm/vs/languages/definitions/markdown/register";
import "monaco-editor/esm/vs/languages/definitions/python/register";
import "monaco-editor/esm/vs/languages/definitions/go/register";
import "monaco-editor/esm/vs/languages/definitions/java/register";
import "monaco-editor/esm/vs/languages/definitions/kotlin/register";
import "monaco-editor/esm/vs/languages/definitions/cpp/register";
import "monaco-editor/esm/vs/languages/definitions/csharp/register";
import "monaco-editor/esm/vs/languages/definitions/ruby/register";
import "monaco-editor/esm/vs/languages/definitions/php/register";
import "monaco-editor/esm/vs/languages/definitions/shell/register";
import "monaco-editor/esm/vs/languages/definitions/powershell/register";
import "monaco-editor/esm/vs/languages/definitions/bat/register";
import "monaco-editor/esm/vs/languages/definitions/sql/register";
import "monaco-editor/esm/vs/languages/definitions/yaml/register";
import "monaco-editor/esm/vs/languages/definitions/xml/register";
import "monaco-editor/esm/vs/languages/definitions/dockerfile/register";
import "monaco-editor/esm/vs/languages/definitions/lua/register";
import "monaco-editor/esm/vs/languages/definitions/swift/register";
import "monaco-editor/esm/vs/languages/definitions/dart/register";
import "monaco-editor/esm/vs/languages/definitions/elixir/register";
import "monaco-editor/esm/vs/languages/definitions/scala/register";

// Monaco ships JSON only as a worker-backed language service, so a compact
// tokenizer is registered here to keep `package.json`-style files highlighted.
monaco.languages.register({ id: "json" });
monaco.languages.setMonarchTokensProvider("json", {
  defaultToken: "",
  tokenPostfix: ".json",
  tokenizer: {
    root: [
      [/"(?:[^"\\]|\\.)*"(?=\s*:)/, "key"],
      [/"(?:[^"\\]|\\.)*"/, "string"],
      [/-?\d+(\.\d+)?([eE][+-]?\d+)?/, "number"],
      [/\b(true|false|null)\b/, "keyword"],
      [/[{}[\]]/, "delimiter.bracket"],
      [/[,:]/, "delimiter"],
    ],
  },
});
monaco.languages.setLanguageConfiguration("json", {
  comments: { lineComment: "//", blockComment: ["/*", "*/"] },
  brackets: [
    ["{", "}"],
    ["[", "]"],
  ],
  autoClosingPairs: [
    { open: "{", close: "}" },
    { open: "[", close: "]" },
    { open: '"', close: '"', notIn: ["string"] },
  ],
});

export const MONACO_THEME = "cogito-dark";

/**
 * Monaco is configured from JavaScript, so it cannot use the Tailwind classes the
 * rest of the interface uses. It reads the same CSS custom properties through
 * `lib/tokens` instead of keeping a second copy of the palette, which is what
 * lets a theme change reach the editor.
 */
function defineMonacoTheme() {
  monaco.editor.defineTheme(MONACO_THEME, {
    base: "vs-dark",
    inherit: true,
    rules: [
      { token: "comment", foreground: hex("text-faint"), fontStyle: "italic" },
      { token: "keyword", foreground: hex("accent-strong") },
      { token: "string", foreground: hex("success") },
      { token: "number", foreground: hex("warning") },
      { token: "key", foreground: hex("accent-strong") },
      { token: "type", foreground: hex("text-secondary") },
    ],
    colors: {
      "editor.background": color("surface-panel"),
      "editor.foreground": color("text-primary"),
      "editorLineNumber.foreground": color("border-strong"),
      "editorLineNumber.activeForeground": color("text-muted"),
      "editor.selectionBackground": alpha("accent", 0.22),
      "editor.lineHighlightBackground": color("surface-elevated"),
      "editorCursor.foreground": color("accent"),
      "editorIndentGuide.background1": color("border-subtle"),
      "editorGutter.background": color("surface-panel"),
      "diffEditor.insertedTextBackground": alpha("success", 0.16),
      "diffEditor.removedTextBackground": alpha("error", 0.16),
      "diffEditor.insertedLineBackground": alpha("success", 0.07),
      "diffEditor.removedLineBackground": alpha("error", 0.07),
      "diffEditor.diagramInsertedFill": alpha("success", 0.28),
      "diffEditor.diagramRemovedFill": alpha("error", 0.28),
      "editorWidget.background": color("surface-elevated"),
      "editorWidget.border": color("border-default"),
      "scrollbarSlider.background": alpha("border-default", 0.7),
      "scrollbarSlider.hoverBackground": alpha("border-strong", 0.7),
      "scrollbarSlider.activeBackground": color("border-strong"),
    },
  });
}

defineMonacoTheme();

/** Read-only Monaco options shared by the source and diff viewers. */
export const READ_ONLY_OPTIONS: monaco.editor.IStandaloneEditorConstructionOptions = {
  readOnly: true,
  domReadOnly: true,
  automaticLayout: true,
  scrollBeyondLastLine: false,
  minimap: { enabled: false },
  fontSize: 12,
  lineHeight: 20,
  fontFamily: '"JetBrains Mono", ui-monospace, SFMono-Regular, monospace',
  fontLigatures: true,
  renderLineHighlight: "line",
  smoothScrolling: true,
  padding: { top: 10, bottom: 10 },
  contextmenu: false,
  quickSuggestions: false,
  folding: true,
  glyphMargin: false,
  lineNumbersMinChars: 3,
  overviewRulerLanes: 0,
  scrollbar: {
    verticalScrollbarSize: 10,
    horizontalScrollbarSize: 10,
    useShadows: false,
  },
};

/** Diff-editor options per layout; both sides stay locked to read-only. */
export const DIFF_OPTIONS: Record<"split" | "inline", monaco.editor.IDiffEditorConstructionOptions> = {
  split: { ...READ_ONLY_OPTIONS, renderSideBySide: true },
  inline: { ...READ_ONLY_OPTIONS, renderSideBySide: false },
};

export { monaco };
