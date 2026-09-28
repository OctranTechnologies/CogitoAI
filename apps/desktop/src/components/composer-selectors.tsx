import { useMemo, useRef, useState, type ReactNode } from "react";
import {
  Bot,
  Check,
  ChevronDown,
  Cpu,
  Eye,
  Layers3,
  Paperclip,
  PlugZap,
  RefreshCw,
  Search,
  ShieldCheck,
  Sparkles,
  Wrench,
} from "lucide-react";
import type {
  ModelDescriptor,
  ModelSettings,
  PermissionSettings,
  ProviderCredentialStatus,
} from "../lib/settings";
import { modeLabel } from "../lib/composer";
import { Badge, Dropdown, Popover, Tooltip, cx, type DropdownItem } from "./ui";

/**
 * Shared chrome for the two composer selectors.
 *
 * A control that cannot act is rendered inert and says why, so a visible button
 * is never a button that quietly does nothing. An active one is a real trigger:
 * clicking it opens the menu, so it works by pointer and by keyboard.
 */
function SelectorShell({
  label,
  icon,
  children,
  disabled,
  disabledReason,
}: {
  label: string;
  icon: ReactNode;
  children: ReactNode;
  disabled?: boolean;
  disabledReason?: string;
}) {
  const body = (
    <span
      className={cx(
        "inline-flex h-control-sm max-w-52 items-center gap-1.5 rounded-md border px-2",
        "text-2xs transition-colors duration-fast",
        disabled
          ? "cursor-not-allowed border-line text-faint"
          : "border-line-strong bg-panel text-secondary hover:border-line-stronger hover:text-primary",
      )}
    >
      {icon}
      {children}
      <ChevronDown className="size-icon-xs shrink-0 opacity-70" />
    </span>
  );

  // The active form is wrapped in Dropdown's own button, which carries the
  // label; adding a second one here would nest two names on one control.
  if (disabled) {
    return (
      <Tooltip label={disabledReason ?? "Connect a runtime to change this"}>
        <span aria-label={label} aria-disabled="true" role="button">
          {body}
        </span>
      </Tooltip>
    );
  }
  return body;
}

/**
 * Model selector.
 *
 * The runtime owns discovery and capability knowledge. The picker only groups
 * and filters that catalog; it never invents a provider's model list or effort
 * levels.
 */
export function ModelSelector({
  models,
  catalog,
  credentials,
  loading,
  onSelect,
  onRefresh,
  onConnect,
  onSelectReasoning,
  disabled,
  disabledReason,
}: {
  models: ModelSettings | null;
  catalog: ModelDescriptor[];
  credentials: ProviderCredentialStatus[];
  loading: boolean;
  onSelect: (provider: string, model: string) => void;
  onRefresh: (providerId: string) => void;
  onConnect: (providerId: string) => void;
  onSelectReasoning: (effort: string) => void;
  disabled?: boolean;
  disabledReason?: string;
}) {
  const [query, setQuery] = useState("");
  const listRef = useRef<HTMLDivElement | null>(null);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const providerRows = useMemo(() => {
    const specs = [
      { id: "openai", label: "OpenAI", icon: <Bot className="size-icon-sm" /> },
      { id: "anthropic", label: "Anthropic", icon: <Sparkles className="size-icon-sm" /> },
      { id: "gemini", label: "Google Gemini", icon: <Sparkles className="size-icon-sm" /> },
      { id: "opencode-zen", label: "OpenCode Zen", icon: <Layers3 className="size-icon-sm" /> },
      { id: "opencode-go", label: "OpenCode Go", icon: <Layers3 className="size-icon-sm" /> },
    ];
    return specs.map((provider) => {
      const credential = credentials.find((item) => item.provider_id === provider.id)?.credential;
      const models = catalog
        .filter((model) => model.provider === provider.id)
        .filter((model) => {
          if (!normalizedQuery) return true;
          return `${provider.label} ${model.display_name} ${model.id}`
            .toLocaleLowerCase()
            .includes(normalizedQuery);
        });
      return { ...provider, credential, models };
    }).filter((provider) => !normalizedQuery || provider.models.length > 0 || provider.label.toLocaleLowerCase().includes(normalizedQuery));
  }, [catalog, credentials, normalizedQuery]);

  if (!models || disabled) {
    return (
      <SelectorShell
        label="Model"
        icon={<Bot className="size-icon-sm" />}
        disabled
        disabledReason={disabledReason}
      >
        <span className="truncate">{models ? `${models.provider} / ${models.model}` : "No model"}</span>
      </SelectorShell>
    );
  }

  const currentModelId = models.model.startsWith(`${models.provider_id}/`)
    ? models.model.slice(models.provider_id.length + 1)
    : models.model;
  const selectedDescriptor = catalog.find(
    (item) => item.provider === models.provider_id && item.id.replace(`${models.provider_id}/`, "") === currentModelId,
  );
  const selectedLevels = models.reasoning_levels.length > 0
    ? models.reasoning_levels
    : selectedDescriptor?.metadata.reasoning_levels ?? [];
  const currentProviderLabel = providerLabel(models.provider_id);
  const effort = models.reasoning_effort ?? "off";

  function moveOption(event: React.KeyboardEvent<HTMLDivElement>) {
    if (!new Set(["ArrowDown", "ArrowUp", "Home", "End"]).has(event.key)) return;
    const options = Array.from(
      listRef.current?.querySelectorAll<HTMLButtonElement>("[data-model-option='true']") ?? [],
    );
    if (options.length === 0) return;
    const current = options.indexOf(document.activeElement as HTMLButtonElement);
    let next = current;
    if (event.key === "ArrowDown") next = (current + 1) % options.length;
    if (event.key === "ArrowUp") next = (current - 1 + options.length) % options.length;
    if (event.key === "Home") next = 0;
    if (event.key === "End") next = options.length - 1;
    event.preventDefault();
    options[next]?.focus();
  }

  return (
    <Popover
      label={`Choose model · ${currentProviderLabel} ${models.model}`}
      placement="top-start"
      panelClassName="w-[min(34rem,calc(100vw-2rem))] overflow-hidden p-0"
      trigger={
        <SelectorShell label="Model" icon={<Bot className="size-icon-sm" />}>
          <span className="max-w-44 truncate" title={`${currentProviderLabel} · ${models.model}`}>
            <span className="text-faint">{currentProviderLabel}</span>
            <span className="opacity-60"> / </span>
            {models.model}
          </span>
        </SelectorShell>
      }
      render={(close) => (
        <div onKeyDown={moveOption}>
          <div className="border-b border-line p-3">
            <label className="flex h-9 items-center gap-2 rounded-md border border-line-strong bg-app px-2.5 text-muted focus-within:border-accent">
              <Search className="size-icon-sm shrink-0 text-faint" />
              <input
                autoFocus
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder="Search models and providers"
                aria-label="Search models and providers"
                className="min-w-0 flex-1 bg-transparent text-xs text-primary outline-none placeholder:text-faint"
              />
              {loading ? <RefreshCw className="size-icon-sm animate-spin text-faint" /> : null}
            </label>
          </div>
          <div ref={listRef} className="max-h-[min(26rem,58vh)] overflow-y-auto p-1.5 scrollbar">
            {providerRows.map((provider) => {
              const connected = provider.credential?.available === true;
              return (
                <section key={provider.id} aria-label={provider.label} className="mb-1 last:mb-0">
                  <div className="sticky top-0 z-10 flex items-center gap-2 bg-overlay px-2 py-1.5">
                    <span className="text-muted">{provider.icon}</span>
                    <span className="text-2xs font-medium text-secondary">{provider.label}</span>
                    <span className={cx(
                      "ml-1 inline-flex items-center gap-1 text-2xs",
                      connected ? "text-success" : "text-faint",
                    )}>
                      <span className={cx("size-1.5 rounded-full", connected ? "bg-success" : "bg-faint")} />
                      {connected ? "Connected" : "Not connected"}
                    </span>
                    <button
                      type="button"
                      aria-label={`Refresh ${provider.label} catalog`}
                      title={`Refresh ${provider.label} catalog`}
                      disabled={loading}
                      onClick={() => onRefresh(provider.id)}
                      className="ml-auto rounded p-1 text-faint transition-colors hover:bg-hover hover:text-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent disabled:opacity-40"
                    >
                      <RefreshCw className={cx("size-icon-xs", loading && "animate-spin")} />
                    </button>
                  </div>
                  {provider.models.length > 0 ? provider.models.map((model) => {
                    const canonical = `${model.provider}/${model.id.replace(`${model.provider}/`, "")}`;
                    const selected = canonical === `${models.provider_id}/${currentModelId}`;
                    const known = model.metadata.capabilities;
                    const badges = [
                      known.tool_calling === "supported" ? { label: "Tools", icon: <Wrench className="size-icon-xs" /> } : null,
                      known.reasoning === "supported" ? { label: "Reasoning", icon: <Sparkles className="size-icon-xs" /> } : null,
                      known.vision === "supported" ? { label: "Vision", icon: <Eye className="size-icon-xs" /> } : null,
                    ].filter((badge) => badge !== null) as { label: string; icon: ReactNode }[];
                    return (
                      <button
                        key={canonical}
                        type="button"
                        data-model-option="true"
                        role="option"
                        aria-selected={selected}
                        title={`${model.display_name} · ${canonical}`}
                        onClick={() => { onSelect(model.provider, model.id); close(); }}
                        className={cx(
                          "flex w-full items-center gap-2 rounded-md px-2 py-2 text-left transition-colors",
                          selected ? "bg-accent/10 text-primary" : "text-secondary hover:bg-hover hover:text-primary",
                          "focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent",
                        )}
                      >
                        <span className="min-w-0 flex-1">
                          <span className="flex min-w-0 items-center gap-2">
                            <span className="truncate text-xs font-medium" title={model.display_name}>{model.display_name}</span>
                            {model.capabilities.context_window ? (
                              <span className="shrink-0 font-mono text-2xs text-faint">{compactTokens(model.capabilities.context_window)} ctx</span>
                            ) : null}
                            {selected ? <Check className="size-icon-sm shrink-0 text-accent" /> : null}
                          </span>
                          <span className="mt-1 flex items-center gap-1.5 text-2xs text-faint">
                            {model.id !== model.display_name ? <span className="truncate font-mono" title={model.id}>{model.id}</span> : null}
                            {badges.map((badge) => (
                              <span key={badge.label} className="inline-flex shrink-0 items-center gap-1 rounded-sm border border-line px-1 py-0.5">
                                {badge.icon}{badge.label}
                              </span>
                            ))}
                          </span>
                        </span>
                      </button>
                    );
                  }) : (
                    <div className="flex items-center gap-2 px-2 py-2 text-2xs text-faint">
                      <span className="min-w-0 flex-1">{loading ? "Loading catalog…" : "No discovered models"}</span>
                      {!connected ? (
                        <button
                          type="button"
                          data-model-option="true"
                          onClick={() => { onConnect(provider.id); close(); }}
                          className="shrink-0 rounded px-1.5 py-1 text-accent hover:bg-hover focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
                        >
                          <PlugZap className="mr-1 inline size-icon-xs" />Connect
                        </button>
                      ) : null}
                    </div>
                  )}
                  {provider.models.length > 0 && !connected ? (
                    <button
                      type="button"
                      data-model-option="true"
                      onClick={() => { onConnect(provider.id); close(); }}
                      className="ml-2 inline-flex items-center gap-1 rounded px-2 py-1 text-2xs text-faint hover:bg-hover hover:text-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
                    >
                      <PlugZap className="size-icon-xs" /> Connect provider
                    </button>
                  ) : null}
                </section>
              );
            })}
            {catalog.length === 0 && !loading ? (
              <p className="px-3 py-5 text-center text-xs text-faint">No model catalogs yet. Refresh a provider or connect an account.</p>
            ) : null}
            {providerRows.length === 0 ? <p className="px-3 py-5 text-center text-xs text-faint">No matching models.</p> : null}
          </div>
          {selectedLevels.length > 0 ? (
            <div className="flex items-center gap-3 border-t border-line px-3 py-2.5">
              <span className="text-2xs text-muted">Reasoning effort</span>
              <select
                value={effort}
                aria-label="Reasoning effort"
                onChange={(event) => onSelectReasoning(event.target.value)}
                className="ml-auto rounded-md border border-line-strong bg-app px-2 py-1 text-2xs text-secondary outline-none focus:border-accent"
              >
                <option value="off">Off</option>
                {selectedLevels.map((level) => (
                  <option key={level} value={level}>{effortLabel(level)}</option>
                ))}
              </select>
            </div>
          ) : null}
        </div>
      )}
    />
  );
}

function providerLabel(providerId: string): string {
  return {
    openai: "OpenAI",
    anthropic: "Anthropic",
    gemini: "Google Gemini",
    "opencode-zen": "OpenCode Zen",
    "opencode-go": "OpenCode Go",
  }[providerId] ?? providerId;
}

function compactTokens(tokens: number): string {
  return tokens >= 1_000_000 ? `${(tokens / 1_000_000).toFixed(tokens % 1_000_000 ? 1 : 0)}m` : `${Math.round(tokens / 1_000)}k`;
}

function effortLabel(value: string): string {
  return value.replaceAll("_", " ").replace(/\b\w/g, (letter) => letter.toUpperCase());
}

/** Execution mode selector, offering only the modes the runtime advertises. */
export function ModeSelector({
  permissions,
  onSelect,
  disabled,
  disabledReason,
  pending,
}: {
  permissions: PermissionSettings | null;
  onSelect: (mode: string) => void;
  disabled?: boolean;
  disabledReason?: string;
  pending?: string | null;
}) {
  const modes = permissions?.available_modes ?? [];
  const current = permissions?.mode ?? "";

  if (!permissions || modes.length === 0 || disabled) {
    return (
      <SelectorShell
        label="Execution mode"
        icon={<ShieldCheck className="size-icon-sm" />}
        disabled
        disabledReason={disabledReason}
      >
        <span className="truncate">
          {pending ? "Applying…" : permissions ? modeLabel(current) : "No mode"}
        </span>
      </SelectorShell>
    );
  }

  const items: DropdownItem[] = modes.map((mode) => ({
    id: mode,
    label: modeLabel(mode),
    detail: mode === current ? "active" : undefined,
    icon: mode === current ? <Check className="size-icon-sm" /> : undefined,
    onSelect: () => onSelect(mode),
  }));

  return (
    <Dropdown
      label={`Execution mode: ${modeLabel(current)}`}
      items={items}
      placement="top-start"
      panelClassName="w-72"
      trigger={
        <SelectorShell label="Execution mode" icon={<ShieldCheck className="size-icon-sm" />}>
          <span className="truncate">{pending ? "Applying…" : modeLabel(current)}</span>
        </SelectorShell>
      }
    />
  );
}

/**
 * Attachment control.
 *
 * The runtime has no upload or file-attach operation, so this is rendered
 * explicitly disabled and labelled as unavailable rather than as a button that
 * quietly does nothing.
 */
export function AttachButton() {
  return (
    <Tooltip label="Attaching files is not available in v0">
      <button
        type="button"
        disabled
        aria-label="Attach files (not available in v0)"
        className="flex size-control-sm cursor-not-allowed items-center justify-center rounded-md border border-line text-faint"
      >
        <Paperclip className="size-icon-sm" />
      </button>
    </Tooltip>
  );
}

/** Compact provider badge, used where there is no room for the full selector. */
export function ModelBadge({ models }: { models: ModelSettings | null }) {
  if (!models) return null;
  return (
    <Badge tone="neutral" indicator>
      <Cpu className="size-icon-xs" />
      {models.provider}
    </Badge>
  );
}
