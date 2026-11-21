import type { Spec } from "@json-render/core";

export type InstanceStatus = "active" | "waiting" | "completed" | "failed" | "cancelled";

export interface MachineSummary {
  id: string;
  description: string | null;
  initial: string;
  completable: boolean;
  default_context: Record<string, unknown>;
  instance_counts: Record<string, number>;
  states_with_ui?: string[];
  initial_ui?: Spec | null;
  initial_allowed_events?: string[];
}

export interface InstanceSnapshot {
  machine: string;
  id: string;
  status: InstanceStatus;
  done: boolean;
  active_states: string[];
  history: Record<string, string>;
  context: Record<string, unknown>;
  output: unknown;
  error: string | null;
  created_at_ms: number;
  updated_at_ms: number;
  allowed_events?: string[];
  ui?: Spec | null;
}

export interface TraceRecord {
  seq: number;
  timestamp_ms?: number;
  kind: string;
  [key: string]: unknown;
}

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${text}`);
  }
  return text ? (JSON.parse(text) as T) : ({} as T);
}

export async function listMachines(): Promise<MachineSummary[]> {
  const data = await fetchJson<{ machines: MachineSummary[] }>("/machines");
  return data.machines ?? [];
}

export async function getDiagram(machineId: string): Promise<string> {
  const res = await fetch(`/machines/${encodeURIComponent(machineId)}/diagram`);
  if (!res.ok) return "";
  return res.text();
}

export async function listInstances(machineId: string): Promise<InstanceSnapshot[]> {
  const data = await fetchJson<{ instances: InstanceSnapshot[] }>(
    `/machines/${encodeURIComponent(machineId)}/instances`,
  );
  return data.instances ?? [];
}

export async function createInstance(
  machineId: string,
  context?: Record<string, unknown>,
): Promise<InstanceSnapshot> {
  return fetchJson<InstanceSnapshot>(
    `/machines/${encodeURIComponent(machineId)}/instances`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(context ? { context } : {}),
    },
  );
}

export async function getInstance(
  machineId: string,
  instanceId: string,
): Promise<InstanceSnapshot> {
  return fetchJson<InstanceSnapshot>(
    `/machines/${encodeURIComponent(machineId)}/instances/${encodeURIComponent(instanceId)}`,
  );
}

export async function sendEvent(
  machineId: string,
  instanceId: string,
  eventType: string,
  payload?: Record<string, unknown>,
): Promise<InstanceSnapshot> {
  const body = { type: eventType, ...(payload ?? {}) };
  return fetchJson<InstanceSnapshot>(
    `/machines/${encodeURIComponent(machineId)}/instances/${encodeURIComponent(instanceId)}/events`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    },
  );
}

export async function cancelInstance(
  machineId: string,
  instanceId: string,
): Promise<void> {
  await fetchJson(
    `/machines/${encodeURIComponent(machineId)}/instances/${encodeURIComponent(instanceId)}`,
    { method: "DELETE" },
  );
}

export function subscribeInstanceTrace(
  machineId: string,
  instanceId: string,
  onRecord: (rec: TraceRecord) => void,
): () => void {
  const url = `/machines/${encodeURIComponent(machineId)}/instances/${encodeURIComponent(instanceId)}/trace`;
  const es = new EventSource(url);
  es.onmessage = (ev) => {
    try {
      const parsed = JSON.parse(ev.data) as TraceRecord;
      onRecord(parsed);
    } catch {
      // Ignore malformed SSE frames
    }
  };
  return () => es.close();
}
