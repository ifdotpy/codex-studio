import "/src/layout/layout-contract.generated.css";
import "/src/hooks/viewport-surface.css";
import "/src/servers/servers.css";

const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
fixtureRoot.innerHTML =
  '<div class="multi-server-shell" data-navigation-compact="true">' +
  '<button class="server-mobile-toggle">Servers and chats</button>' +
  '<div class="server-views"><main>Conversation</main></div></div>';
const side = new URLSearchParams(location.search).get("rail");
if (side) {
  const rail = document.createElement("nav");
  rail.className = "edge-rail";
  rail.dataset.side = side;
  document.body.append(rail);
}
document.body.dataset.ready = "true";
