import SidebarView, { type SidebarProps } from "../components/SidebarView";
import { useServerOwner, useServerSidebarServices } from "./servicesContext";
export * from "../components/SidebarView";
export default function NativeSidebar(props: SidebarProps) {
  const owner = useServerOwner();
  const services = useServerSidebarServices();
  const viewOwnerId = owner.id === "local" ? null : owner.id;
  const alias = owner.alias || owner.label;
  return (
    <SidebarView
      {...props}
      services={props.services ?? services}
      defaultBackendOwnerId={owner.id}
      aliases={{ [owner.id]: alias }}
      viewOwnerId={viewOwnerId}
      homeHref={viewOwnerId ? location.pathname + location.search : "/"}
      fallbackAlias={alias}
    />
  );
}
