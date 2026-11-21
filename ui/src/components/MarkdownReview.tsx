import { Fragment, type ReactNode, useMemo, useState } from "react";
import { useActions, useStateStore } from "@json-render/react";
import {
  AlertTriangle,
  Check,
  Info,
  MessageSquare,
  MessageSquarePlus,
  RotateCcw,
  Trash2,
} from "lucide-react";

export interface AnchoredComment {
  line?: number;
  heading: string;
  quote: string;
  comment: string;
}

export interface AgentNote {
  line?: number;
  heading?: string;
  severity?: string;
  note?: string;
  text?: string;
}

interface MdLine {
  lineNum: number;
  heading: string;
  isHeadingLine: boolean;
  level: number;
  raw: string;
  inCodeFence: boolean;
}

function parseMdLines(md: string): MdLine[] {
  const rawLines = md.split(/\r?\n/);
  const out: MdLine[] = [];
  let currentHeading = "Overview";
  let inFence = false;

  rawLines.forEach((line, idx) => {
    const trimmed = line.trim();
    if (trimmed.startsWith("```")) {
      inFence = !inFence;
      out.push({
        lineNum: idx + 1,
        heading: currentHeading,
        isHeadingLine: false,
        level: 0,
        raw: line,
        inCodeFence: true,
      });
      return;
    }

    const match = !inFence ? /^(#{1,4})\s+(.+)$/.exec(trimmed) : null;
    if (match) {
      currentHeading = match[2].trim();
      out.push({
        lineNum: idx + 1,
        heading: currentHeading,
        isHeadingLine: true,
        level: match[1].length,
        raw: line,
        inCodeFence: false,
      });
    } else {
      out.push({
        lineNum: idx + 1,
        heading: currentHeading,
        isHeadingLine: false,
        level: 0,
        raw: line,
        inCodeFence: inFence,
      });
    }
  });

  return out;
}

function normalizeNotes(raw: unknown): AgentNote[] {
  if (!Array.isArray(raw)) return [];
  return raw.map((item) => {
    if (typeof item === "string") return { note: item };
    if (item && typeof item === "object") return item as AgentNote;
    return { note: String(item) };
  });
}

function normalizeComments(raw: unknown): AnchoredComment[] {
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (item): item is AnchoredComment =>
      Boolean(item && typeof item === "object" && "comment" in item),
  );
}

function commentMatchesLine(c: AnchoredComment, ml: MdLine): boolean {
  if (typeof c.line === "number") return c.line === ml.lineNum;
  return ml.isHeadingLine && c.heading.toLowerCase() === ml.heading.toLowerCase();
}

function notesForLine(notes: AgentNote[], ml: MdLine): AgentNote[] {
  return notes.filter((n) => {
    if (typeof n.line === "number") return n.line === ml.lineNum;
    if (!n.heading || n.heading === "*") return ml.lineNum === 1;
    return ml.isHeadingLine && n.heading.toLowerCase() === ml.heading.toLowerCase();
  });
}

/** Renders inline Markdown (`code`, **bold**) as clean React nodes. */
export function renderInlineMarkdown(text: string): ReactNode {
  const tokens: ReactNode[] = [];
  const regex = /(`[^`]+`|\*\*[^*]+\*\*)/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  let key = 0;

  while ((match = regex.exec(text)) !== null) {
    if (match.index > lastIndex) {
      tokens.push(<Fragment key={key++}>{text.slice(lastIndex, match.index)}</Fragment>);
    }
    const token = match[0];
    if (token.startsWith("`") && token.endsWith("`")) {
      tokens.push(
        <code
          key={key++}
          className="rounded bg-zinc-800/90 border border-zinc-700/60 px-1.5 py-0.5 font-mono text-[12px] text-sky-300"
        >
          {token.slice(1, -1)}
        </code>,
      );
    } else if (token.startsWith("**") && token.endsWith("**")) {
      tokens.push(
        <strong key={key++} className="font-semibold text-zinc-50">
          {token.slice(2, -2)}
        </strong>,
      );
    }
    lastIndex = regex.lastIndex;
  }

  if (lastIndex < text.length) {
    tokens.push(<Fragment key={key++}>{text.slice(lastIndex)}</Fragment>);
  }
  return tokens.length > 0 ? tokens : text;
}

function renderMdLineContent(ml: MdLine): ReactNode {
  if (ml.inCodeFence) {
    return (
      <span className="font-mono text-xs text-sky-200 bg-zinc-800/70 px-1.5 py-0.5 rounded">
        {ml.raw || " "}
      </span>
    );
  }
  const trimmed = ml.raw.trimStart();
  if (!trimmed) return <span className="inline-block h-3" />;

  if (ml.isHeadingLine) {
    const headingText = trimmed.replace(/^#{1,4}\s+/, "");
    if (ml.level === 1) {
      return (
        <span className="text-base font-bold text-zinc-50 tracking-tight">
          {renderInlineMarkdown(headingText)}
        </span>
      );
    }
    if (ml.level === 2) {
      return (
        <span className="text-sm font-semibold text-sky-300 tracking-tight">
          {renderInlineMarkdown(headingText)}
        </span>
      );
    }
    return (
      <span className="text-xs font-semibold text-zinc-300 uppercase tracking-wide">
        {renderInlineMarkdown(headingText)}
      </span>
    );
  }

  const bulletMatch = /^([-*])\s+(.*)$/.exec(trimmed);
  if (bulletMatch) {
    return (
      <span className="inline-flex items-start gap-2 pl-2 text-sm text-zinc-200 leading-relaxed">
        <span className="text-zinc-500 select-none mt-0.5">•</span>
        <span>{renderInlineMarkdown(bulletMatch[2])}</span>
      </span>
    );
  }

  return (
    <span className="text-sm text-zinc-200 leading-relaxed">
      {renderInlineMarkdown(ml.raw)}
    </span>
  );
}

export interface MarkdownReviewProps {
  title: string;
  markdown: unknown;
  agent_notes?: unknown;
  comments_path?: string;
  approve_event: string;
  revise_event: string;
  approve_label?: string;
  revise_label?: string;
}

export function MarkdownReviewView({ props }: { props: MarkdownReviewProps }) {
  const store = useStateStore();
  const { execute } = useActions();
  const commentsPath = props.comments_path || "/draft_comments";

  const markdownText =
    typeof props.markdown === "string" ? props.markdown : String(props.markdown ?? "");
  const mdLines = useMemo(() => parseMdLines(markdownText), [markdownText]);
  const agentNotes = useMemo(() => normalizeNotes(props.agent_notes), [props.agent_notes]);
  const comments = normalizeComments(store.get(commentsPath));

  const [activeLineNum, setActiveLineNum] = useState<number | null>(null);
  const [editing, setEditing] = useState(false);
  const [draftText, setDraftText] = useState("");
  const [busy, setBusy] = useState(false);

  const activeLine = mdLines.find((ml) => ml.lineNum === activeLineNum) ?? null;
  const activeNotes = activeLine ? notesForLine(agentNotes, activeLine) : [];
  const activeCommentIdx = activeLine
    ? comments.findIndex((c) => commentMatchesLine(c, activeLine))
    : -1;
  const activeComment = activeCommentIdx >= 0 ? comments[activeCommentIdx] : null;

  const selectLine = (ml: MdLine, openEditor: boolean) => {
    setActiveLineNum(ml.lineNum);
    const idx = comments.findIndex((c) => commentMatchesLine(c, ml));
    if (openEditor) {
      setDraftText(idx >= 0 ? comments[idx].comment : "");
      setEditing(true);
    } else {
      setEditing(false);
    }
  };

  const saveComment = () => {
    if (!activeLine) return;
    const trimmed = draftText.trim();
    if (!trimmed) {
      if (activeCommentIdx >= 0) {
        const next = comments.filter((_, i) => i !== activeCommentIdx);
        store.set(commentsPath, next);
      }
      setEditing(false);
      return;
    }

    const entry: AnchoredComment = {
      line: activeLine.lineNum,
      heading: activeLine.heading,
      quote: activeLine.raw.trim(),
      comment: trimmed,
    };
    const next =
      activeCommentIdx >= 0
        ? comments.map((c, i) => (i === activeCommentIdx ? entry : c))
        : [...comments, entry];
    store.set(commentsPath, next);
    setEditing(false);
  };

  const deleteActiveComment = () => {
    if (activeCommentIdx < 0) return;
    const next = comments.filter((_, i) => i !== activeCommentIdx);
    store.set(commentsPath, next);
    setEditing(false);
    setDraftText("");
  };

  const dispatchReview = async (eventName: string) => {
    setBusy(true);
    try {
      await execute({
        action: "send_event",
        params: {
          event: eventName,
          payload: { comments },
        },
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900/80 shadow-lg overflow-hidden">
      {/* Header */}
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-zinc-800 px-4 py-3 bg-zinc-900">
        <div className="flex items-center gap-2.5">
          <h2 className="text-sm font-semibold text-zinc-100">{props.title}</h2>
          <span className="text-[11px] font-mono text-zinc-500">
            {mdLines.length} lines
          </span>
          {comments.length > 0 && (
            <span className="inline-flex items-center gap-1 rounded-full border border-amber-500/40 bg-amber-500/15 px-2 py-0.5 text-[11px] font-medium text-amber-200">
              <MessageSquare className="h-3 w-3" />
              {comments.length} comment{comments.length === 1 ? "" : "s"}
            </span>
          )}
        </div>
        <span className="text-[11px] text-zinc-500">
          Click any line to inspect or comment
        </span>
      </div>

      {/* Document with per-line gutter */}
      <div className="py-2 font-sans">
        {mdLines.map((ml) => {
          const lineNotes = notesForLine(agentNotes, ml);
          const hasNote = lineNotes.length > 0;
          const isWarnNote = lineNotes.some(
            (n) => (n.severity ?? "").toLowerCase() === "warning",
          );
          const hasComment = comments.some((c) => commentMatchesLine(c, ml));
          const isSelected = activeLineNum === ml.lineNum;

          return (
            <div
              key={ml.lineNum}
              onClick={() => selectLine(ml, !hasNote || hasComment)}
              className={`group flex items-start gap-2 px-3 py-1 transition-colors cursor-pointer ${
                isSelected
                  ? "bg-sky-500/12 border-l-2 border-sky-400 pl-[10px]"
                  : "hover:bg-zinc-800/50 border-l-2 border-transparent pl-[10px]"
              }`}
            >
              {/* Line number + gutter indicators */}
              <div className="flex items-center gap-1.5 w-14 shrink-0 pt-0.5 select-none">
                <span
                  className={`w-6 text-right font-mono text-[11px] ${
                    isSelected ? "text-sky-300 font-semibold" : "text-zinc-600"
                  }`}
                >
                  {ml.lineNum}
                </span>
                <span className="w-6 flex items-center gap-0.5">
                  {hasComment && (
                    <span
                      title="Has comment — click to edit"
                      className="text-amber-400"
                    >
                      <MessageSquare className="h-3.5 w-3.5 fill-amber-400/20" />
                    </span>
                  )}
                  {hasNote && (
                    <span
                      title={`Agent note (${isWarnNote ? "warning" : "info"})`}
                      className={isWarnNote ? "text-amber-400" : "text-sky-400"}
                    >
                      {isWarnNote ? (
                        <AlertTriangle className="h-3.5 w-3.5" />
                      ) : (
                        <Info className="h-3.5 w-3.5" />
                      )}
                    </span>
                  )}
                </span>
              </div>

              {/* Rendered Markdown line */}
              <div className="flex-1 min-w-0 break-words">
                {renderMdLineContent(ml)}
              </div>

              {/* Hover quick comment trigger */}
              <button
                type="button"
                onClick={(e) => {
                  e.stopPropagation();
                  selectLine(ml, true);
                }}
                className="opacity-0 group-hover:opacity-100 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-zinc-400 hover:bg-zinc-800 hover:text-sky-300 shrink-0 cursor-pointer"
                title={hasComment ? "Edit comment" : "Add comment"}
              >
                <MessageSquarePlus className="h-3.5 w-3.5" />
              </button>
            </div>
          );
        })}
      </div>

      {/* Bottom Inspector & Comment Editor Drawer (only shown when a line is selected) */}
      {activeLine && (
        <div className="border-t border-zinc-800 bg-zinc-950/80 px-4 py-3 space-y-2.5">
          <div className="flex items-center justify-between gap-2 text-xs">
            <span className="font-mono text-zinc-400">
              L{activeLine.lineNum} ·{" "}
              <span className="text-zinc-200">{activeLine.heading}</span>
            </span>
            <div className="flex items-center gap-2">
              {!editing && (
                <button
                  type="button"
                  onClick={() => {
                    setDraftText(activeComment?.comment ?? "");
                    setEditing(true);
                  }}
                  className="inline-flex items-center gap-1 rounded-md border border-zinc-700 bg-zinc-800 px-2 py-1 text-[11px] text-zinc-200 hover:border-sky-500/50 hover:text-sky-200 cursor-pointer"
                >
                  <MessageSquarePlus className="h-3 w-3" />
                  {activeComment ? "Edit Comment" : "Add Comment"}
                </button>
              )}
              <button
                type="button"
                onClick={() => {
                  setActiveLineNum(null);
                  setEditing(false);
                }}
                className="text-[11px] text-zinc-500 hover:text-zinc-300 cursor-pointer"
              >
                Close
              </button>
            </div>
          </div>

          {/* Agent Notes anchored to this line */}
          {activeNotes.map((n, idx) => {
            const isWarn = (n.severity ?? "").toLowerCase() === "warning";
            return (
              <div
                key={idx}
                className={`flex items-start gap-2 rounded-lg border px-3 py-2 text-xs ${
                  isWarn
                    ? "border-amber-500/40 bg-amber-950/30 text-amber-200"
                    : "border-sky-500/35 bg-sky-950/25 text-sky-200"
                }`}
              >
                {isWarn ? (
                  <AlertTriangle className="h-3.5 w-3.5 mt-0.5 shrink-0 text-amber-400" />
                ) : (
                  <Info className="h-3.5 w-3.5 mt-0.5 shrink-0 text-sky-400" />
                )}
                <div className="leading-relaxed">
                  <span className="font-semibold uppercase tracking-wide text-[10px] mr-1.5 opacity-80">
                    Agent Note{n.severity ? ` (${n.severity})` : ""}:
                  </span>
                  {renderInlineMarkdown(n.note || n.text || "")}
                </div>
              </div>
            );
          })}

          {/* Existing comment preview (when not actively editing) */}
          {!editing && activeComment && (
            <div className="flex items-start justify-between gap-2 rounded-lg border border-amber-500/35 bg-amber-950/25 px-3 py-2 text-xs text-amber-100">
              <div className="whitespace-pre-wrap leading-relaxed">
                {activeComment.comment}
              </div>
              <button
                type="button"
                onClick={deleteActiveComment}
                className="text-zinc-400 hover:text-rose-400 p-0.5 shrink-0 cursor-pointer"
                title="Delete comment"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          )}

          {/* Multiline Comment Editor */}
          {editing && (
            <div className="space-y-2">
              {activeLine.raw.trim() && (
                <div className="text-[11px] font-mono text-zinc-500 border-l-2 border-zinc-700 pl-2 truncate">
                  {activeLine.raw.trim()}
                </div>
              )}
              <textarea
                rows={3}
                autoFocus
                value={draftText}
                onChange={(e) => setDraftText(e.target.value)}
                onKeyDown={(e) => {
                  if ((e.metaKey || e.ctrlKey) && (e.key === "Enter" || e.key.toLowerCase() === "s")) {
                    e.preventDefault();
                    saveComment();
                  } else if (e.key === "Escape") {
                    setEditing(false);
                  }
                }}
                placeholder="Write revision feedback (Enter for newline, Cmd/Ctrl+Enter to save)..."
                className="w-full rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-2 text-xs text-zinc-100 focus:border-sky-500 focus:outline-none"
              />
              <div className="flex items-center justify-between">
                <div>
                  {activeComment && (
                    <button
                      type="button"
                      onClick={deleteActiveComment}
                      className="inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs text-rose-400 hover:bg-rose-950/40 cursor-pointer"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                      Delete
                    </button>
                  )}
                </div>
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    onClick={() => setEditing(false)}
                    className="rounded-md border border-zinc-700 bg-zinc-800 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-700 cursor-pointer"
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    onClick={saveComment}
                    className="rounded-md bg-sky-600 px-3 py-1 text-xs font-medium text-white hover:bg-sky-500 cursor-pointer"
                  >
                    Save
                  </button>
                </div>
              </div>
            </div>
          )}
        </div>
      )}

      {/* Action Bar */}
      <div className="flex flex-wrap items-center justify-end gap-2 border-t border-zinc-800 bg-zinc-900/90 px-4 py-3">
        <button
          type="button"
          disabled={busy}
          onClick={() => dispatchReview(props.revise_event)}
          className="inline-flex items-center gap-1.5 rounded-lg border border-sky-500/40 bg-sky-500/15 px-3.5 py-1.5 text-xs font-medium text-sky-200 hover:bg-sky-500/25 disabled:opacity-50 cursor-pointer"
        >
          <RotateCcw className="h-3.5 w-3.5" />
          {props.revise_label || "Request Changes"}
          {comments.length > 0 && (
            <span className="ml-0.5 rounded-full bg-sky-500/30 px-1.5 py-0.5 text-[10px]">
              {comments.length}
            </span>
          )}
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => dispatchReview(props.approve_event)}
          className="inline-flex items-center gap-1.5 rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:opacity-50 cursor-pointer"
        >
          <Check className="h-3.5 w-3.5" />
          {props.approve_label || "Approve"}
        </button>
      </div>
    </div>
  );
}
