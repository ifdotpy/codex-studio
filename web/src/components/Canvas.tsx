import { useRef, useState } from "react";
import { save, saved } from "../api";
import { statusLabel, type Agent } from "../types";
type Position = { x: number; y: number };
type Camera = Position & { z: number };
export default function Canvas({
  agents,
  stateDir,
  opened,
  open,
}: {
  agents: Agent[];
  stateDir: string;
  opened: string | null;
  open: (id: string) => void;
}) {
  const key = `codex-canvas-graph:${stateDir}`;
  const [layout, setLayout] = useState(() => {
    const raw = saved<{ positions: Record<string, Position>; view: Camera }>(
      key,
      { positions: {}, view: { x: 40, y: 40, z: 1 } },
    );
    const positions = Object.fromEntries(
      Object.entries(raw.positions || {}).filter(
        ([, p]) => Number.isFinite(p.x) && Number.isFinite(p.y),
      ),
    );
    agents.forEach(
      (a, i) =>
        (positions[a.id] ||= { x: (i % 4) * 270, y: Math.floor(i / 4) * 140 }),
    );
    return {
      positions,
      view:
        raw.view && [raw.view.x, raw.view.y, raw.view.z].every(Number.isFinite)
          ? { ...raw.view, z: Math.max(0.15, Math.min(2, raw.view.z)) }
          : { x: 40, y: 40, z: 1 },
    };
  });
  const host = useRef<HTMLElement>(null),
    drag = useRef<{
      point: Position;
      node?: string;
      position?: Position;
      camera: Camera;
      moved: boolean;
    } | null>(null);
  const positions = { ...layout.positions };
  agents.forEach(
    (a, i) =>
      (positions[a.id] ||= { x: (i % 4) * 270, y: Math.floor(i / 4) * 140 }),
  );
  const commit = (view: Camera, next = positions) => {
    const value = { view, positions: next };
    setLayout(value);
    save(key, value);
  };
  const fit = () => {
    const ps = agents.map((a) => positions[a.id]);
    if (!ps.length) return;
    const x = Math.min(...ps.map((p) => p.x)),
      y = Math.min(...ps.map((p) => p.y)),
      w = Math.max(...ps.map((p) => p.x)) + 210 - x,
      h = Math.max(...ps.map((p) => p.y)) + 90 - y,
      z = Math.max(
        0.15,
        Math.min(
          1.15,
          ((host.current?.clientWidth || 900) - 80) / w,
          ((host.current?.clientHeight || 600) - 80) / h,
        ),
      );
    commit({ x: 40 - x * z, y: 40 - y * z, z });
  };
  return (
    <section
      id="canvas"
      ref={host}
      aria-label="All agents canvas"
      onWheel={(e) => {
        const r = e.currentTarget.getBoundingClientRect(),
          x = e.clientX - r.left,
          y = e.clientY - r.top,
          z = Math.max(
            0.15,
            Math.min(2, layout.view.z * Math.exp(-e.deltaY * 0.002)),
          ),
          ratio = z / layout.view.z;
        commit({
          x: x - (x - layout.view.x) * ratio,
          y: y - (y - layout.view.y) * ratio,
          z,
        });
      }}
      onPointerDown={(e) => {
        if ((e.target as HTMLElement).closest("#fit")) return;
        const node = (e.target as HTMLElement).closest<HTMLElement>(
          "[data-node]",
        )?.dataset.node;
        drag.current = {
          point: { x: e.clientX, y: e.clientY },
          node,
          position: node ? positions[node] : undefined,
          camera: { ...layout.view },
          moved: false,
        };
        e.currentTarget.setPointerCapture(e.pointerId);
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (!d) return;
        const dx = e.clientX - d.point.x,
          dy = e.clientY - d.point.y;
        d.moved ||= Math.abs(dx) + Math.abs(dy) > 4;
        if (d.node && d.position)
          setLayout({
            view: layout.view,
            positions: {
              ...positions,
              [d.node]: {
                x: d.position.x + dx / layout.view.z,
                y: d.position.y + dy / layout.view.z,
              },
            },
          });
        else
          setLayout({
            positions,
            view: { ...d.camera, x: d.camera.x + dx, y: d.camera.y + dy },
          });
      }}
      onPointerUp={() => {
        const d = drag.current;
        drag.current = null;
        if (d?.node && !d.moved) open(d.node);
        else commit(layout.view);
      }}
      onPointerCancel={() => {
        const d = drag.current;
        drag.current = null;
        if (d)
          commit(
            d.camera,
            d.node && d.position
              ? { ...positions, [d.node]: d.position }
              : positions,
          );
      }}
    >
      <div
        id="world"
        style={{
          transform: `translate(${layout.view.x}px,${layout.view.y}px) scale(${layout.view.z})`,
        }}
      >
        <svg id="edges">
          {agents
            .filter(
              (a) => a.parentId && agents.some((p) => p.id === a.parentId),
            )
            .map((a) => {
              const p = positions[a.parentId!],
                q = positions[a.id];
              return (
                <path
                  key={a.id}
                  d={`M${p.x + 210},${p.y + 38} C${p.x + 245},${p.y + 38} ${q.x - 35},${q.y + 38} ${q.x},${q.y + 38}`}
                />
              );
            })}
        </svg>
        <div id="nodes">
          {agents.map((a) => (
            <button
              key={a.id}
              data-node={a.id}
              className={`node ${opened === a.id ? "selected" : ""}`}
              style={{ left: positions[a.id].x, top: positions[a.id].y }}
              onClick={(e) => {
                if (e.detail === 0) open(a.id);
              }}
            >
              <span className={`dot ${a.status}`} />
              <span>
                <strong>{a.name}</strong>
                <small>
                  {a.isLead ? "Lead · " : ""}
                  {statusLabel(a.status)}
                </small>
              </span>
            </button>
          ))}
        </div>
      </div>
      <button id="fit" className="canvas-fit" onClick={fit}>
        Fit
      </button>
      <p className="canvas-hint">Drag to pan · Scroll to zoom</p>
    </section>
  );
}
