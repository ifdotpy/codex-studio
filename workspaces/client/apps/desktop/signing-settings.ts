// Owner of the signing configuration shape and its runtime validation.
export interface SigningConfiguration {
  identity?: string | undefined;
  keychain?: string | undefined;
  passwordFile?: string | undefined;
}

export function validate(identity: unknown): asserts identity is string {
  if (typeof identity !== "string" || !/^[a-fA-F0-9]{40}$/.test(identity))
    throw new Error(
      "Configure a persistent Studio signing certificate. Ad hoc signing loses macOS permissions after updates.",
    );
}

export function validateSigningConfiguration(
  value: unknown,
  source: string,
): asserts value is SigningConfiguration {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error(`Signing configuration ${source} must be an object.`);
  const fields = value as Record<string, unknown>;
  for (const key of ["identity", "keychain", "passwordFile"] as const)
    if (fields[key] !== undefined && typeof fields[key] !== "string")
      throw new Error(`"${key}" in ${source} must be a string.`);
  try {
    validate(fields.identity);
  } catch {
    throw new Error(
      `"identity" in ${source} must be a 40 character hexadecimal certificate fingerprint.`,
    );
  }
  if (fields.passwordFile && !fields.keychain)
    throw new Error(`"passwordFile" in ${source} needs a "keychain" path.`);
}
