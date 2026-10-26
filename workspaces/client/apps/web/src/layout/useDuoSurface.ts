import { useLayoutEffect, useState } from "react";

/** Read the viewport owner's orientation-independent Duo control profile. */
export function useDuoSurface() {
  const [duo, setDuo] = useState(false);
  useLayoutEffect(() => {
    const root = document.documentElement;
    const update = () => setDuo(root.hasAttribute("data-duo-ui"));
    const observer = new MutationObserver(update);
    observer.observe(root, {
      attributes: true,
      attributeFilter: ["data-duo-ui"],
    });
    update();
    return () => observer.disconnect();
  }, []);
  return duo;
}
