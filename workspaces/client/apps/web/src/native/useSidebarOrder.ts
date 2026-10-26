import { useServerSidebarServices } from "./servicesContext";
import type { SidebarServices } from "../components/sidebar/serviceContext";
import { useSidebarOrderWithServices } from "../components/useSidebarOrderCore";
export { sidebarOrderCanWrite } from "../components/useSidebarOrderCore";
export function useSidebarOrder(
  key: Parameters<typeof useSidebarOrderWithServices>[0],
  server: Parameters<typeof useSidebarOrderWithServices>[1],
  refresh?: () => Promise<void>,
  notify?: (text: string) => void,
  services?: SidebarServices,
  sessionToken?: string,
) {
  const captured = useServerSidebarServices();
  return useSidebarOrderWithServices(
    key,
    server,
    refresh,
    notify,
    services ?? captured,
    sessionToken,
  );
}
