import { useCallback, useEffect, useRef, useState } from "react";
import { JSONUIProvider, Renderer, createStateStore } from "@json-render/react";
import {
  Braces,
  GitBranch,
  Layers,
  Loader2,
  Play,
  RefreshCw,
  Square,
  Terminal,
} from "lucide-react";
import {
  type InstanceSnapshot,
  type MachineSummary,
  type TraceRecord,
  cancelInstance,
  createInstance,
  getDiagram,
  getInstance,
  listInstances,
  listMachines,
  sendEvent,
  subscribeInstanceTrace,
} from "./api";
import { starnessRegistry } from "./registry";
import { MermaidDiagram } from "./components/MermaidDiagram";
import { FallbackEventForm } from "./components/FallbackEventForm";

const STATUS_BADGE: Record<string, string> = {
  active: "bg-sky-500/15 text-sky-300 border-sky-500/40",
  waiting: "bg-amber-500/15 text-amber-200 border-amber-500/40",
  completed: "bg-emerald-500/15 text-emerald-300 border-emerald-500/40",
  failed: "bg-rose-500/15 text-rose-300 border-rose-500/40",
  cancelled: "bg-zinc-700/40 text-zinc-400 border-zinc-700",
};

type InspectorTab = "diagram" | "trace" | "context";

export function App() {
  const [machines, setMachines] = useState<MachineSummary[]>([]);
  const [selectedMachine, setSelectedMachine] = useState<string>("");
  const [diagram, setDiagram] = useState<string>("");
  const [instances, setInstances] = useState<InstanceSnapshot[]>([]);
  const [selectedInstanceId, setSelectedInstanceId] = useState<string>("");
  const [snapshot, setSnapshot] = useState<InstanceSnapshot | null>(null);
  const [traces, setTraces] = useState<TraceRecord[]>([]);
  const [inspectorTab, setInspectorTab] = useState<InspectorTab>("diagram");
  const [error, setError] = useState<string | null>(null);
  const [newCtxJson, setNewCtxJson] = useState("{}");
  const [showNewForm, setShowNewForm] = useState(false);

  const traceEndRef = useRef<HTMLDivElement | null>(null);
  const stateStoreRef = useRef(createStateStore({}));
  const lastUiKeyRef = useRef<string>("");

  const refreshMachines = useCallback(async () => {
    try {
      const list = await listMachines();
      setMachines(list);
      setSelectedMachine((prev) => (prev && list.some((m) => m.id === prev) ? prev : list[0]?.id ?? ""));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  const refreshInstances = useCallback(async (machineId: string) => {
    if (!machineId) return;
    try {
      const list = await listInstances(machineId);
      setInstances(list);
      setSelectedInstanceId((prev) =>
        prev && list.some((inst) => inst.id === prev) ? prev : list[0]?.id ?? "",
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  const refreshSnapshot = useCallback(async (machineId: string, instanceId: string) => {
    if (!machineId || !instanceId) {
      setSnapshot(null);
      return;
    }
    try {
      const snap = await getInstance(machineId, instanceId);
      setSnapshot(snap);
      const nextKey = `${snap.id}:${snap.active_states.join(",")}:${snap.updated_at_ms}`;
      if (lastUiKeyRef.current !== nextKey) {
        lastUiKeyRef.current = nextKey;
        stateStoreRef.current = createStateStore({ ...(snap.context ?? {}) });
      }
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refreshMachines();
  }, [refreshMachines]);

  useEffect(() => {
    if (!selectedMachine) return;
    void getDiagram(selectedMachine).then(setDiagram);
    void refreshInstances(selectedMachine);
  }, [selectedMachine, refreshInstances]);

  useEffect(() => {
    if (!selectedMachine || !selectedInstanceId) {
      setSnapshot(null);
      setTraces([]);
      return;
    }
    setTraces([]);
    void refreshSnapshot(selectedMachine, selectedInstanceId);
    const unsub = subscribeInstanceTrace(selectedMachine, selectedInstanceId, (rec) => {
      setTraces((prev) => {
        if (prev.some((p) => p.seq === rec.seq)) return prev;
        return [...prev.slice(-400), rec];
      });
      if (rec.kind === "transition" || rec.kind === "started" || rec.kind === "stopped" || rec.kind === "error") {
        void refreshSnapshot(selectedMachine, selectedInstanceId);
        void refreshInstances(selectedMachine);
      }
    });
    return unsub;
  }, [selectedMachine, selectedInstanceId, refreshSnapshot, refreshInstances]);

  useEffect(() => {
    traceEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [traces.length]);

  const handleCreateInstance = async () => {
    if (!selectedMachine) return;
    setError(null);
    let ctx: Record<string, unknown> | undefined;
    const trimmed = newCtxJson.trim();
    if (trimmed && trimmed !== "{}") {
      try {
        ctx = JSON.parse(trimmed) as Record<string, unknown>;
      } catch (e) {
        setError(`Invalid initial context JSON: ${e instanceof Error ? e.message : String(e)}`);
        return;
      }
    }
    try {
      const created = await createInstance(selectedMachine, ctx);
      setShowNewForm(false);
      setNewCtxJson("{}");
      await refreshInstances(selectedMachine);
      setSelectedInstanceId(created.id);
      setSnapshot(created);
      stateStoreRef.current = createStateStore({ ...(created.context ?? {}) });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const handleDispatchEvent = useCallback(
    async (eventType: string, payload?: Record<string, unknown>) => {
      if (!selectedMachine || !selectedInstanceId) return;
      try {
        const updated = await sendEvent(selectedMachine, selectedInstanceId, eventType, payload);
        setSnapshot(updated);
        stateStoreRef.current = createStateStore({ ...(updated.context ?? {}) });
        await refreshInstances(selectedMachine);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    },
    [selectedMachine, selectedInstanceId, refreshInstances],
  );

  const handleCancel = async () => {
    if (!selectedMachine || !selectedInstanceId) return;
    try {
      await cancelInstance(selectedMachine, selectedInstanceId);
      await refreshSnapshot(selectedMachine, selectedInstanceId);
      await refreshInstances(selectedMachine);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const currentMachineMeta = machines.find((m) => m.id === selectedMachine);

  return (
    <div className="min-h-screen flex flex-col bg-zinc-950 text-zinc-100">
      <header className="flex items-center justify-between border-b border-zinc-800 bg-zinc-900/90 px-5 py-3">
        <div className="flex items-center gap-2.5">
          <Layers className="h-5 w-5 text-sky-400" />
          <span className="font-semibold tracking-tight text-sm">Starness Studio</span>
          {selectedMachine && (
            <span className="rounded-md bg-zinc-800 px-2 py-0.5 font-mono text-xs text-zinc-300">
              {selectedMachine}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => {
              void refreshMachines();
              if (selectedMachine) void refreshInstances(selectedMachine);
            }}
            className="inline-flex items-center gap-1.5 rounded-lg border border-zinc-700 bg-zinc-800 px-2.5 py-1.5 text-xs text-zinc-200 hover:bg-zinc-700 cursor-pointer"
          >
            <RefreshCw className="h-3.5 w-3.5" />
            Refresh
          </button>
        </div>
      </header>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-950/40 px-5 py-2 text-xs text-rose-200">
          {error}
        </div>
      )}

      <div className="flex-1 grid grid-cols-1 lg:grid-cols-12 min-h-0">
        {/* Left Sidebar: Machines & Instances */}
        <aside className="lg:col-span-2 border-r border-zinc-800 bg-zinc-900/40 p-4 space-y-5 overflow-y-auto">
          <section className="space-y-2">
            <h2 className="text-[11px] font-semibold uppercase tracking-wider text-zinc-400">
              Machines ({machines.length})
            </h2>
            <div className="space-y-1">
              {machines.map((m) => (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => setSelectedMachine(m.id)}
                  className={`w-full text-left rounded-lg px-3 py-2 text-xs transition cursor-pointer ${
                    selectedMachine === m.id
                      ? "bg-sky-500/15 border border-sky-500/40 text-sky-200"
                      : "border border-transparent hover:bg-zinc-800/70 text-zinc-300"
                  }`}
                >
                  <div className="font-mono font-medium">{m.id}</div>
                  {m.description && (
                    <div className="text-[11px] text-zinc-400 truncate mt-0.5">{m.description}</div>
                  )}
                </button>
              ))}
            </div>
          </section>

          {selectedMachine && (
            <section className="space-y-2.5">
              <div className="flex items-center justify-between">
                <h2 className="text-[11px] font-semibold uppercase tracking-wider text-zinc-400">
                  Instances ({instances.length})
                </h2>
                <div className="flex items-center gap-1.5">
                  <button
                    type="button"
                    onClick={() => setShowNewForm((s) => !s)}
                    title="Start with custom initial JSON context"
                    className={`inline-flex items-center rounded-md border px-1.5 py-1 text-[11px] cursor-pointer ${
                      showNewForm
                        ? "border-sky-500/50 bg-sky-500/15 text-sky-300"
                        : "border-zinc-700 bg-zinc-800/80 text-zinc-400 hover:text-zinc-200"
                    }`}
                  >
                    <Braces className="h-3 w-3" />
                  </button>
                  <button
                    type="button"
                    onClick={() => void handleCreateInstance()}
                    className="inline-flex items-center gap-1 rounded-md bg-sky-600 px-2.5 py-1 text-[11px] font-medium text-white hover:bg-sky-500 cursor-pointer"
                  >
                    <Play className="h-3 w-3" />
                    New
                  </button>
                </div>
              </div>

              {showNewForm && (
                <div className="rounded-lg border border-zinc-700 bg-zinc-900 p-3 space-y-2">
                  <label className="block text-[11px] text-zinc-400">
                    Initial context override (JSON)
                  </label>
                  <textarea
                    rows={3}
                    value={newCtxJson}
                    onChange={(e) => setNewCtxJson(e.target.value)}
                    className="w-full rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-[11px] text-zinc-100"
                  />
                  <div className="flex justify-end gap-2">
                    <button
                      type="button"
                      onClick={() => setShowNewForm(false)}
                      className="text-xs text-zinc-400 hover:text-zinc-200 cursor-pointer"
                    >
                      Cancel
                    </button>
                    <button
                      type="button"
                      onClick={() => void handleCreateInstance()}
                      className="rounded bg-sky-600 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-500 cursor-pointer"
                    >
                      Start
                    </button>
                  </div>
                </div>
              )}

              <div className="space-y-1.5">
                {instances.map((inst) => (
                  <button
                    key={inst.id}
                    type="button"
                    onClick={() => setSelectedInstanceId(inst.id)}
                    className={`w-full text-left rounded-lg border px-3 py-2 text-xs transition cursor-pointer ${
                      selectedInstanceId === inst.id
                        ? "border-sky-500/50 bg-zinc-800/90"
                        : "border-zinc-800/80 bg-zinc-900/50 hover:border-zinc-700"
                    }`}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="font-mono text-[11px] text-zinc-200 truncate">{inst.id}</span>
                      <span
                        className={`rounded-full border px-1.5 py-0.5 text-[10px] font-medium ${
                          STATUS_BADGE[inst.status] ?? STATUS_BADGE.cancelled
                        }`}
                      >
                        {inst.status}
                      </span>
                    </div>
                    <div className="mt-1 font-mono text-[10px] text-zinc-400 truncate">
                      {inst.active_states.join(", ") || "—"}
                    </div>
                  </button>
                ))}
                {instances.length === 0 && (
                  <p className="text-xs text-zinc-500">No instances yet. Click New to start one.</p>
                )}
              </div>
            </section>
          )}
        </aside>

        {/* Center Pane: Always Interactive UI (matches TUI main pane) */}
        <main className="lg:col-span-6 p-5 space-y-4 overflow-y-auto">
          {snapshot ? (
            <>
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div className="flex items-center gap-2">
                  <span
                    className={`rounded-full border px-2.5 py-0.5 text-xs font-medium ${
                      STATUS_BADGE[snapshot.status] ?? STATUS_BADGE.cancelled
                    }`}
                  >
                    {snapshot.status}
                  </span>
                  <span className="font-mono text-xs text-zinc-300">
                    {snapshot.active_states.join(", ")}
                  </span>
                </div>

                {!snapshot.done && snapshot.status !== "cancelled" && snapshot.status !== "failed" && (
                  <button
                    type="button"
                    onClick={() => void handleCancel()}
                    title="Stop this running instance"
                    className="inline-flex items-center gap-1 rounded-lg border border-zinc-700 bg-zinc-800/80 px-2.5 py-1.5 text-xs text-zinc-300 hover:border-rose-500/40 hover:text-rose-300 cursor-pointer"
                  >
                    <Square className="h-3 w-3" />
                    Stop
                  </button>
                )}
              </div>

              <div className="space-y-4">
                  {snapshot.status === "completed" && (
                    <div className="rounded-xl border border-emerald-500/40 bg-emerald-950/25 p-4 text-sm text-emerald-200">
                      <div className="font-semibold">Workflow Completed</div>
                      {snapshot.output != null && (
                        <pre className="mt-2 text-xs text-emerald-100/90 overflow-x-auto">
                          {JSON.stringify(snapshot.output, null, 2)}
                        </pre>
                      )}
                    </div>
                  )}

                  {snapshot.status === "failed" && (
                    <div className="rounded-xl border border-rose-500/40 bg-rose-950/30 p-4 text-sm text-rose-200">
                      <div className="font-semibold">Workflow Failed</div>
                      <pre className="mt-2 text-xs whitespace-pre-wrap">{snapshot.error ?? "Unknown error"}</pre>
                    </div>
                  )}

                  {snapshot.ui ? (
                    <JSONUIProvider
                      key={`${snapshot.id}-${snapshot.active_states.join(",")}-${snapshot.updated_at_ms}`}
                      registry={starnessRegistry}
                      store={stateStoreRef.current}
                      handlers={{
                        send_event: async (params: Record<string, unknown>) => {
                          const evName = String(params.event ?? "");
                          const explicitPayload =
                            params.payload && typeof params.payload === "object"
                              ? (params.payload as Record<string, unknown>)
                              : {};
                          const liveState = stateStoreRef.current.getSnapshot();
                          const mergedPayload: Record<string, unknown> = {
                            ...explicitPayload,
                          };
                          for (const [k, v] of Object.entries(liveState)) {
                            if (
                              !(k in mergedPayload) &&
                              JSON.stringify(snapshot.context?.[k]) !== JSON.stringify(v)
                            ) {
                              mergedPayload[k] = v;
                            }
                          }
                          await handleDispatchEvent(evName, mergedPayload);
                        },
                      }}
                    >
                      <Renderer spec={snapshot.ui} registry={starnessRegistry} />
                    </JSONUIProvider>
                  ) : snapshot.status === "waiting" ? (
                    <FallbackEventForm
                      allowedEvents={snapshot.allowed_events ?? []}
                      onSend={handleDispatchEvent}
                    />
                  ) : snapshot.status === "active" ? (
                    <div className="rounded-xl border border-sky-500/30 bg-sky-950/20 p-6 flex items-center gap-3 text-sm text-sky-200">
                      <Loader2 className="h-4 w-4 animate-spin text-sky-400 shrink-0" />
                      <div>
                        <div className="font-medium">
                          Running in <code className="font-mono text-sky-300">{snapshot.active_states.join(", ")}</code>…
                        </div>
                        <div className="text-xs text-zinc-400 mt-0.5">
                          Waiting for state transition to complete.
                        </div>
                      </div>
                    </div>
                  ) : null}
              </div>
            </>
          ) : (
            <div className="space-y-4">
              {currentMachineMeta && (
                <div className="rounded-xl border border-zinc-800 bg-zinc-900/60 p-5 space-y-2">
                  <h2 className="text-sm font-semibold text-zinc-100">{currentMachineMeta.id}</h2>
                  {currentMachineMeta.description && (
                    <p className="text-xs text-zinc-400">{currentMachineMeta.description}</p>
                  )}
                  <p className="text-xs text-zinc-400">
                    Select an instance on the left or click <strong className="text-zinc-200">New</strong> to start one.
                  </p>
                </div>
              )}
            </div>
          )}
        </main>

        {/* Right Inspector Pane: Live Diagram / Trace / Context (consistent with TUI right drawer) */}
        <aside className="lg:col-span-4 border-l border-zinc-800 bg-zinc-950/80 p-4 flex flex-col min-h-0">
          <div className="flex items-center justify-between gap-2 mb-3">
            <div className="inline-flex rounded-lg border border-zinc-800 bg-zinc-900 p-0.5 text-xs">
              <button
                type="button"
                onClick={() => setInspectorTab("diagram")}
                className={`inline-flex items-center gap-1 rounded-md px-2.5 py-1 cursor-pointer ${
                  inspectorTab === "diagram"
                    ? "bg-sky-500/15 text-sky-200 font-medium"
                    : "text-zinc-400 hover:text-zinc-200"
                }`}
              >
                <GitBranch className="h-3 w-3" />
                Diagram
              </button>
              <button
                type="button"
                onClick={() => setInspectorTab("trace")}
                className={`inline-flex items-center gap-1 rounded-md px-2.5 py-1 cursor-pointer ${
                  inspectorTab === "trace"
                    ? "bg-sky-500/15 text-sky-200 font-medium"
                    : "text-zinc-400 hover:text-zinc-200"
                }`}
              >
                <Terminal className="h-3 w-3" />
                Trace ({traces.length})
              </button>
              <button
                type="button"
                onClick={() => setInspectorTab("context")}
                className={`inline-flex items-center gap-1 rounded-md px-2.5 py-1 cursor-pointer ${
                  inspectorTab === "context"
                    ? "bg-sky-500/15 text-sky-200 font-medium"
                    : "text-zinc-400 hover:text-zinc-200"
                }`}
              >
                <Braces className="h-3 w-3" />
                Context
              </button>
            </div>
          </div>

          {inspectorTab === "diagram" && (
            <div className="flex-1 overflow-y-auto pr-1">
              {diagram ? (
                <MermaidDiagram source={diagram} activeStates={snapshot?.active_states} />
              ) : (
                <p className="text-xs text-zinc-500">No diagram loaded.</p>
              )}
            </div>
          )}

          {inspectorTab === "trace" && (
            <div className="flex-1 space-y-2 overflow-y-auto pr-1 text-xs">
              {traces.map((rec) => (
                <div
                  key={rec.seq}
                  className="rounded-lg border border-zinc-800/80 bg-zinc-900/60 px-2.5 py-2 space-y-1"
                >
                  <div className="flex items-center justify-between text-[10px] font-mono text-zinc-400">
                    <span className="text-sky-400">{rec.kind}</span>
                    <span>#{rec.seq}</span>
                  </div>
                  {rec.kind === "transition" && (
                    <div className="font-mono text-[11px] text-zinc-200">
                      {Array.isArray(rec.from) ? rec.from.join(",") : ""} →{" "}
                      <span className="text-emerald-300">
                        {Array.isArray(rec.to) ? rec.to.join(",") : ""}
                      </span>
                    </div>
                  )}
                  {rec.kind === "service_progress" && (
                    <div className="text-[11px] text-zinc-300 whitespace-pre-wrap break-words">
                      <span className="text-indigo-400 mr-1">[{String(rec.stream ?? "log")}]</span>
                      {String(rec.chunk ?? "")}
                    </div>
                  )}
                  {rec.kind === "error" && (
                    <div className="text-[11px] text-rose-300">{String(rec.message ?? "")}</div>
                  )}
                </div>
              ))}
              {traces.length === 0 && (
                <p className="text-xs text-zinc-500">No trace events yet.</p>
              )}
              <div ref={traceEndRef} />
            </div>
          )}

          {inspectorTab === "context" && (
            <div className="flex-1 overflow-y-auto pr-1">
              {snapshot ? (
                <pre className="rounded-xl border border-zinc-800 bg-zinc-900/70 p-3.5 text-xs text-zinc-200 whitespace-pre-wrap break-words overflow-x-auto">
                  {JSON.stringify(snapshot.context, null, 2)}
                </pre>
              ) : (
                <p className="text-xs text-zinc-500">Select or start an instance to inspect context.</p>
              )}
            </div>
          )}
        </aside>
      </div>
    </div>
  );
}
