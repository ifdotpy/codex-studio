/** Phone identity for the Duo viewport profile. Geometry remains owned by isDuoSurface. */
export function isDuoDevice(input: {
  nativeHinge: string | undefined;
  userAgent: string;
  platform: string;
  maxTouchPoints: number;
  mobile?: boolean | undefined;
}) {
  if (["open", "partial", "closed"].includes(input.nativeHinge || ""))
    return true;
  if (
    /iPad/i.test(input.userAgent) ||
    (input.platform === "MacIntel" && input.maxTouchPoints > 1)
  )
    return false;
  return /iPhone|iPod/i.test(input.userAgent) || input.mobile === true;
}
