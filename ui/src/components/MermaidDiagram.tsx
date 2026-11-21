import { useEffect, useId, useState } from "react";
import mermaid from "mermaid";

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
    mermaid
      .render(renderKey, source)
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
  }, [source, id]);

  if (error) {
    return (
      <div className="rounded-lg border border-rose-500/30 bg-rose-950/20 p-3 text-xs text-rose-200">
        Diagram render error: {error}
        <pre className="mt-2 overflow-x-auto text-zinc-400">{source}</pre>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {activeStates && activeStates.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5 text-xs">
          <span className="text-zinc-400">Active:</span>
          {activeStates.map((st) => (
            <span
              key={st}
              className="rounded-full border border-sky-500/40 bg-sky-500/15 px-2.5 py-0.5 font-mono text-[11px] text-sky-200"
            >
              {st}
            </span>
          ))}
        </div>
      )}
      <div
        className="overflow-x-auto rounded-xl border border-zinc-800 bg-zinc-900/70 p-4"
        dangerouslySetInnerHTML={{ __html: svg }}
      />
    </div>
  );
}
