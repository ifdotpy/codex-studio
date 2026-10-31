import { importNativeProfile } from "./native/preferences";
import { cleanupLegacyPersistence } from "./browserPersistence";

// Preferences must be ready before modules read their initial settings.
void importNativeProfile()
  .then(() => cleanupLegacyPersistence())
  .then(() => import("./mount"))
  .catch((error) => {
    const container = document.getElementById("root");
    if (container)
      container.textContent = `Studio could not recover its settings: ${String(error)}`;
  });
