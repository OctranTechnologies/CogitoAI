import type { ReactNode } from "react";
import { Bot, Check, ChevronDown, Cpu, Paperclip, ShieldCheck } from "lucide-react";
import type { ModelSettings, PermissionSettings } from "../lib/settings";
import { modeLabel } from "../lib/composer";
import { Badge, Dropdown, Tooltip, cx, type DropdownItem } from "./ui";

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
 * Offers only the models the runtime advertises, and shows provider and model.
 * There is no reasoning-effort setting in v0, so the runtime's reported
 * capabilities are announced to assistive technology instead of inventing an
 * effort control.
 */
export function ModelSelector({
  models,
  onSelect,
  disabled,
  disabledReason,
}: {
  models: ModelSettings | null;
  onSelect: (model: string) => void;
  disabled?: boolean;
  disabledReason?: string;
}) {
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

  const items: DropdownItem[] = models.available_models.length
    ? models.available_models.map((name) => ({
        id: name,
        label: name,
        detail: name === models.model ? "active" : undefined,
        icon: name === models.model ? <Check className="size-icon-sm" /> : undefined,
        onSelect: () => onSelect(name),
      }))
    : [
        {
          id: models.model,
          label: models.model,
          detail: "only model the runtime advertises",
          disabled: true,
          onSelect: () => {},
        },
      ];

  const capabilities = [
    models.capabilities.tool_calling ? "tool calling" : null,
    models.capabilities.reasoning ? "reasoning" : null,
    models.capabilities.vision ? "vision" : null,
  ].filter((value): value is string => value !== null);

  return (
    <Dropdown
      label={`Model: ${models.provider} / ${models.model}`}
      items={items}
      placement="top-start"
      panelClassName="w-64"
      trigger={
        <SelectorShell label="Model" icon={<Bot className="size-icon-sm" />}>
          <span className="truncate">
            <span className="text-faint">{models.provider}</span>
            <span className="opacity-60"> / </span>
            {models.model}
          </span>
          {capabilities.length > 0 ? (
            <span className="sr-only">Supports {capabilities.join(", ")}.</span>
          ) : null}
        </SelectorShell>
      }
    />
  );
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
