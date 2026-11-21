import { useState } from "react";
import { Send } from "lucide-react";

export function FallbackEventForm({
  allowedEvents,
  onSend,
}: {
  allowedEvents: string[];
  onSend: (eventType: string, payload?: Record<string, unknown>) => Promise<void>;
}) {
  const [selected, setSelected] = useState(allowedEvents[0] ?? "");
  const [payloadText, setPayloadText] = useState("{}");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const activeEvent = allowedEvents.includes(selected) ? selected : allowedEvents[0] ?? "";

  const submit = async (evName: string) => {
    setError(null);
    let parsed: Record<string, unknown> | undefined;
    const trimmed = payloadText.trim();
    if (trimmed && trimmed !== "{}") {
      try {
        const val = JSON.parse(trimmed);
        if (!val || typeof val !== "object" || Array.isArray(val)) {
          setError("Payload must be a JSON object.");
          return;
        }
        parsed = val as Record<string, unknown>;
      } catch (e) {
        setError(`Invalid JSON payload: ${e instanceof Error ? e.message : String(e)}`);
        return;
      }
    }
    setBusy(true);
    try {
      await onSend(evName, parsed);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (allowedEvents.length === 0) {
    return (
      <div className="rounded-xl border border-zinc-800 bg-zinc-900/60 p-5 text-sm text-zinc-400">
        No outgoing events declared for the current active state.
      </div>
    );
  }

  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900/80 p-5 space-y-4">
      <div>
        <h3 className="text-sm font-semibold text-zinc-100">Waiting for Event</h3>
        <p className="text-xs text-zinc-400">
          Auto-generated controls from this state&apos;s declared <code className="text-zinc-200">allowed_events</code>.
        </p>
      </div>

      <div className="flex flex-wrap gap-2">
        {allowedEvents.map((ev) => (
          <button
            key={ev}
            type="button"
            onClick={() => setSelected(ev)}
            className={`rounded-lg border px-3 py-1.5 font-mono text-xs cursor-pointer transition ${
              activeEvent === ev
                ? "border-sky-500 bg-sky-500/20 text-sky-200"
                : "border-zinc-700 bg-zinc-800/70 text-zinc-300 hover:border-zinc-500"
            }`}
          >
            {ev}
          </button>
        ))}
      </div>

      <label className="block space-y-1">
        <span className="text-xs font-medium text-zinc-400">Optional JSON Payload</span>
        <textarea
          rows={3}
          value={payloadText}
          onChange={(e) => setPayloadText(e.target.value)}
          className="w-full rounded-lg border border-zinc-700 bg-zinc-950 px-3 py-2 font-mono text-xs text-zinc-100 focus:border-sky-500 focus:outline-none"
        />
      </label>

      {error && <div className="text-xs text-rose-400">{error}</div>}

      <div className="flex justify-end">
        <button
          type="button"
          disabled={busy || !activeEvent}
          onClick={() => submit(activeEvent)}
          className="inline-flex items-center gap-1.5 rounded-lg bg-sky-600 px-4 py-2 text-xs font-semibold text-white hover:bg-sky-500 disabled:opacity-50 cursor-pointer"
        >
          <Send className="h-3.5 w-3.5" />
          Dispatch {activeEvent}
        </button>
      </div>
    </div>
  );
}
