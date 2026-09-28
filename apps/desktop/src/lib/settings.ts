/**
 * Types and helpers for the v0 settings surfaces.
 *
 * Credentials never appear in any of these types. The runtime reports only
 * whether a credential is available and which environment variable holds it, so
 * there is no field here that could carry a secret.
 */

export interface CredentialStatus {
  available: boolean;
  source: "environment" | "keychain" | "none" | "unavailable";
  /** Name of the environment variable holding the credential. */
  env_var: string;
}

export interface ProviderCredentialStatus {
  provider_id: string;
  provider: string;
  credential: CredentialStatus;
}

export interface ModelCapabilities {
  text_input: boolean;
  image_input: boolean;
  streaming: boolean;
  tool_calling: boolean;
  parallel_tool_calls: boolean;
  vision: boolean;
  reasoning: boolean;
  configurable_reasoning_effort: boolean;
  context_window: number | null;
  max_output_tokens: number | null;
  structured_output: boolean;
}

export type CapabilityKnowledge = "unknown" | "supported" | "unsupported";

export interface ModelDescriptor {
  provider: string;
  id: string;
  display_name: string;
  capabilities: ModelCapabilities;
  metadata: {
    source: "unknown" | "discovered" | "cached" | "manually_configured";
    stale: boolean;
    refreshed_at_unix: number | null;
    reasoning_levels: string[] | null;
    capabilities: {
      text_input: CapabilityKnowledge;
      vision: CapabilityKnowledge;
      streaming: CapabilityKnowledge;
      tool_calling: CapabilityKnowledge;
      parallel_tool_calls: CapabilityKnowledge;
      reasoning: CapabilityKnowledge;
      configurable_reasoning_effort: CapabilityKnowledge;
      structured_output: CapabilityKnowledge;
    };
    pricing: { input_usd_per_million_tokens: string | null; output_usd_per_million_tokens: string | null } | null;
  };
}

export interface ModelCatalog {
  models: ModelDescriptor[];
  defaults: Record<string, string>;
}

export interface ModelSettings {
  provider: string;
  provider_id: string;
  model: string;
  base_url: string;
  api_key_env: string;
  capabilities: ModelCapabilities;
  credential: CredentialStatus;
  available_models: string[];
  reasoning_levels: string[];
  reasoning_effort: string | null;
  configured: boolean;
}

export interface CatalogRefreshReport {
  providers: { provider_id: string; model_count: number; error: string | null }[];
  available_model_count: number;
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
  credentials: ProviderCredentialStatus[];
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
  reasoning_effort?: string;
  preference_scope?: "none" | "user" | "project" | "session";
  session_id?: string;
  record_session_event?: boolean;
}

export interface UpdatePermissionsRequest {
  mode: string;
}

export interface ConnectionTestResult {
  ok: boolean;
  message: string;
  skipped: boolean;
}

export interface CredentialActionResult {
  provider: ProviderCredentialStatus;
  providers: ProviderCredentialStatus[];
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
  if (!credential.available && credential.source !== "unavailable") {
    return `Not connected · set ${credential.env_var} or connect below`;
  }
  switch (credential.source) {
    case "environment":
      return `Connected from ${credential.env_var} · managed by the environment`;
    case "keychain":
      return "Connected · stored in the OS credential store";
    case "unavailable":
      return "OS credential store unavailable";
    default:
      return `Not connected · set ${credential.env_var} or connect below`;
  }
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
