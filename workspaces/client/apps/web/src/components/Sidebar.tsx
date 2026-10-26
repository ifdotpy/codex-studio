import SidebarView, { type SidebarProps } from "./SidebarView";
import { localSidebarBackend, localSidebarServices } from "./sidebar/services";
import { readServerAliases } from "../servers/registry";
import {
  DEFAULT_LOCAL_SERVER_ALIAS,
  useServerAliases,
} from "../servers/serverAliases";
import { serverViewId } from "../servers/environment";
export * from "./SidebarView";
export default function Sidebar(props: SidebarProps) {
  const aliases = useServerAliases(readServerAliases);
  return (
    <SidebarView
      {...props}
      services={props.services ?? localSidebarServices}
      defaultBackendOwnerId={localSidebarBackend.ownerId}
      aliases={aliases}
      viewOwnerId={serverViewId}
      homeHref={serverViewId ? location.pathname + location.search : "/"}
      fallbackAlias={DEFAULT_LOCAL_SERVER_ALIAS}
    />
  );
}
