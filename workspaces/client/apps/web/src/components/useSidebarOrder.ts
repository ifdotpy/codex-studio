import { localSidebarServices, type SidebarServices } from "./sidebar/services";
import { useSidebarOrderWithServices } from "./useSidebarOrderCore";
export { sidebarOrderCanWrite } from "./useSidebarOrderCore";
export function useSidebarOrder(
  key: Parameters<typeof useSidebarOrderWithServices>[0],
  server: Parameters<typeof useSidebarOrderWithServices>[1],
  refresh?: () => Promise<void>,
  notify?: (text: string) => void,
  services: SidebarServices = localSidebarServices,
  sessionToken?: string,
) {
  return useSidebarOrderWithServices(
    key,
    server,
    refresh,
    notify,
    services,
    sessionToken,
  );
}
