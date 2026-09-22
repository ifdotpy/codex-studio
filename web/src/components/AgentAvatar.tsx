import { useId } from "react";

// Each agent keeps the same abstract avatar across sessions.
export default function AgentAvatar({
  id,
  size = 28,
}: {
  id: string;
  size?: number;
}) {
  const gradient = useId();
  let seed = 2166136261;
  for (const char of id)
    seed = Math.imul(seed ^ char.charCodeAt(0), 16777619) >>> 0;
  const hue = seed % 360;
  return (
    <svg
      className="agent-avatar"
      width={size}
      height={size}
      viewBox="0 0 40 40"
      aria-hidden="true"
    >
      <defs>
        <linearGradient id={gradient} x1="0" y1="0" x2="1" y2="1">
          <stop stopColor={`hsl(${hue} 44% 72%)`} />
          <stop offset="1" stopColor={`hsl(${(hue + 35) % 360} 48% 46%)`} />
        </linearGradient>
      </defs>
      <circle cx="20" cy="20" r="20" fill={`url(#${gradient})`} />
      <g
        transform={`rotate(${seed % 360} 20 20)`}
        fill="none"
        stroke="white"
        strokeOpacity=".75"
        strokeWidth="4"
        strokeLinecap="round"
      >
        <path d="M12 24 C12 10 28 10 28 24" />
        <path d={seed % 2 ? "M17 28 H23" : "M20 23 V28"} />
      </g>
    </svg>
  );
}
