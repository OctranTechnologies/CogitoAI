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

const SCREEN_ICONS: Record<SettingsScreen, typeof Settings2> = {
  models: Settings2,
  runtime: Terminal,
  permissions: ShieldCheck,
  project: FolderOpen,
  verification: TestTube2,
};

/** Read-only settings dialog covering the five v0 configuration screens. */
export function SettingsDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const settings = useDesktopStore((state) => state.settings);
  const isLoading = useDesktopStore((state) => state.isLoadingSettings);
  const error = useDesktopStore((state) => state.settingsError);
  const refreshSettings = useDesktopStore((state) => state.refreshSettings);
  const clearSettingsError = useDesktopStore((state) => state.clearSettingsError);
  const [screen, setScreen] = useState<SettingsScreen>("models");

  // Reload on open so a change made elsewhere in the runtime is picked up.
  useEffect(() => {
    if (open) void refreshSettings();
  }, [open, refreshSettings]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-20 flex items-center justify-center bg-ink-950/80 p-6">
      <div
        className="flex h-[min(720px,90vh)] w-[min(1080px,96vw)] flex-col overflow-hidden rounded-xl border border-ink-800 bg-ink-900 shadow-panel"
        role="dialog"
        aria-modal="true"
        aria-label="Settings"
      >
        <div className="flex h-12 shrink-0 items-center justify-between border-b border-ink-800 px-4">
          <div className="flex items-center gap-2">
            <Settings2 size={15} className="text-ink-400" />
            <h2 className="text-sm font-semibold text-ink-100">Settings</h2>
          </div>
          <div className="flex items-center gap-2">
            <button
              className="icon-button h-7 w-7"
              onClick={() => void refreshSettings()}
              disabled={isLoading}
              aria-label="Reload settings"
              title="Reload settings from the runtime"
            >
              <RefreshCw size={13} className={isLoading ? "animate-spin" : ""} />
            </button>
            <button className="icon-button h-7 w-7" onClick={onClose} aria-label="Close settings">
              <X size={14} />
            </button>
          </div>
        </div>

        <div className="flex min-h-0 flex-1">
          <nav className="w-52 shrink-0 border-r border-ink-800 p-2" aria-label="Settings sections">
            {SETTINGS_SCREENS.map((entry) => {
              const Icon = SCREEN_ICONS[entry.id];
              return (
                <button
                  key={entry.id}
                  onClick={() => {
                    setScreen(entry.id);
                    clearSettingsError();
                  }}
                  aria-current={screen === entry.id ? "page" : undefined}
                  className={`mb-0.5 flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left text-[12px] transition-colors ${
                    screen === entry.id
                      ? "bg-ink-800 text-ink-100"
                      : "text-ink-400 hover:bg-ink-850 hover:text-ink-200"
                  }`}
                >
                  <Icon size={14} className="shrink-0" />
                  {entry.label}
                </button>
              );
            })}
          </nav>

          <div className="min-w-0 flex-1 overflow-y-auto p-5">
            {error ? (
              <div
                role="alert"
                className="mb-4 flex items-start gap-2 rounded-lg border border-danger/30 bg-danger/5 p-3 text-[11px] leading-4 text-danger"
              >
                <CircleAlert size={14} className="mt-0.5 shrink-0" />
                <span className="flex-1">{error}</span>
                <button onClick={clearSettingsError} aria-label="Dismiss">
                  <X size={12} />
                </button>
              </div>
            ) : null}

            {!settings ? (
              <div className="flex h-full items-center justify-center gap-2 text-[12px] text-ink-500">
                {isLoading ? <LoaderCircle size={14} className="animate-spin" /> : null}
                {isLoading ? "Loading settings…" : "Settings are unavailable until a runtime is connected."}
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
      </div>
    </div>
  );
}

function Screen({ title, description, children }: { title: string; description: string; children: React.ReactNode }) {
  return (
    <section>
      <h3 className="text-sm font-semibold text-ink-100">{title}</h3>
      <p className="mt-1 text-[11px] leading-4 text-ink-500">{description}</p>
      <div className="mt-4 space-y-3">{children}</div>
    </section>
  );
}

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="grid grid-cols-[10rem_1fr] items-baseline gap-3 rounded-md border border-ink-800 bg-ink-950/40 px-3 py-2">
      <span className="text-[11px] text-ink-500">{label}</span>
      <span className={`break-words text-[12px] text-ink-200 ${mono ? "font-mono" : ""}`}>{value}</span>
    </div>
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

  return (
    <Screen
      title="Models"
      description="Choose the provider and model the agent runs on. Changes apply to the next run."
    >
      <Field label="Provider" value={settings.models.provider} />
      <Field label="Model" value={settings.models.model} mono />
      <Field label="Capabilities" value={describeCapabilities(settings.models.capabilities)} />
      <Field label="Base URL" value={settings.models.base_url} mono />

      <div className="rounded-lg border border-ink-800 bg-ink-950/40 p-3">
        <div className="flex items-center gap-2">
          <KeyRound size={13} className="text-ink-500" />
          <span className="text-[11px] font-medium text-ink-300">Credential</span>
        </div>
        <p className="mt-1.5 text-[11px] leading-4 text-ink-400">
          {describeCredential(settings.models.credential)}
        </p>
        <p className="mt-1 text-[10px] leading-4 text-ink-600">
          Secrets are read from the runtime environment and are never sent to this app, so they cannot be
          displayed or edited here.
        </p>
      </div>

      <div className="grid grid-cols-1 gap-2">
        <label className="grid grid-cols-[10rem_1fr] items-center gap-3">
          <span className="text-[11px] text-ink-500">Model name</span>
          <input
            className="rounded-md border border-ink-700 bg-ink-950 px-2.5 py-1.5 font-mono text-[12px] text-ink-100 outline-none focus:border-signal-500"
            value={model}
            onChange={(event) => {
              setModel(event.target.value);
              setSaved(false);
            }}
            aria-label="Model name"
          />
        </label>
        {settings.models.available_models.length > 0 ? (
          <div className="grid grid-cols-[10rem_1fr] items-start gap-3">
            <span className="text-[11px] text-ink-500">Known models</span>
            <div className="flex flex-wrap gap-1">
              {settings.models.available_models.map((name) => (
                <button
                  key={name}
                  onClick={() => {
                    setModel(name);
                    setSaved(false);
                  }}
                  className={`rounded px-1.5 py-0.5 font-mono text-[10px] ${
                    name === model
                      ? "bg-signal-500/20 text-signal-400"
                      : "bg-ink-850 text-ink-400 hover:text-ink-200"
                  }`}
                >
                  {name}
                </button>
              ))}
            </div>
          </div>
        ) : null}
        <label className="grid grid-cols-[10rem_1fr] items-center gap-3">
          <span className="text-[11px] text-ink-500">Base URL</span>
          <input
            className="rounded-md border border-ink-700 bg-ink-950 px-2.5 py-1.5 font-mono text-[12px] text-ink-100 outline-none focus:border-signal-500"
            value={baseUrl}
            onChange={(event) => {
              setBaseUrl(event.target.value);
              setSaved(false);
            }}
            aria-label="Base URL"
          />
        </label>
        <label className="grid grid-cols-[10rem_1fr] items-center gap-3">
          <span className="text-[11px] text-ink-500">Key variable</span>
          <input
            className="rounded-md border border-ink-700 bg-ink-950 px-2.5 py-1.5 font-mono text-[12px] text-ink-100 outline-none focus:border-signal-500"
            value={apiKeyEnv}
            onChange={(event) => {
              setApiKeyEnv(event.target.value);
              setSaved(false);
            }}
            aria-label="API key environment variable name"
          />
        </label>
      </div>

      <div className="flex items-center gap-2 pt-1">
        <button className="primary-button" onClick={() => void save()}>
          <Save size={13} /> Apply model
        </button>
        <button className="quiet-button" onClick={() => void testModelConnection()}>
          <CircleDot size={13} /> Test connection
        </button>
        {saved ? (
          <span className="flex items-center gap-1 text-[11px] text-success">
            <CheckCircle2 size={12} /> Applied
          </span>
        ) : null}
      </div>

      {modelTest ? (
        <div
          className={`rounded-md border p-2.5 text-[11px] leading-4 ${
            modelTest.ok
              ? "border-success/30 bg-success/5 text-success"
              : modelTest.skipped
                ? "border-warning/30 bg-warning/5 text-warning"
                : "border-danger/30 bg-danger/5 text-danger"
          }`}
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
        value={settings.runtime.provider_names.length > 0 ? settings.runtime.provider_names.join(", ") : "none"}
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
      <div className="rounded-lg border border-ink-800 bg-ink-950/40 p-3">
        <div className="flex items-center gap-2">
          <ShieldCheck size={14} className="text-signal-400" />
          <span className="text-[12px] font-medium text-ink-100">{settings.permissions.mode}</span>
        </div>
        <p className="mt-1.5 text-[11px] leading-4 text-ink-400">
          {settings.permissions.mode_description}
        </p>
      </div>

      <div className="flex flex-wrap gap-1.5">
        {settings.permissions.available_modes.map((mode) => (
          <button
            key={mode}
            onClick={async () => {
              if (mode === settings.permissions.mode) return;
              setPending(mode);
              await updatePermissionMode(mode);
              setPending(null);
            }}
            disabled={pending !== null || mode === settings.permissions.mode}
            className={`rounded-md border px-2.5 py-1.5 font-mono text-[11px] transition-colors disabled:opacity-50 ${
              mode === settings.permissions.mode
                ? "border-signal-500/50 bg-signal-500/15 text-signal-400"
                : "border-ink-700 bg-ink-850 text-ink-300 hover:border-ink-600 hover:text-ink-100"
            }`}
          >
            {pending === mode ? <LoaderCircle size={12} className="mr-1 inline animate-spin" /> : null}
            {mode}
          </button>
        ))}
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
  rules: { name: string; action: string; reason: string; tools: string[]; operations: string[] }[];
  emptyText?: string;
}) {
  return (
    <div>
      <p className="text-[11px] font-medium text-ink-300">{title}</p>
      <p className="mt-0.5 text-[10px] text-ink-600">{hint}</p>
      {rules.length === 0 ? (
        <p className="mt-2 rounded-md border border-dashed border-ink-800 p-2.5 text-[11px] text-ink-600">
          {emptyText ?? "None."}
        </p>
      ) : (
        <ul className="mt-2 space-y-1">
          {rules.map((rule, index) => (
            <li
              key={`${rule.name}-${index}`}
              className="rounded-md border border-ink-800 bg-ink-950/40 px-3 py-2"
            >
              <div className="flex items-center justify-between gap-2">
                <span className="font-mono text-[11px] text-ink-200">{rule.name}</span>
                <span
                  className={`text-[10px] ${
                    rule.action === "allow" || rule.action === "allowed"
                      ? "text-success"
                      : rule.action === "deny" || rule.action === "denied"
                        ? "text-danger"
                        : "text-warning"
                  }`}
                >
                  {rule.action}
                </span>
              </div>
              {rule.reason ? (
                <p className="mt-1 text-[11px] leading-4 text-ink-500">{rule.reason}</p>
              ) : null}
              {rule.tools.length > 0 || rule.operations.length > 0 ? (
                <p className="mt-1 font-mono text-[10px] text-ink-600">
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
    <div className="rounded-md border border-ink-800 bg-ink-950/40 px-3 py-2">
      <span className="text-[11px] text-ink-500">{label}</span>
      {items.length === 0 ? (
        <p className="mt-1 text-[11px] text-ink-600">{empty}</p>
      ) : (
        <div className="mt-1.5 flex flex-wrap gap-1">
          {items.map((item) => (
            <span
              key={item}
              className="max-w-full truncate rounded bg-ink-850 px-1.5 py-0.5 font-mono text-[10px] text-ink-300"
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
        <p className="rounded-md border border-dashed border-ink-800 p-3 text-[11px] leading-4 text-ink-600">
          No verification commands were detected for this project.
        </p>
      ) : (
        <ul className="space-y-1">
          {settings.verification.commands.map((command) => (
            <li
              key={command.category}
              className="rounded-md border border-ink-800 bg-ink-950/40 px-3 py-2"
            >
              <div className="flex items-center justify-between gap-2">
                <span className="text-[11px] font-medium text-ink-300">{command.category}</span>
                <span className="text-[10px] text-ink-600">
                  {command.is_override ? "project override" : "detected"}
                </span>
              </div>
              <p className="mt-1 break-words font-mono text-[11px] text-ink-200">
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
