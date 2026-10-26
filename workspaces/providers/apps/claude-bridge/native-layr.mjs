// Native Layr environment supplied by Studio after workspace preparation.
export function nativeLayrEnvironment(session) {
  const value = session.studioNativeLayrEnvironment;
  if (value === undefined) return {};
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Invalid native Layr environment");
  for (const [key, text] of Object.entries(value)) {
    if (!/^LAYR_[A-Z_]+$/.test(key) || typeof text !== "string" || text.includes("\0"))
      throw new Error("Invalid native Layr environment variable");
  }
  return { ...value };
}
