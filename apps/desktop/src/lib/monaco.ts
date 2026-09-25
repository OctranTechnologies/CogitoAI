import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";

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

monaco.editor.defineTheme(MONACO_THEME, {
  base: "vs-dark",
  inherit: true,
  rules: [
    { token: "comment", foreground: "5c6b7f", fontStyle: "italic" },
    { token: "keyword", foreground: "7dd3fc" },
    { token: "string", foreground: "5eead4" },
    { token: "number", foreground: "fbbf24" },
    { token: "key", foreground: "7dd3fc" },
    { token: "type", foreground: "c8d0dc" },
  ],
  colors: {
    "editor.background": "#101318",
    "editor.foreground": "#e7ebf1",
    "editorLineNumber.foreground": "#3a4554",
    "editorLineNumber.activeForeground": "#94a0b2",
    "editor.selectionBackground": "#1e3a4d",
    "editor.lineHighlightBackground": "#151920",
    "editorCursor.foreground": "#38bdf8",
    "editorIndentGuide.background1": "#1b2029",
    "editorGutter.background": "#101318",
    "diffEditor.insertedTextBackground": "#0f3d33aa",
    "diffEditor.removedTextBackground": "#4a1f2baa",
    "diffEditor.insertedLineBackground": "#0f3d3355",
    "diffEditor.removedLineBackground": "#4a1f2b55",
    "diffEditor.diagramFill": "#0ea5e933",
    "diffEditor.diagramInsertedFill": "#5eead455",
    "diffEditor.diagramRemovedFill": "#fb718555",
    "editorWidget.background": "#151920",
    "editorWidget.border": "#28303c",
    "scrollbarSlider.background": "#28303c99",
    "scrollbarSlider.hoverBackground": "#3a4554aa",
    "scrollbarSlider.activeBackground": "#3a4554",
  },
});

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
