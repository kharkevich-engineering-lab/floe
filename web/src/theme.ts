/**
 * Colour theme, shared with the Kharkevich Engineering Lab sites
 * (kharkevich.com, hub.kharkevich.com, ai.kharkevich.com): the same
 * `kharkevich-theme` localStorage key and the same three-state cycle
 * system → light → dark. Dark is the default palette; `html.light` switches it.
 * `index.html` applies the stored choice before first paint.
 */
import { useEffect, useSyncExternalStore } from "react";

export type Theme = "system" | "light" | "dark";
export const THEME_KEY = "kharkevich-theme";
const THEMES: Theme[] = ["system", "light", "dark"];

const media = typeof window !== "undefined" ? window.matchMedia("(prefers-color-scheme: light)") : null;
const listeners = new Set<() => void>();

function read(): Theme {
  try {
    const s = localStorage.getItem(THEME_KEY);
    if (s === "light" || s === "dark") return s;
  } catch {
    /* storage unavailable */
  }
  return "system";
}

let current: Theme = read();

/** Whether the light palette is in effect. */
export function isLight(t: Theme = current): boolean {
  return t === "light" || (t === "system" && (media?.matches ?? false));
}

function apply() {
  document.documentElement.classList.toggle("light", isLight());
  for (const l of listeners) l();
}

media?.addEventListener("change", () => {
  if (current === "system") apply();
});

export function nextTheme(): void {
  current = THEMES[(THEMES.indexOf(current) + 1) % THEMES.length] ?? "system";
  try {
    localStorage.setItem(THEME_KEY, current);
  } catch {
    /* ignore */
  }
  apply();
}

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => listeners.delete(l);
};

/** The chosen theme (`system` included) and whether light is in effect — for the toggle and for renderers that need a palette (diffs, code). */
export function useTheme(): { theme: Theme; light: boolean } {
  const theme = useSyncExternalStore(subscribe, () => current);
  const light = useSyncExternalStore(subscribe, () => isLight());
  useEffect(() => {
    // Another tab (or a sister site in this browser) changed the theme.
    const onStorage = (e: StorageEvent) => {
      if (e.key === THEME_KEY) {
        current = read();
        apply();
      }
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);
  return { theme, light };
}

export const THEME_LABEL: Record<Theme, string> = {
  system: "Colour theme: system",
  light: "Colour theme: light",
  dark: "Colour theme: dark",
};
