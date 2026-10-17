import { initializeNativePreferences } from "./native/preferences";

// Preferences must be ready before modules read their initial settings.
void initializeNativePreferences()
  .then(() => import("./mount"))
  .catch((error) => {
    const container = document.getElementById("root");
    if (container)
      container.textContent = `Studio could not recover its settings: ${String(error)}`;
  });
