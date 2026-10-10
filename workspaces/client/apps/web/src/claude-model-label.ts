// Claude Code puts the version in its description, not always in its label.
export function claudeModelLabel(label: string, description: string): string {
  if (!label.startsWith("Claude · ")) return label;
  const version = /^(?:Claude )?([A-Za-z]+) (\d+(?:\.\d+)*)(?=\s|$|[·(])/.exec(
    description,
  );
  if (!version) return label;
  const [, family, number] = version;
  const familyLabel = new RegExp(`\\b${family}(?: \\d+(?:\\.\\d+)*)?\\b`);
  return familyLabel.test(label)
    ? label.replace(familyLabel, `${family} ${number}`)
    : `${label} · ${family} ${number}`;
}
