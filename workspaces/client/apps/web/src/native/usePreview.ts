import { useMemo } from "react";
import { useServerServices } from "./servicesContext";

export function useNativePreview() {
  const services = useServerServices();
  return useMemo(() => Promise.resolve(services.previews), [services.previews]);
}

/** A native action uses this port directly in its button callback. */
export function useNativePreviewPort() {
  return useServerServices().previews;
}
