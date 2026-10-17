export const DEFAULT_API_URL = "http://localhost:8080";

/** Subset of `vscode.WorkspaceConfiguration.inspect()` the resolver needs. */
export interface SettingInspection {
  defaultValue?: string;
  globalValue?: string;
  workspaceValue?: string;
  workspaceFolderValue?: string;
}

export interface ConnectionSettings {
  apiUrl: string;
  token: string;
}

type Env = Record<string, string | undefined>;

// Priority: a setting the user set explicitly > DARUMA_API_URL / DARUMA_TOKEN > built-in default.
// `config.get()` cannot be used for this: it returns the manifest default when nothing is set,
// which would always shadow the environment variable.
export function resolveConnection(
  apiUrl: SettingInspection | undefined,
  token: SettingInspection | undefined,
  env: Env
): ConnectionSettings {
  return {
    apiUrl: explicitValue(apiUrl) || env.DARUMA_API_URL || DEFAULT_API_URL,
    token: explicitValue(token) || env.DARUMA_TOKEN || ""
  };
}

function explicitValue(inspection: SettingInspection | undefined): string | undefined {
  if (!inspection) {
    return undefined;
  }
  return inspection.workspaceFolderValue || inspection.workspaceValue || inspection.globalValue || undefined;
}
