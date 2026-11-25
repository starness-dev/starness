import { createContext, useContext, useState } from "react";
import {
  CheckCircle2,
  Circle,
  Loader2,
  MessageSquare,
  Paperclip,
  Square,
  X,
  XCircle,
} from "lucide-react";
import type { DynamicToolUIPart } from "ai";
import { Badge } from "@/components/ui/badge";
import {
  Conversation,
  ConversationContent,
  ConversationEmptyState,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation";
import {
  Message,
  MessageContent,
  MessageResponse,
} from "@/components/ai-elements/message";
import {
  PromptInput,
  PromptInputBody,
  PromptInputButton,
  PromptInputFooter,
  PromptInputSubmit,
  PromptInputTextarea,
  PromptInputTools,
  usePromptInputAttachments,
} from "@/components/ai-elements/prompt-input";
import {
  Reasoning,
  ReasoningContent,
  ReasoningTrigger,
} from "@/components/ai-elements/reasoning";
import {
  Task,
  TaskContent,
  TaskItem,
  TaskTrigger,
} from "@/components/ai-elements/task";
import {
  Terminal,
  TerminalActions,
  TerminalContent,
  TerminalCopyButton,
  TerminalHeader,
  TerminalTitle,
} from "@/components/ai-elements/terminal";
import {
  Tool,
  ToolContent,
  ToolHeader,
  ToolInput,
  ToolOutput,
} from "@/components/ai-elements/tool";
import type { TraceRecord, UploadedFile } from "../api";

// ── Helpers ──────────────────────────────────────────────────────────────────

function formatValue(val: unknown): string {
  if (val == null) return "";
  if (typeof val === "string") return val;
  try {
    return JSON.stringify(val, null, 2);
  } catch {
    return String(val);
  }
}

export type ToolStatus = "pending" | "running" | "ok" | "error";

export function normalizeToolStatus(raw: unknown): ToolStatus {
  const s = String(raw ?? "ok").toLowerCase().trim();
  if (s === "pending" || s === "queued") return "pending";
  if (s === "running" || s === "in_progress" || s === "calling") return "running";
  if (s === "error" || s === "failed" || s === "err") return "error";
  return "ok";
}

function toAiToolState(status: ToolStatus): DynamicToolUIPart["state"] {
  switch (status) {
    case "pending":
      return "input-streaming";
    case "running":
      return "input-available";
    case "error":
      return "output-error";
    case "ok":
    default:
      return "output-available";
  }
}

export interface NormalizedToolCall {
  id?: string;
  name: string;
  status: ToolStatus;
  arguments?: unknown;
  output?: unknown;
  error?: string;
}

export interface NormalizedMessage {
  role: "user" | "assistant" | "tool" | "system";
  text: string;
  reasoning?: string;
  toolCalls: NormalizedToolCall[];
  attachments?: string[];
  toolName?: string;
  callId?: string;
}

function extractGenaiParts(content: unknown): {
  text: string[];
  reasoning: string[];
  toolCalls: NormalizedToolCall[];
  attachments: string[];
} {
  const text: string[] = [];
  const reasoning: string[] = [];
  const toolCalls: NormalizedToolCall[] = [];
  const attachments: string[] = [];

  if (typeof content === "string") {
    if (content) text.push(content);
    return { text, reasoning, toolCalls, attachments };
  }

  const parts = Array.isArray(content) ? content : [];
  for (const part of parts) {
    if (typeof part === "string") {
      if (part) text.push(part);
      continue;
    }
    if (!part || typeof part !== "object") continue;
    const obj = part as Record<string, unknown>;
    if (typeof obj.Text === "string") {
      text.push(obj.Text);
    } else if (typeof obj.Thought === "string") {
      reasoning.push(obj.Thought);
    } else if (typeof obj.Reasoning === "string") {
      reasoning.push(obj.Reasoning);
    } else if (typeof obj.ReasoningContent === "string") {
      reasoning.push(obj.ReasoningContent);
    } else if (obj.ToolCall && typeof obj.ToolCall === "object") {
      const tc = obj.ToolCall as Record<string, unknown>;
      toolCalls.push({
        id: tc.call_id ? String(tc.call_id) : undefined,
        name: String(tc.fn_name ?? tc.name ?? "tool"),
        status: "ok",
        arguments: tc.fn_arguments ?? tc.arguments ?? tc.args,
      });
    } else if (obj.Binary && typeof obj.Binary === "object") {
      const bin = obj.Binary as Record<string, unknown>;
      const label =
        (typeof bin.name === "string" && bin.name) ||
        (typeof bin.content_type === "string" && bin.content_type) ||
        "attachment";
      attachments.push(label);
    } else if (typeof obj.text === "string") {
      text.push(obj.text);
    }
  }
  return { text, reasoning, toolCalls, attachments };
}

/**
 * Normalizes `@starness/llm` (`$starness_llm_msg`, `user_msg`, `assistant_msg`,
 * `tool_result`, `tool_results_msg`) and plain chat dicts, merging `role: "tool"`
 * results back into their matching `call_id` on the preceding assistant message.
 */
export function normalizeMessages(raw: unknown): NormalizedMessage[] {
  if (!Array.isArray(raw)) return [];
  const out: NormalizedMessage[] = [];

  const attachToolResult = (
    callId: string | undefined,
    toolName: string | undefined,
    content: unknown,
  ): boolean => {
    const isErr =
      content != null &&
      typeof content === "object" &&
      !Array.isArray(content) &&
      "error" in (content as Record<string, unknown>);
    const errorStr = isErr
      ? String((content as Record<string, unknown>).error)
      : undefined;

    for (let i = out.length - 1; i >= 0; i--) {
      if (out[i].role !== "assistant") continue;
      const match = out[i].toolCalls.find(
        (tc) =>
          (callId && tc.id && tc.id === callId) ||
          (!callId && toolName && tc.name === toolName && tc.output === undefined),
      );
      if (match) {
        match.output = isErr ? undefined : content;
        match.error = errorStr;
        match.status = isErr ? "error" : "ok";
        return true;
      }
    }
    return false;
  };

  for (const item of raw) {
    if (!item || typeof item !== "object") continue;
    const obj = item as Record<string, unknown>;

    // Unwrap @starness/llm lossless `$starness_llm_msg` envelope or typed `LlmMessage.raw`
    const lossless =
      (obj.$starness_llm_msg && typeof obj.$starness_llm_msg === "object"
        ? (obj.$starness_llm_msg as Record<string, unknown>)
        : null) ??
      (obj.raw && typeof obj.raw === "object" ? (obj.raw as Record<string, unknown>) : null);
    if (lossless) {
      const inner = lossless;
      const roleRaw = String(inner.role ?? "assistant").toLowerCase();
      const role =
        roleRaw === "user"
          ? "user"
          : roleRaw === "system"
            ? "system"
            : roleRaw === "tool"
              ? "tool"
              : "assistant";
      const extracted = extractGenaiParts(inner.content);
      out.push({
        role,
        text: extracted.text.join("\n"),
        reasoning: extracted.reasoning.length > 0 ? extracted.reasoning.join("\n") : undefined,
        toolCalls: extracted.toolCalls,
        attachments: extracted.attachments.length > 0 ? extracted.attachments : undefined,
      });
      continue;
    }

    const roleRaw = String(obj.role ?? "assistant").toLowerCase();
    const role: NormalizedMessage["role"] =
      roleRaw === "user"
        ? "user"
        : roleRaw === "system"
          ? "system"
          : roleRaw === "tool"
            ? "tool"
            : "assistant";

    // Handle tool_results_msg({ role: "tool", results: [...] })
    if (role === "tool" && Array.isArray(obj.results)) {
      for (const res of obj.results) {
        if (!res || typeof res !== "object") continue;
        const r = res as Record<string, unknown>;
        const cid = r.call_id ? String(r.call_id) : undefined;
        const tname = String(r.fn_name ?? r.name ?? "tool");
        if (!attachToolResult(cid, tname, r.content)) {
          out.push({
            role: "tool",
            text: formatValue(r.content),
            toolCalls: [],
            toolName: tname,
            callId: cid,
          });
        }
      }
      continue;
    }

    // Handle single tool_result(...)
    if (role === "tool") {
      const cid = obj.call_id ? String(obj.call_id) : undefined;
      const tname = String(obj.fn_name ?? obj.name ?? "tool");
      const content = obj.content ?? obj.text;
      if (!attachToolResult(cid, tname, content)) {
        out.push({
          role: "tool",
          text: formatValue(content),
          toolCalls: [],
          toolName: tname,
          callId: cid,
        });
      }
      continue;
    }

    const text =
      typeof obj.text === "string"
        ? obj.text
        : typeof obj.content === "string"
          ? obj.content
          : obj.content != null
            ? formatValue(obj.content)
            : "";

    const reasoning =
      typeof obj.reasoning === "string"
        ? obj.reasoning
        : typeof obj.thought === "string"
          ? obj.thought
          : undefined;

    const toolCalls: NormalizedToolCall[] = [];
    const rawTcs = Array.isArray(obj.tool_calls) ? obj.tool_calls : [];
    for (const rawTc of rawTcs) {
      if (!rawTc || typeof rawTc !== "object") continue;
      const tc = rawTc as Record<string, unknown>;
      toolCalls.push({
        id: tc.id ? String(tc.id) : tc.call_id ? String(tc.call_id) : undefined,
        name: String(tc.name ?? tc.fn_name ?? "tool"),
        status: normalizeToolStatus(tc.status ?? "ok"),
        arguments: tc.arguments ?? tc.args ?? tc.fn_arguments,
        output: tc.output ?? tc.result,
        error: tc.error != null ? String(tc.error) : undefined,
      });
    }

    const attachments: string[] = [];
    if (Array.isArray(obj.attachments)) {
      for (const att of obj.attachments) {
        if (!att || typeof att !== "object") continue;
        const a = att as Record<string, unknown>;
        const label =
          (typeof a.name === "string" && a.name) ||
          (typeof a.path === "string" && a.path.split(/[\\/]/).pop()) ||
          (typeof a.url === "string" && a.url) ||
          "attachment";
        attachments.push(label);
      }
    }

    out.push({
      role,
      text,
      reasoning,
      toolCalls,
      attachments: attachments.length > 0 ? attachments : undefined,
    });
  }

  return out;
}

// ── 1. Reasoning (shadcn AI Elements <Reasoning>) ────────────────────────────

export function ReasoningView({
  title,
  content,
  durationMs,
  defaultExpanded = false,
  isStreaming = false,
}: {
  title?: string;
  content: unknown;
  durationMs?: unknown;
  defaultExpanded?: boolean;
  isStreaming?: boolean;
}) {
  const text = Array.isArray(content) ? content.map(String).join("\n") : formatValue(content);
  const durationSec =
    typeof durationMs === "number"
      ? Math.max(1, Math.round(durationMs / 1000))
      : typeof durationMs === "string" &&
          durationMs.trim() !== "" &&
          !Number.isNaN(Number(durationMs))
        ? Math.max(1, Math.round(Number(durationMs) / 1000))
        : undefined;

  return (
    <Reasoning
      defaultOpen={defaultExpanded}
      isStreaming={isStreaming}
      duration={durationSec}
      className="mb-0"
    >
      <ReasoningTrigger
        getThinkingMessage={(streaming, duration) => {
          if (streaming) return <p>{title || "Thinking…"}</p>;
          if (title && duration !== undefined) return <p>{title} ({duration}s)</p>;
          if (title) return <p>{title}</p>;
          if (duration !== undefined) return <p>Thought for {duration} seconds</p>;
          return <p>Thought for a few seconds</p>;
        }}
      />
      <ReasoningContent>{text || "(empty)"}</ReasoningContent>
    </Reasoning>
  );
}

// ── 2. ToolCall (shadcn AI Elements <Tool>) ──────────────────────────────────

export function ToolCallView({
  name,
  status = "ok",
  argumentsVal,
  output,
  error,
  defaultExpanded,
}: {
  name: string;
  status?: unknown;
  argumentsVal?: unknown;
  output?: unknown;
  error?: unknown;
  defaultExpanded?: boolean;
}) {
  const normStatus = normalizeToolStatus(error ? "error" : status);
  const aiState = toAiToolState(normStatus);
  const errorText = error != null && error !== "" ? formatValue(error) : undefined;

  return (
    <Tool defaultOpen={defaultExpanded ?? normStatus === "error"} className="mb-0">
      <ToolHeader type="dynamic-tool" toolName={name} state={aiState} />
      <ToolContent>
        {argumentsVal !== undefined && <ToolInput input={argumentsVal} />}
        {(output !== undefined || errorText) && (
          <ToolOutput output={output} errorText={errorText} />
        )}
      </ToolContent>
    </Tool>
  );
}

// ── 3. TaskList (shadcn AI Elements <Task>) ──────────────────────────────────

interface NormalizedTask {
  title: string;
  status: "pending" | "running" | "done" | "failed";
  detail?: string;
}

function normalizeTaskStatus(raw: unknown): NormalizedTask["status"] {
  const s = String(raw ?? "pending").toLowerCase().trim();
  if (s === "done" || s === "completed" || s === "ok" || s === "passed") return "done";
  if (s === "running" || s === "in_progress" || s === "active") return "running";
  if (s === "failed" || s === "error") return "failed";
  return "pending";
}

function normalizeTasks(raw: unknown): NormalizedTask[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((item): NormalizedTask | null => {
      if (typeof item === "string") {
        return { title: item, status: "pending" };
      }
      if (!item || typeof item !== "object") return null;
      const obj = item as Record<string, unknown>;
      const title = String(obj.title ?? obj.label ?? obj.text ?? obj.name ?? "");
      if (!title) return null;
      return {
        title,
        status: normalizeTaskStatus(obj.status ?? (obj.done ? "done" : "pending")),
        detail: obj.detail ? String(obj.detail) : undefined,
      };
    })
    .filter((t): t is NormalizedTask => t !== null);
}

export function TaskListView({
  title,
  tasks,
}: {
  title?: string;
  tasks: unknown;
}) {
  const items = normalizeTasks(tasks);
  const doneCount = items.filter((t) => t.status === "done").length;
  const triggerTitle =
    items.length > 0 ? `${title || "Plan"} (${doneCount}/${items.length})` : title || "Plan";

  return (
    <Task defaultOpen>
      <TaskTrigger title={triggerTitle} />
      <TaskContent>
        {items.length === 0 ? (
          <TaskItem>No tasks.</TaskItem>
        ) : (
          items.map((task, idx) => (
            <TaskItem key={idx} className="flex items-start gap-2">
              {task.status === "done" && (
                <CheckCircle2 className="size-4 text-emerald-500 shrink-0 mt-0.5" />
              )}
              {task.status === "running" && (
                <Loader2 className="size-4 text-sky-400 animate-spin shrink-0 mt-0.5" />
              )}
              {task.status === "failed" && (
                <XCircle className="size-4 text-rose-500 shrink-0 mt-0.5" />
              )}
              {task.status === "pending" && (
                <Circle className="size-4 text-muted-foreground shrink-0 mt-0.5" />
              )}
              <div className="min-w-0 flex-1">
                <span
                  className={
                    task.status === "done"
                      ? "line-through text-muted-foreground"
                      : task.status === "running"
                        ? "text-foreground font-medium"
                        : task.status === "failed"
                          ? "text-destructive font-medium"
                          : "text-foreground"
                  }
                >
                  {task.title}
                </span>
                {task.detail && (
                  <span className="ml-2 text-xs text-muted-foreground">{task.detail}</span>
                )}
              </div>
            </TaskItem>
          ))
        )}
      </TaskContent>
    </Task>
  );
}

// ── 4. Terminal (shadcn AI Elements <Terminal>) ──────────────────────────────

export function TerminalView({
  title,
  command,
  stdout,
  stderr,
  exitCode,
  isStreaming = false,
}: {
  title?: string;
  command?: string;
  stdout?: unknown;
  stderr?: unknown;
  exitCode?: unknown;
  isStreaming?: boolean;
}) {
  const outText = formatValue(stdout);
  const errText = formatValue(stderr);
  const combined = [outText, errText].filter(Boolean).join("\n") || "(no output)";
  const code =
    typeof exitCode === "number"
      ? exitCode
      : typeof exitCode === "string" && exitCode.trim() !== "" && !Number.isNaN(Number(exitCode))
        ? Number(exitCode)
        : null;

  return (
    <Terminal output={combined} isStreaming={isStreaming}>
      <TerminalHeader>
        <TerminalTitle className="font-mono text-xs truncate">
          {command ? `$ ${command}` : title || "Terminal"}
        </TerminalTitle>
        <TerminalActions className="gap-2">
          {code != null && (
            <Badge
              variant="secondary"
              className={
                code === 0
                  ? "bg-emerald-500/15 text-emerald-300 border-emerald-500/30 font-mono text-[10px]"
                  : "bg-rose-500/15 text-rose-300 border-rose-500/30 font-mono text-[10px]"
              }
            >
              exit {code}
            </Badge>
          )}
          <TerminalCopyButton />
        </TerminalActions>
      </TerminalHeader>
      <TerminalContent />
    </Terminal>
  );
}

export interface LiveTracesContextValue {
  traces: TraceRecord[];
  isActive: boolean;
}

export const LiveTracesContext = createContext<LiveTracesContextValue>({
  traces: [],
  isActive: false,
});

function extractActiveStream(traces: TraceRecord[]) {
  let startIdx = 0;
  for (let i = traces.length - 1; i >= 0; i--) {
    if (traces[i].kind === "service_finished") {
      // Latest invocation already finished; no in-flight stream
      return null;
    }
    if (traces[i].kind === "service_started" || traces[i].kind === "transition") {
      startIdx = i;
      break;
    }
  }

  const recent = traces.slice(startIdx).filter((r) => r.kind === "service_progress");
  if (recent.length === 0) return null;

  let agentText = "";
  let agentThought = "";
  let stdout = "";
  let stderr = "";
  const toolCalls: string[] = [];
  const planLines: string[] = [];

  for (const r of recent) {
    const stream = String(r.stream ?? "log");
    const chunk = String(r.chunk ?? "");
    if (stream === "agent_text") agentText += chunk;
    else if (stream === "agent_thought") agentThought += chunk;
    else if (stream === "stdout") stdout += (stdout ? "\n" : "") + chunk;
    else if (stream === "stderr") stderr += (stderr ? "\n" : "") + chunk;
    else if (stream === "tool_call") toolCalls.push(chunk);
    else if (stream === "plan") planLines.push(chunk);
  }

  if (
    !agentText &&
    !agentThought &&
    !stdout &&
    !stderr &&
    toolCalls.length === 0 &&
    planLines.length === 0
  ) {
    return null;
  }

  return { agentText, agentThought, stdout, stderr, toolCalls, planLines };
}

// ── 5. Conversation (shadcn AI Elements <Conversation> + <Message>) ──────────

export function ConversationView({
  title,
  messages,
  emptyText,
}: {
  title?: string;
  messages: unknown;
  emptyText?: string;
}) {
  const normalized = normalizeMessages(messages);
  const { traces, isActive } = useContext(LiveTracesContext);
  const live = isActive ? extractActiveStream(traces) : null;
  const hasContent = normalized.length > 0 || live !== null;

  return (
    <div className="rounded-xl border border-border bg-card/40 overflow-hidden flex flex-col">
      {title && (
        <div className="flex items-center justify-between border-b border-border px-4 py-2 bg-card/60">
          <h4 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
            {title}
          </h4>
          <span className="font-mono text-[11px] text-muted-foreground">
            {normalized.length} {normalized.length === 1 ? "turn" : "turns"}
          </span>
        </div>
      )}
      <Conversation
        className={
          !hasContent ? "h-40 flex-none" : "h-[28rem] min-h-0 flex-none"
        }
      >
        <ConversationContent className="gap-5 p-4">
          {!hasContent ? (
            <ConversationEmptyState
              icon={<MessageSquare className="size-8" />}
              title="No messages yet"
              description={emptyText || "Send a prompt to begin."}
            />
          ) : (
            <>
              {normalized.map((msg, idx) => {
                if (msg.role === "tool") {
                  return (
                    <ToolCallView
                      key={idx}
                      name={msg.toolName || "tool"}
                      status="ok"
                      output={msg.text}
                    />
                  );
                }
                return (
                  <Message key={idx} from={msg.role}>
                    <MessageContent>
                      {msg.reasoning && (
                        <ReasoningView title="Thinking" content={msg.reasoning} />
                      )}
                      {msg.attachments && msg.attachments.length > 0 && (
                        <div className="flex flex-wrap gap-1.5 mb-1">
                          {msg.attachments.map((att, aIdx) => (
                            <span
                              key={aIdx}
                              className="inline-flex items-center gap-1 rounded-md border border-border bg-muted/60 px-2 py-0.5 text-[11px] font-mono text-muted-foreground"
                            >
                              <Paperclip className="size-3 shrink-0" />
                              <span className="truncate max-w-[200px]">{att}</span>
                            </span>
                          ))}
                        </div>
                      )}
                      {msg.text && <MessageResponse>{msg.text}</MessageResponse>}
                      {msg.toolCalls.length > 0 && (
                        <div className="flex flex-col gap-2 w-full">
                          {msg.toolCalls.map((tc, tIdx) => (
                            <ToolCallView
                              key={tc.id ?? tIdx}
                              name={tc.name}
                              status={tc.status}
                              argumentsVal={tc.arguments}
                              output={tc.output}
                              error={tc.error}
                            />
                          ))}
                        </div>
                      )}
                    </MessageContent>
                  </Message>
                );
              })}

              {live && (
                <Message from="assistant">
                  <MessageContent>
                    {live.agentThought && (
                      <ReasoningView
                        title="Thinking"
                        content={live.agentThought}
                        defaultExpanded
                        isStreaming
                      />
                    )}
                    {live.agentText && <MessageResponse>{live.agentText}</MessageResponse>}
                    {live.toolCalls.length > 0 && (
                      <div className="flex flex-col gap-2 w-full">
                        {live.toolCalls.slice(-6).map((tc, idx) => (
                          <ToolCallView key={idx} name={tc} status="running" />
                        ))}
                      </div>
                    )}
                    {(live.stdout || live.stderr) && (
                      <TerminalView
                        title="Live Output"
                        stdout={live.stdout || undefined}
                        stderr={live.stderr || undefined}
                        isStreaming
                      />
                    )}
                  </MessageContent>
                </Message>
              )}
            </>
          )}
        </ConversationContent>
        <ConversationScrollButton />
      </Conversation>
    </div>
  );
}

// ── 6. PromptInput (shadcn AI Elements <PromptInput>) ────────────────────────

export type UploadFileFn = (
  file: Blob,
  filename: string,
  mime?: string,
) => Promise<UploadedFile>;

export const UploadFileContext = createContext<UploadFileFn | null>(null);

async function filePartToBlob(url: string, mediaType?: string): Promise<Blob> {
  if (url.startsWith("data:")) {
    const commaIdx = url.indexOf(",");
    if (commaIdx !== -1) {
      const header = url.slice(5, commaIdx);
      const data = url.slice(commaIdx + 1);
      const mime = mediaType || header.split(";")[0] || "application/octet-stream";
      if (header.includes(";base64")) {
        const binary = atob(data);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        return new Blob([bytes], { type: mime });
      }
      return new Blob([decodeURIComponent(data)], { type: mime });
    }
  }
  const res = await fetch(url);
  return res.blob();
}

function PromptAttachmentsBar() {
  const { files, remove } = usePromptInputAttachments();
  if (files.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 w-full px-3 pt-2.5">
      {files.map((file) => (
        <span
          key={file.id}
          data-testid="prompt-attachment-chip"
          className="inline-flex items-center gap-1.5 rounded-md border border-border bg-muted/70 px-2 py-1 text-xs text-foreground"
        >
          <Paperclip className="size-3 text-muted-foreground shrink-0" />
          <span className="truncate max-w-[180px] font-mono text-[11px]">
            {file.filename || "attachment"}
          </span>
          <button
            type="button"
            aria-label={`Remove ${file.filename || "attachment"}`}
            onClick={() => remove(file.id)}
            className="rounded p-0.5 text-muted-foreground hover:text-foreground hover:bg-muted cursor-pointer"
          >
            <X className="size-3" />
          </button>
        </span>
      ))}
    </div>
  );
}

function PromptAttachButton() {
  const { openFileDialog } = usePromptInputAttachments();
  return (
    <PromptInputButton
      variant="ghost"
      aria-label="Attach files"
      title="Attach files (or drag & drop)"
      onClick={() => openFileDialog()}
    >
      <Paperclip className="size-3.5" />
    </PromptInputButton>
  );
}

export function PromptInputView({
  label,
  placeholder,
  submitEvent,
  fieldName = "text",
  interruptEvent,
  interruptLabel,
  allowAttachments = false,
  accept,
  onUploadFile,
  onDispatch,
}: {
  label?: string;
  placeholder?: string;
  submitEvent: string;
  submitLabel?: string;
  fieldName?: string;
  interruptEvent?: string;
  interruptLabel?: string;
  allowAttachments?: boolean;
  accept?: string;
  onUploadFile?: UploadFileFn;
  onDispatch: (event: string, payload: Record<string, unknown>) => Promise<void> | void;
}) {
  const ctxUploadFile = useContext(UploadFileContext);
  const uploadFile = onUploadFile ?? ctxUploadFile;
  const [status, setStatus] = useState<"ready" | "submitted">("ready");
  const [isDragging, setIsDragging] = useState(false);
  const [uploadError, setUploadError] = useState<string | null>(null);

  return (
    <div
      className="space-y-1.5"
      onDragEnter={(e) => {
        if (!allowAttachments) return;
        if (e.dataTransfer?.types?.includes("Files")) {
          setIsDragging(true);
        }
      }}
      onDragOver={(e) => {
        if (!allowAttachments) return;
        if (e.dataTransfer?.types?.includes("Files")) {
          e.preventDefault();
          setIsDragging(true);
        }
      }}
      onDragLeave={(e) => {
        if (!allowAttachments) return;
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
          setIsDragging(false);
        }
      }}
      onDrop={() => {
        setIsDragging(false);
      }}
    >
      {label && <div className="text-xs font-medium text-muted-foreground">{label}</div>}
      <div
        className={
          isDragging
            ? "rounded-xl ring-2 ring-sky-400/80 ring-offset-2 ring-offset-background transition"
            : "transition"
        }
      >
        <PromptInput
          accept={accept}
          multiple={allowAttachments}
          onSubmit={async (msg) => {
            const trimmed = msg.text.trim();
            const rawFiles = allowAttachments ? msg.files : [];
            if (!trimmed && rawFiles.length === 0) return;

            setUploadError(null);
            setStatus("submitted");
            try {
              const uploaded: UploadedFile[] = [];
              if (rawFiles.length > 0) {
                if (!uploadFile) {
                  throw new Error("File upload handler is not configured");
                }
                for (const part of rawFiles) {
                  const blob = await filePartToBlob(part.url, part.mediaType);
                  const saved = await uploadFile(
                    blob,
                    part.filename || "upload.bin",
                    part.mediaType,
                  );
                  uploaded.push(saved);
                }
              }
              const payload: Record<string, unknown> = { [fieldName]: trimmed };
              if (uploaded.length > 0) {
                payload.files = uploaded;
              }
              await onDispatch(submitEvent, payload);
            } catch (err) {
              setUploadError(err instanceof Error ? err.message : String(err));
              throw err;
            } finally {
              setStatus("ready");
            }
          }}
        >
          {allowAttachments && <PromptAttachmentsBar />}
          <PromptInputBody>
            <PromptInputTextarea
              placeholder={
                isDragging
                  ? "Drop files to attach…"
                  : placeholder ||
                    (allowAttachments
                      ? "Message… (drag & drop files or click paperclip)"
                      : "What would you like to do?")
              }
            />
          </PromptInputBody>
          <PromptInputFooter>
            <PromptInputTools>
              {allowAttachments && <PromptAttachButton />}
              {interruptEvent && (
                <PromptInputButton
                  variant="ghost"
                  onClick={() => void onDispatch(interruptEvent, {})}
                >
                  <Square className="size-3.5 text-destructive" />
                  <span>{interruptLabel || "Stop"}</span>
                </PromptInputButton>
              )}
            </PromptInputTools>
            <PromptInputSubmit status={status} />
          </PromptInputFooter>
        </PromptInput>
      </div>
      {uploadError && (
        <div className="text-xs text-rose-400">Upload failed: {uploadError}</div>
      )}
    </div>
  );
}

// ── Live Trace Stream Renderer (for built-in Running view) ───────────────────

export function LiveServiceStream({ traces }: { traces: TraceRecord[] }) {
  let startIdx = 0;
  for (let i = traces.length - 1; i >= 0; i--) {
    if (traces[i].kind === "service_started") {
      startIdx = i;
      break;
    }
  }
  const recent = traces.slice(startIdx).filter((r) => r.kind === "service_progress");
  if (recent.length === 0) return null;

  let agentText = "";
  let agentThought = "";
  let stdout = "";
  let stderr = "";
  const toolCalls: string[] = [];
  const planLines: string[] = [];

  for (const r of recent) {
    const stream = String(r.stream ?? "log");
    const chunk = String(r.chunk ?? "");
    if (stream === "agent_text") agentText += chunk;
    else if (stream === "agent_thought") agentThought += chunk;
    else if (stream === "stdout") stdout += (stdout ? "\n" : "") + chunk;
    else if (stream === "stderr") stderr += (stderr ? "\n" : "") + chunk;
    else if (stream === "tool_call") toolCalls.push(chunk);
    else if (stream === "plan") planLines.push(chunk);
  }

  if (
    !agentText &&
    !agentThought &&
    !stdout &&
    !stderr &&
    toolCalls.length === 0 &&
    planLines.length === 0
  ) {
    return null;
  }

  return (
    <div className="space-y-3">
      {agentThought && (
        <ReasoningView title="Reasoning" content={agentThought} defaultExpanded isStreaming />
      )}
      {planLines.length > 0 && (
        <TaskListView
          title="Live Plan"
          tasks={planLines.map((line) => ({ title: line, status: "running" }))}
        />
      )}
      {toolCalls.length > 0 && (
        <div className="space-y-2">
          {toolCalls.slice(-5).map((tc, idx) => (
            <ToolCallView key={idx} name={tc} status="running" />
          ))}
        </div>
      )}
      {agentText && (
        <Message from="assistant">
          <MessageContent>
            <MessageResponse>{agentText}</MessageResponse>
          </MessageContent>
        </Message>
      )}
      {(stdout || stderr) && (
        <TerminalView
          title="Live Output"
          stdout={stdout || undefined}
          stderr={stderr || undefined}
          isStreaming
        />
      )}
    </div>
  );
}
