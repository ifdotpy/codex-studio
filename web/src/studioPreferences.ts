export const studioPreferencesStorageKey = "codex-studio-preferences-v1";

export const fontFamilies = {
  system: {
    label: "System sans-serif",
    css: 'system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
  },
  arial: { label: "Arial", css: 'Arial, "Helvetica Neue", sans-serif' },
  inter: { label: "Inter", css: "Inter, system-ui, sans-serif" },
  georgia: { label: "Georgia", css: 'Georgia, "Times New Roman", serif' },
  verdana: { label: "Verdana", css: "Verdana, Geneva, sans-serif" },
} as const;

export type StudioFontFamily = keyof typeof fontFamilies;
export interface StudioPreferences {
  theme: "auto" | "light" | "dark";
  typography: "original" | "custom";
  contentLayout: "original" | "custom";
  sidebarFontSize: number;
  mainFontSize: number;
  fontFamily: StudioFontFamily;
  contentWidth: number;
  sidebarShortcut: string;
  showMessageAvatars: boolean;
}

export const defaultStudioPreferences: StudioPreferences = {
  theme: "auto",
  typography: "custom",
  contentLayout: "custom",
  sidebarFontSize: 14,
  mainFontSize: 14,
  fontFamily: "system",
  contentWidth: 100,
  sidebarShortcut: `${navigator.platform.toLowerCase().includes("mac") ? "Meta" : "Control"}+b`,
  showMessageAvatars: false,
};

const validThemes = new Set(["auto", "light", "dark"]);
const validFonts = new Set<string>(Object.keys(fontFamilies));

export function parseStudioPreferences(value: string): StudioPreferences {
  const parsed: unknown = JSON.parse(value);
  if (!parsed || typeof parsed !== "object")
    throw new Error("Invalid preferences");
  const candidate = parsed as Record<string, unknown>;
  if (
    !validThemes.has(String(candidate.theme)) ||
    (candidate.typography !== undefined &&
      candidate.typography !== "original" &&
      candidate.typography !== "custom") ||
    (candidate.contentLayout !== undefined &&
      candidate.contentLayout !== "original" &&
      candidate.contentLayout !== "custom") ||
    !Number.isInteger(candidate.sidebarFontSize) ||
    Number(candidate.sidebarFontSize) < 12 ||
    Number(candidate.sidebarFontSize) > 24 ||
    !Number.isInteger(candidate.mainFontSize) ||
    Number(candidate.mainFontSize) < 12 ||
    Number(candidate.mainFontSize) > 24 ||
    !validFonts.has(String(candidate.fontFamily)) ||
    !Number.isInteger(candidate.contentWidth) ||
    Number(candidate.contentWidth) < 60 ||
    Number(candidate.contentWidth) > 100 ||
    (candidate.showMessageAvatars !== undefined &&
      typeof candidate.showMessageAvatars !== "boolean") ||
    typeof candidate.sidebarShortcut !== "string" ||
    !parseSidebarShortcut(candidate.sidebarShortcut)
  )
    throw new Error("Invalid preferences");
  return {
    theme: candidate.theme as StudioPreferences["theme"],
    typography: candidate.typography === "original" ? "original" : "custom",
    contentLayout:
      candidate.contentLayout === "original" ? "original" : "custom",
    sidebarFontSize: candidate.sidebarFontSize as number,
    mainFontSize: candidate.mainFontSize as number,
    fontFamily: candidate.fontFamily as StudioPreferences["fontFamily"],
    contentWidth: candidate.contentWidth as number,
    sidebarShortcut: candidate.sidebarShortcut,
    showMessageAvatars: candidate.showMessageAvatars === true,
  };
}

export function parseSidebarShortcut(value: string): {
  key: string;
  meta: boolean;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
} | null {
  const parts = value.split("+");
  if (parts.length < 2 || parts.length > 3) return null;
  const key = parts.at(-1)!.toLowerCase();
  const modifiers = parts.slice(0, -1).map((part) => part.toLowerCase());
  if (
    !/^[a-z0-9]$/.test(key) ||
    modifiers.some(
      (part) => !["meta", "control", "alt", "shift"].includes(part),
    ) ||
    new Set(modifiers).size !== modifiers.length ||
    modifiers.includes("alt") ||
    modifiers.includes("meta") === modifiers.includes("control")
  )
    return null;
  if (["r", "l", "t", "w", "n", "p", "f", "k"].includes(key)) return null;
  if (modifiers.includes("shift") && ["i", "j", "c"].includes(key)) return null;
  return {
    key,
    meta: modifiers.includes("meta"),
    ctrl: modifiers.includes("control"),
    alt: false,
    shift: modifiers.includes("shift"),
  };
}

export function formatSidebarShortcut(value: string): string {
  return value
    .split("+")
    .map(
      (part) =>
        ({ meta: "⌘", control: "Ctrl", shift: "Shift", alt: "Alt" })[
          part.toLowerCase()
        ] || part.toUpperCase(),
    )
    .join("+");
}
