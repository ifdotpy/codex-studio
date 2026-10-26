import "/src/layout/layout-contract.generated.css";
import "/src/hooks/viewport-surface.css";
import "/src/servers/servers.css";

const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
fixtureRoot.innerHTML =
  '<div class="multi-server-shell" data-navigation-compact="true">' +
  '<button class="server-mobile-toggle">Servers and chats</button>' +
  '<div class="server-views"><main>Conversation</main></div></div>';
const query = new URLSearchParams(location.search);
const side = query.get("rail");
// The native shell's portrait top strip (mobileViewport.ts, ios_viewport.js).
if (query.has("strip")) {
  document.documentElement.setAttribute("data-viewport-top-strip", "");
  document.documentElement.style.setProperty("--native-safe-top", "82px");
}
const drawer = document.createElement("div");
drawer.className = "mantine-Drawer-inner";
drawer.style.position = "fixed";
document.body.append(drawer);
if (side) {
  const rail = document.createElement("nav");
  rail.className = "edge-rail";
  rail.dataset.side = side;
  document.body.append(rail);
}
document.body.dataset.ready = "true";
