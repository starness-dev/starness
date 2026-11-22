import { useEffect, useId, useRef, useState } from "react";
import mermaid from "mermaid";
import { Maximize2, Minimize2, RotateCcw, ZoomIn, ZoomOut } from "lucide-react";

let initialized = false;

export function MermaidDiagram({
  source,
  activeStates,
}: {
  source: string;
  activeStates?: string[];
}) {
  const id = useId().replace(/:/g, "_");
  const [svg, setSvg] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [scale, setScale] = useState<number>(1);
  const [offset, setOffset] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [dragging, setDragging] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const dragStartRef = useRef<{ x: number; y: number; ox: number; oy: number } | null>(null);

  useEffect(() => {
    if (!source.trim()) {
      setSvg("");
      return;
    }
    if (!initialized) {
      mermaid.initialize({
        startOnLoad: false,
        theme: "dark",
        securityLevel: "strict",
      });
      initialized = true;
    }

    let cancelled = false;
    const renderKey = `mermaid_${id}_${Date.now()}`;
    let decoratedSource = source;
    if (activeStates && activeStates.length > 0) {
      const leafIds = Array.from(
        new Set(
          activeStates
            .map((st) => st.split(".").pop()?.trim() ?? "")
            .filter((s) => /^[A-Za-z0-9_]+$/.test(s)),
        ),
      );
      if (leafIds.length > 0) {
        decoratedSource = `${source}\n    classDef starnessActive fill:#0c4a6e,stroke:#38bdf8,stroke-width:2.5px,color:#f0f9ff\n${leafIds
          .map((s) => `    class ${s} starnessActive`)
          .join("\n")}\n`;
      }
    }
    mermaid
      .render(renderKey, decoratedSource)
      .then(({ svg: rendered }) => {
        if (!cancelled) {
          setSvg(rendered);
          setError(null);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
        }
      });

    return () => {
      cancelled = true;
    };
  }, [source, activeStates, id]);

  useEffect(() => {
    if (!expanded) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setExpanded(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [expanded]);

  const zoomBy = (factor: number) => {
    setScale((s) => Math.min(4, Math.max(0.4, Number((s * factor).toFixed(2)))));
  };

  const resetView = () => {
    setScale(1);
    setOffset({ x: 0, y: 0 });
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    setDragging(true);
    dragStartRef.current = { x: e.clientX, y: e.clientY, ox: offset.x, oy: offset.y };
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!dragging || !dragStartRef.current) return;
    const dx = e.clientX - dragStartRef.current.x;
    const dy = e.clientY - dragStartRef.current.y;
    setOffset({ x: dragStartRef.current.ox + dx, y: dragStartRef.current.oy + dy });
  };

  const onPointerUp = () => {
    setDragging(false);
    dragStartRef.current = null;
  };

  const onWheel = (e: React.WheelEvent<HTMLDivElement>) => {
    e.preventDefault();
    zoomBy(e.deltaY < 0 ? 1.12 : 0.89);
  };

  if (error) {
    return (
      <div className="rounded-lg border border-rose-500/30 bg-rose-950/20 p-3 text-xs text-rose-200">
        Diagram render error: {error}
        <pre className="mt-2 overflow-x-auto text-zinc-400">{source}</pre>
      </div>
    );
  }

  const canvas = (heightClass: string) => (
    <div className="space-y-2.5 flex flex-col">
      <div className="flex flex-wrap items-center justify-between gap-2 text-xs">
        <div className="flex flex-wrap items-center gap-1.5">
          {activeStates && activeStates.length > 0 && (
            <>
              <span className="text-zinc-400">Active:</span>
              {activeStates.map((st) => (
                <span
                  key={st}
                  className="rounded-full border border-sky-500/40 bg-sky-500/15 px-2.5 py-0.5 font-mono text-[11px] text-sky-200"
                >
                  {st}
                </span>
              ))}
            </>
          )}
        </div>
        <div className="inline-flex items-center gap-1 rounded-lg border border-zinc-800 bg-zinc-900 p-0.5 text-[11px] text-zinc-300">
          <button
            type="button"
            onClick={() => zoomBy(0.85)}
            title="Zoom out"
            className="rounded p-1 hover:bg-zinc-800 cursor-pointer"
          >
            <ZoomOut className="h-3.5 w-3.5" />
          </button>
          <span className="px-1.5 font-mono text-[10px] text-zinc-400 select-none">
            {Math.round(scale * 100)}%
          </span>
          <button
            type="button"
            onClick={() => zoomBy(1.18)}
            title="Zoom in"
            className="rounded p-1 hover:bg-zinc-800 cursor-pointer"
          >
            <ZoomIn className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={resetView}
            title="Reset zoom & pan"
            className="rounded p-1 hover:bg-zinc-800 cursor-pointer"
          >
            <RotateCcw className="h-3.5 w-3.5" />
          </button>
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            title={expanded ? "Exit fullscreen (Esc)" : "Expand diagram"}
            className="rounded p-1 hover:bg-zinc-800 cursor-pointer"
          >
            {expanded ? <Minimize2 className="h-3.5 w-3.5" /> : <Maximize2 className="h-3.5 w-3.5" />}
          </button>
        </div>
      </div>

      <div
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onWheel={onWheel}
        className={`relative overflow-hidden rounded-xl border border-zinc-800 bg-zinc-900/70 select-none ${heightClass} ${
          dragging ? "cursor-grabbing" : "cursor-grab"
        }`}
      >
        <div
          className="w-full h-full flex items-center justify-center p-4"
          style={{
            transform: `translate(${offset.x}px, ${offset.y}px) scale(${scale})`,
            transformOrigin: "center center",
            transition: dragging ? "none" : "transform 80ms ease-out",
          }}
          dangerouslySetInnerHTML={{ __html: svg }}
        />
        <span className="pointer-events-none absolute bottom-2 right-2.5 text-[10px] text-zinc-500">
          Drag to pan · Scroll to zoom
        </span>
      </div>
    </div>
  );

  if (expanded) {
    return (
      <div className="fixed inset-0 z-50 bg-zinc-950/90 backdrop-blur-sm p-6 flex flex-col">
        {canvas("flex-1 min-h-[75vh]")}
      </div>
    );
  }

  return canvas("min-h-[360px] h-[55vh]");
}
