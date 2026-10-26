import {
  useOptionalSidebarBackend,
  useSidebarVisualStateFor,
} from "../components/sidebar/serviceContext";
import { useServerSidebarServices } from "./servicesContext";
export * from "../components/sidebar/serviceContext";

/** A row override wins, otherwise the enclosing immutable native owner supplies it. */
export function useSidebarBackend() {
  const row = useOptionalSidebarBackend();
  const services = useServerSidebarServices();
  return row ?? services.cache;
}
export function useSidebarVisualState<T>(key: string, fallback: T) {
  return useSidebarVisualStateFor(useSidebarBackend(), key, fallback);
}
