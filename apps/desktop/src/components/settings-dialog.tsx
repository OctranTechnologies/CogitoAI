import { useEffect, useState } from "react";
import {
  CheckCircle2,
  CircleAlert,
  CircleDot,
  FolderOpen,
  LoaderCircle,
  KeyRound,
  RefreshCw,
  Save,
  Settings2,
  ShieldCheck,
  Terminal,
  TestTube2,
  X,
} from "lucide-react";
import { useDesktopStore } from "../store";
import {
  SETTINGS_SCREENS,
  describeCapabilities,
  describeCredential,
  formatCommand,
  type SettingsScreen,
} from "../lib/settings";
import { Badge, Button, IconButton, Modal, toneText, type Tone } from "./ui";
import { cx } from "./ui/cx";

const SCREEN_ICONS: Record<SettingsScreen, typeof Settings2> = {
  models: Settings2,
  runtime: Terminal,
  permissions: ShieldCheck,
  project: FolderOpen,
  verification: TestTube2,
};

/** Read-only settings dialog covering the five v0 configuration screens. */
export function SettingsDialog({
  open,
  onClose,
  initialScreen = "models",
}: {
  open: boolean;
  onClose: () => void;
  /** Lets the navigation rail open the dialog on a chosen screen. */
  initialScreen?: SettingsScreen;
}) {
  const settings = useDesktopStore((state) => state.settings);
  const isLoading = useDesktopStore((state) => state.isLoadingSettings);
  const error = useDesktopStore((state) => state.settingsError);
  const refreshSettings = useDesktopStore((state) => state.refreshSettings);
  const clearSettingsError = useDesktopStore((state) => state.clearSettingsError);
  const [screen, setScreen] = useState<SettingsScreen>(initialScreen);

  // Follow the requested screen each time it is opened, so the rail entry that
  // was pressed is the screen that appears.
  useEffect(() => {
    if (open) setScreen(initialScreen);
  }, [open, initialScreen]);

  // Reload on open so a change made elsewhere in the runtime is picked up.
  useEffect(() => {
    if (open) void refreshSettings();
  }, [open, refreshSettings]);

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="Settings"
      description="Read from and applied through the runtime. The desktop never edits configuration itself."
      width="max-w-5xl"
    >
      <div className="flex h-[min(680px,80vh)] min-h-0">
        <nav className="w-52 shrink-0 border-r border-line p-2" aria-label="Settings sections">
          {SETTINGS_SCREENS.map((entry) => {
            const Icon = SCREEN_ICONS[entry.id];
            const active = screen === entry.id;
            return (
              <button
                key={entry.id}
                onClick={() => {
                  setScreen(entry.id);
                  clearSettingsError();
                }}
                aria-current={active ? "page" : undefined}
                className={cx(
                  "mb-0.5 flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left text-sm",
                  "transition-colors duration-fast",
                  active ? "bg-active text-primary" : "text-muted hover:bg-hover hover:text-primary",
                )}
              >
                <Icon className="size-icon-md shrink-0" />
                {entry.label}
              </button>
            );
          })}
        </nav>

        <div className="scroll-area min-w-0 flex-1 p-5">
          {error ? (
            <div
              role="alert"
              className="mb-4 flex items-start gap-2 rounded-lg border border-error/30 bg-error/5 p-3 text-xs leading-4 text-error"
            >
              <CircleAlert className="mt-0.5 size-icon-md shrink-0" />
              <span className="flex-1">{error}</span>
              <IconButton label="Dismiss" size="sm" onClick={clearSettingsError}>
                <X className="size-icon-sm" />
              </IconButton>
            </div>
          ) : null}

          {!settings ? (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-faint">
              {isLoading ? <LoaderCircle className="size-icon-md animate-spin" /> : null}
              {isLoading
                ? "Loading settings…"
                : "Settings are unavailable until a runtime is connected."}
            </div>
          ) : (
            <>
              {screen === "models" ? <ModelsScreen /> : null}
              {screen === "runtime" ? <RuntimeScreen /> : null}
              {screen === "permissions" ? <PermissionsScreen /> : null}
              {screen === "project" ? <ProjectScreen /> : null}
              {screen === "verification" ? <VerificationScreen /> : null}
            </>
          )}
        </div>
      </div>
      <SettingsFooter
        isLoading={isLoading}
        onRefresh={() => void refreshSettings()}
        onClose={onClose}
      />
    </Modal>
  );
}

function SettingsFooter({
  isLoading,
  onRefresh,
  onClose,
}: {
  isLoading: boolean;
  onRefresh: () => void;
  onClose: () => void;
}) {
  return (
    <div className="flex items-center justify-between border-t border-line px-5 py-2.5">
      <span className="text-2xs text-faint">The runtime owns execution and credentials.</span>
      <div className="flex items-center gap-2">
        <Button
          size="sm"
          onClick={onRefresh}
          disabled={isLoading}
          icon={
            <RefreshCw className={cx("size-icon-sm", isLoading && "animate-spin")} />
          }
        >
          Reload
        </Button>
        <Button size="sm" variant="primary" onClick={onClose}>
          Done
        </Button>
      </div>
    </div>
  );
}

function Screen({
  title,
  description,
  children,
}: {
  title: string;
  description: string;
  children: React.ReactNode;
}) {
  return (
    <section>
      <h3 className="text-md font-semibold text-primary">{title}</h3>
      <p className="mt-1 text-xs leading-4 text-muted">{description}</p>
      <div className="mt-4 space-y-3">{children}</div>
    </section>
  );
}

/** Label/value row. The two-column grid keeps long values aligned. */
function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="grid grid-cols-[10rem_1fr] items-baseline gap-3 rounded-md border border-line bg-sunken px-3 py-2">
      <span className="text-xs text-faint">{label}</span>
      <span
        className={cx(
          "break-words text-sm text-secondary",
          mono && "font-mono text-xs",
        )}
      >
        {value}
      </span>
    </div>
  );
}

/** Text input paired with a label, matching the Field grid. */
function LabelledInput({
  label,
  value,
  onChange,
  ariaLabel,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  ariaLabel: string;
}) {
  return (
    <label className="grid grid-cols-[10rem_1fr] items-center gap-3">
      <span className="text-xs text-faint">{label}</span>
      <input
        className="rounded-md border border-line-strong bg-app px-2.5 py-1.5 font-mono text-sm text-primary outline-none transition-colors duration-fast focus:border-accent"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        aria-label={ariaLabel}
      />
    </label>
  );
}

function ModelsScreen() {
  const settings = useDesktopStore((state) => state.settings)!;
  const updateModel = useDesktopStore((state) => state.updateModel);
  const testModelConnection = useDesktopStore((state) => state.testModelConnection);
  const modelTest = useDesktopStore((state) => state.modelTest);
  const [model, setModel] = useState(settings.models.model);
  const [baseUrl, setBaseUrl] = useState(settings.models.base_url);
  const [apiKeyEnv, setApiKeyEnv] = useState(settings.models.api_key_env);
  const [saved, setSaved] = useState(false);

  // Re-seed the form when the runtime reports a different selection.
  useEffect(() => {
    setModel(settings.models.model);
    setBaseUrl(settings.models.base_url);
    setApiKeyEnv(settings.models.api_key_env);
  }, [settings.models.model, settings.models.base_url, settings.models.api_key_env]);

  async function save() {
    const changed =
      model !== settings.models.model ||
      baseUrl !== settings.models.base_url ||
      apiKeyEnv !== settings.models.api_key_env;
    if (!changed) return;
    const ok = await updateModel({ model, base_url: baseUrl, api_key_env: apiKeyEnv });
    setSaved(ok);
  }

  const testTone: Tone = modelTest?.ok ? "success" : modelTest?.skipped ? "warning" : "error";

  return (
    <Screen
      title="Models"
      description="Choose the provider and model the agent runs on. Changes apply to the next run."
    >
      <Field label="Provider" value={settings.models.provider} />
      <Field label="Model" value={settings.models.model} mono />
      <Field label="Capabilities" value={describeCapabilities(settings.models.capabilities)} />
      <Field label="Base URL" value={settings.models.base_url} mono />

      <div className="rounded-lg border border-line bg-sunken p-3">
        <div className="flex items-center gap-2">
          <KeyRound className="size-icon-sm text-faint" />
          <span className="text-xs font-medium text-secondary">Credential</span>
        </div>
        <p className="mt-1.5 text-xs leading-4 text-muted">
          {describeCredential(settings.models.credential)}
        </p>
        <p className="mt-1 text-2xs leading-4 text-faint">
          Secrets are read from the runtime environment and are never sent to this app, so they cannot
          be displayed or edited here.
        </p>
      </div>

      <div className="grid grid-cols-1 gap-2">
        <LabelledInput
          label="Model name"
          value={model}
          onChange={(value) => {
            setModel(value);
            setSaved(false);
          }}
          ariaLabel="Model name"
        />
        {settings.models.available_models.length > 0 ? (
          <div className="grid grid-cols-[10rem_1fr] items-start gap-3">
            <span className="text-xs text-faint">Known models</span>
            <div className="flex flex-wrap gap-1">
              {settings.models.available_models.map((name) => (
                <button
                  key={name}
                  onClick={() => {
                    setModel(name);
                    setSaved(false);
                  }}
                  className={cx(
                    "rounded-sm px-1.5 py-0.5 font-mono text-2xs",
                    "transition-colors duration-fast",
                    name === model
                      ? "bg-accent/15 text-accent"
                      : "bg-elevated text-muted hover:text-primary",
                  )}
                >
                  {name}
                </button>
              ))}
            </div>
          </div>
        ) : null}
        <LabelledInput
          label="Base URL"
          value={baseUrl}
          onChange={(value) => {
            setBaseUrl(value);
            setSaved(false);
          }}
          ariaLabel="Base URL"
        />
        <LabelledInput
          label="Key variable"
          value={apiKeyEnv}
          onChange={(value) => {
            setApiKeyEnv(value);
            setSaved(false);
          }}
          ariaLabel="API key environment variable name"
        />
      </div>

      <div className="flex items-center gap-2 pt-1">
        <Button
          variant="primary"
          size="sm"
          onClick={() => void save()}
          icon={<Save className="size-icon-sm" />}
        >
          Apply model
        </Button>
        <Button
          size="sm"
          onClick={() => void testModelConnection()}
          icon={<CircleDot className="size-icon-sm" />}
        >
          Test connection
        </Button>
        {saved ? (
          <Badge tone="success">
            <CheckCircle2 className="size-icon-xs" /> Applied
          </Badge>
        ) : null}
      </div>

      {modelTest ? (
        <div
          className={cx(
            "rounded-md border p-2.5 text-xs leading-4",
            testTone === "success"
              ? "border-success/30 bg-success/5"
              : testTone === "warning"
                ? "border-warning/30 bg-warning/5"
                : "border-error/30 bg-error/5",
            toneText(testTone),
          )}
        >
          {modelTest.message}
        </div>
      ) : null}
    </Screen>
  );
}

function RuntimeScreen() {
  const settings = useDesktopStore((state) => state.settings)!;
  return (
    <Screen title="Runtime" description="The local runtime this session is connected to.">
      <Field label="Version" value={settings.runtime.version} mono />
      <Field label="Sessions" value={settings.runtime.session_storage_path} mono />
      <Field label="Checkpoints" value={settings.runtime.checkpoint_storage_path} mono />
      <Field label="Log level" value={settings.runtime.log_level} mono />
      <Field label="Logs" value={settings.runtime.log_target} />
      <Field
        label="Providers"
        value={
          settings.runtime.provider_names.length > 0
            ? settings.runtime.provider_names.join(", ")
            : "none"
        }
      />
      <Field label="Credentials" value={`from ${settings.runtime.credential_source}`} />
    </Screen>
  );
}

function PermissionsScreen() {
  const settings = useDesktopStore((state) => state.settings)!;
  const updatePermissionMode = useDesktopStore((state) => state.updatePermissionMode);
  const [pending, setPending] = useState<string | null>(null);

  return (
    <Screen
      title="Permissions"
      description="The execution mode governs what the agent may do without asking. Human terminal sessions are separate and are not governed here."
    >
      <div className="rounded-lg border border-line bg-sunken p-3">
        <div className="flex items-center gap-2">
          <ShieldCheck className="size-icon-md text-accent" />
          <span className="text-sm font-medium text-primary">{settings.permissions.mode}</span>
        </div>
        <p className="mt-1.5 text-xs leading-4 text-muted">
          {settings.permissions.mode_description}
        </p>
      </div>

      <div className="flex flex-wrap gap-1.5">
        {settings.permissions.available_modes.map((mode) => {
          const active = mode === settings.permissions.mode;
          return (
            <button
              key={mode}
              onClick={async () => {
                if (active) return;
                setPending(mode);
                await updatePermissionMode(mode);
                setPending(null);
              }}
              disabled={pending !== null || active}
              className={cx(
                "rounded-md border px-2.5 py-1.5 font-mono text-xs",
                "transition-colors duration-fast disabled:opacity-50",
                active
                  ? "border-accent/50 bg-accent/10 text-accent"
                  : "border-line-strong bg-elevated text-secondary hover:border-line-stronger hover:text-primary",
              )}
            >
              {pending === mode ? (
                <LoaderCircle className="mr-1 inline size-icon-sm animate-spin" />
              ) : null}
              {mode}
            </button>
          );
        })}
      </div>

      <RuleList
        title="Default behaviour"
        hint="What happens when no configured rule matches."
        rules={settings.permissions.default_behavior.map((entry) => ({
          name: entry.operation,
          action: entry.effect,
          reason: "",
          tools: [],
          operations: [],
        }))}
      />
      <RuleList
        title="Built-in rules"
        hint="Always applied, in evaluation order."
        rules={settings.permissions.built_in_rules}
      />
      <RuleList
        title="Project rules"
        hint="Loaded from .agent/policy.toml. These override the mode default."
        rules={settings.permissions.configured_rules}
        emptyText="No project rules configured."
      />
    </Screen>
  );
}

function RuleList({
  title,
  hint,
  rules,
  emptyText,
}: {
  title: string;
  hint: string;
  rules: {
    name: string;
    action: string;
    reason: string;
    tools: string[];
    operations: string[];
  }[];
  emptyText?: string;
}) {
  return (
    <div>
      <p className="text-xs font-medium text-secondary">{title}</p>
      <p className="mt-0.5 text-2xs text-faint">{hint}</p>
      {rules.length === 0 ? (
        <p className="mt-2 rounded-md border border-dashed border-line p-2.5 text-xs text-faint">
          {emptyText ?? "None."}
        </p>
      ) : (
        <ul className="mt-2 space-y-1">
          {rules.map((rule, index) => (
            <li key={`${rule.name}-${index}`} className="rounded-md border border-line bg-sunken px-3 py-2">
              <div className="flex items-center justify-between gap-2">
                <span className="font-mono text-xs text-secondary">{rule.name}</span>
                <Badge tone={actionTone(rule.action)}>{rule.action}</Badge>
              </div>
              {rule.reason ? (
                <p className="mt-1 text-xs leading-4 text-faint">{rule.reason}</p>
              ) : null}
              {rule.tools.length > 0 || rule.operations.length > 0 ? (
                <p className="mt-1 font-mono text-2xs text-faint">
                  {[...rule.tools, ...rule.operations].join(", ")}
                </p>
              ) : null}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Maps a policy action onto a tone so allow/ask/deny read consistently. */
function actionTone(action: string): Tone {
  const value = action.toLowerCase();
  if (value.startsWith("allow")) return "success";
  if (value.startsWith("deny")) return "error";
  return "warning";
}

function ProjectScreen() {
  const settings = useDesktopStore((state) => state.settings)!;
  return (
    <Screen title="Project" description="What the runtime detected for the open workspace.">
      <Field label="Workspace" value={settings.project.workspace_path} mono />
      <Field
        label="Repository"
        value={settings.project.repository_root ?? "not a Git repository"}
        mono
      />
      <ChipRow label="Ecosystem" items={settings.project.languages} empty="No languages detected." />
      <ChipRow label="Manifests" items={settings.project.manifests} empty="No manifests detected." />
      <ChipRow
        label="Instructions"
        items={settings.project.instruction_files}
        empty="No instruction files detected."
      />
      <Field
        label="Package manager"
        value={settings.project.package_manager ?? "none detected"}
      />
      <Field label="Monorepo" value={settings.project.monorepo ? "yes" : "no"} />
    </Screen>
  );
}

function ChipRow({ label, items, empty }: { label: string; items: string[]; empty: string }) {
  return (
    <div className="rounded-md border border-line bg-sunken px-3 py-2">
      <span className="text-xs text-faint">{label}</span>
      {items.length === 0 ? (
        <p className="mt-1 text-xs text-faint">{empty}</p>
      ) : (
        <div className="mt-1.5 flex flex-wrap gap-1">
          {items.map((item) => (
            <span
              key={item}
              className="max-w-full truncate rounded-sm bg-elevated px-1.5 py-0.5 font-mono text-2xs text-secondary"
              title={item}
            >
              {item}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

function VerificationScreen() {
  const settings = useDesktopStore((state) => state.settings)!;
  return (
    <Screen
      title="Verification"
      description="Commands the runtime runs to verify agent changes."
    >
      {settings.verification.commands.length === 0 ? (
        <p className="rounded-md border border-dashed border-line p-3 text-xs leading-4 text-faint">
          No verification commands were detected for this project.
        </p>
      ) : (
        <ul className="space-y-1">
          {settings.verification.commands.map((command) => (
            <li key={command.category} className="rounded-md border border-line bg-sunken px-3 py-2">
              <div className="flex items-center justify-between gap-2">
                <span className="text-xs font-medium text-secondary">{command.category}</span>
                <span className="text-2xs text-faint">
                  {command.is_override ? "project override" : "detected"}
                </span>
              </div>
              <p className="mt-1 break-words font-mono text-xs text-secondary">
                {formatCommand(command)}
              </p>
            </li>
          ))}
        </ul>
      )}
      <Field
        label="Source"
        value={settings.verification.source ?? "detected from the project layout"}
        mono
      />
    </Screen>
  );
}
