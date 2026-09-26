/**
 * Types and helpers for the v0 settings surfaces.
 *
 * Credentials never appear in any of these types. The runtime reports only
 * whether a credential is available and which environment variable holds it, so
 * there is no field here that could carry a secret.
 */

export interface CredentialStatus {
  available: boolean;
  source: "environment";
  /** Name of the environment variable holding the credential. */
  env_var: string;
}

export interface ModelCapabilities {
  streaming: boolean;
  tool_calling: boolean;
  vision: boolean;
  reasoning: boolean;
  context_window: number | null;
}

export interface ModelSettings {
  provider: string;
  model: string;
  base_url: string;
  api_key_env: string;
  capabilities: ModelCapabilities;
  credential: CredentialStatus;
  available_models: string[];
  configured: boolean;
}

export interface RuleSummary {
  name: string;
  action: string;
  reason: string;
  tools: string[];
  operations: string[];
}

export interface OperationSummary {
  operation: string;
  effect: string;
}

export interface PermissionSettings {
  mode: string;
  mode_description: string;
  available_modes: string[];
  built_in_rules: RuleSummary[];
  configured_rules: RuleSummary[];
  default_behavior: OperationSummary[];
}

export interface ProjectSettings {
  workspace_path: string;
  repository_root: string | null;
  is_git_repository: boolean;
  languages: string[];
  manifests: string[];
  package_manager: string | null;
  instruction_files: string[];
  monorepo: boolean;
}

export interface VerificationCommand {
  category: string;
  program: string;
  args: string[];
  is_override: boolean;
}

export interface VerificationSettings {
  commands: VerificationCommand[];
  source: string | null;
  has_project_overrides: boolean;
}

export interface RuntimeSettings {
  version: string;
  session_storage_path: string;
  checkpoint_storage_path: string;
  log_level: string;
  log_target: string;
  provider_names: string[];
  credential_source: string;
}

export interface SettingsSnapshot {
  models: ModelSettings;
  permissions: PermissionSettings;
  project: ProjectSettings;
  verification: VerificationSettings;
  runtime: RuntimeSettings;
}

export interface UpdateModelRequest {
  provider?: string;
  model?: string;
  base_url?: string;
  api_key_env?: string;
}

export interface UpdatePermissionsRequest {
  mode: string;
}

export interface ConnectionTestResult {
  ok: boolean;
  message: string;
  skipped: boolean;
}

export type SettingsScreen = "models" | "runtime" | "permissions" | "project" | "verification";

export const SETTINGS_SCREENS: { id: SettingsScreen; label: string }[] = [
  { id: "models", label: "Models" },
  { id: "runtime", label: "Runtime" },
  { id: "permissions", label: "Permissions" },
  { id: "project", label: "Project" },
  { id: "verification", label: "Verification" },
];

export function isSettingsSnapshot(value: unknown): value is SettingsSnapshot {
  if (!value || typeof value !== "object") return false;
  const snapshot = value as Partial<SettingsSnapshot>;
  return (
    typeof snapshot.models === "object" &&
    snapshot.models !== null &&
    typeof snapshot.permissions === "object" &&
    snapshot.permissions !== null &&
    typeof snapshot.project === "object" &&
    snapshot.project !== null &&
    typeof snapshot.verification === "object" &&
    snapshot.verification !== null &&
    typeof snapshot.runtime === "object" &&
    snapshot.runtime !== null
  );
}

/**
 * Describes a credential without revealing it.
 *
 * The UI must never receive a secret, so this is derived from presence and the
 * variable name alone.
 */
export function describeCredential(credential: CredentialStatus): string {
  return credential.available
    ? `Configured from ${credential.env_var}`
    : `Not set — set ${credential.env_var} in the runtime environment`;
}

export function describeCapabilities(capabilities: ModelCapabilities): string {
  const enabled = [
    capabilities.streaming ? "streaming" : null,
    capabilities.tool_calling ? "tool calling" : null,
    capabilities.vision ? "vision" : null,
    capabilities.reasoning ? "reasoning" : null,
  ].filter(Boolean);
  const context = capabilities.context_window
    ? `${capabilities.context_window.toLocaleString()} token context`
    : "unknown context window";
  return enabled.length > 0 ? `${enabled.join(", ")} · ${context}` : context;
}

export function formatCommand(command: VerificationCommand): string {
  return [command.program, ...command.args].join(" ");
}
