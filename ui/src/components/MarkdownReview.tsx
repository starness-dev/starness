import { useMemo, useState } from "react";
import { useActions, useStateStore } from "@json-render/react";
import { Check, MessageSquarePlus, RotateCcw, Sparkles, Trash2 } from "lucide-react";

export interface AnchoredComment {
  heading: string;
  quote: string;
  comment: string;
}

export interface AgentNote {
  heading?: string;
  severity?: string;
  note?: string;
  text?: string;
}

interface MarkdownSection {
  heading: string;
  level: number;
  lines: string[];
}

function parseMarkdownSections(md: string): MarkdownSection[] {
  const rawLines = md.split(/\r?\n/);
  const sections: MarkdownSection[] = [];
  let current: MarkdownSection = { heading: "Overview", level: 1, lines: [] };

  for (const line of rawLines) {
    const match = /^(#{1,4})\s+(.+)$/.exec(line.trim());
    if (match) {
      if (current.lines.some((l) => l.trim().length > 0) || sections.length > 0) {
        sections.push(current);
      }
      current = {
        heading: match[2].trim(),
        level: match[1].length,
        lines: [],
      };
    } else {
      current.lines.push(line);
    }
  }
  sections.push(current);
  return sections;
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

  const markdownText = typeof props.markdown === "string" ? props.markdown : String(props.markdown ?? "");
  const sections = useMemo(() => parseMarkdownSections(markdownText), [markdownText]);
  const agentNotes = useMemo(() => normalizeNotes(props.agent_notes), [props.agent_notes]);

  const comments = normalizeComments(store.get(commentsPath));

  const [draftHeading, setDraftHeading] = useState<string | null>(null);
  const [draftQuote, setDraftQuote] = useState("");
  const [draftText, setDraftText] = useState("");
  const [busy, setBusy] = useState(false);

  const openCommentFor = (heading: string, quote = "") => {
    setDraftHeading(heading);
    setDraftQuote(quote);
    setDraftText("");
  };

  const addComment = () => {
    if (!draftHeading || !draftText.trim()) return;
    const next: AnchoredComment[] = [
      ...comments,
      {
        heading: draftHeading,
        quote: draftQuote.trim(),
        comment: draftText.trim(),
      },
    ];
    store.set(commentsPath, next);
    setDraftHeading(null);
    setDraftQuote("");
    setDraftText("");
  };

  const removeComment = (idx: number) => {
    const next = comments.filter((_, i) => i !== idx);
    store.set(commentsPath, next);
  };

  const dispatchReview = async (eventName: string) => {
    setBusy(true);
    try {
      await execute({
        action: "send_event",
        params: {
          event: eventName,
          payload: {
            comments,
          },
        },
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900/80 shadow-lg overflow-hidden">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-zinc-800 px-5 py-3.5 bg-zinc-900">
        <div>
          <h2 className="text-base font-semibold text-zinc-100">{props.title}</h2>
          <p className="text-xs text-zinc-400">
            Click <span className="text-zinc-200">+ Comment</span> on any section or line to attach anchored feedback.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={() => dispatchReview(props.revise_event)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-amber-500/40 bg-amber-500/15 px-3 py-1.5 text-xs font-medium text-amber-200 hover:bg-amber-500/25 disabled:opacity-50 cursor-pointer"
          >
            <RotateCcw className="h-3.5 w-3.5" />
            {props.revise_label || "Request Changes"}
            {comments.length > 0 && (
              <span className="ml-1 rounded-full bg-amber-500/30 px-1.5 py-0.5 text-[10px]">
                {comments.length}
              </span>
            )}
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => dispatchReview(props.approve_event)}
            className="inline-flex items-center gap-1.5 rounded-lg bg-emerald-600 px-3.5 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:opacity-50 cursor-pointer"
          >
            <Check className="h-3.5 w-3.5" />
            {props.approve_label || "Approve"}
          </button>
        </div>
      </div>

      <div className="divide-y divide-zinc-800/80">
        {sections.map((sec, sIdx) => {
          const matchedNotes = agentNotes.filter(
            (n) =>
              !n.heading ||
              n.heading.toLowerCase() === sec.heading.toLowerCase() ||
              (sIdx === 0 && n.heading === "*"),
          );
          const secComments = comments
            .map((c, idx) => ({ c, idx }))
            .filter(({ c }) => c.heading === sec.heading);

          return (
            <div key={`${sec.heading}-${sIdx}`} className="p-5 space-y-3">
              <div className="flex items-center justify-between gap-2">
                <h3 className="text-sm font-semibold text-sky-300 tracking-tight">
                  {"#".repeat(Math.min(sec.level, 3))} {sec.heading}
                </h3>
                <button
                  type="button"
                  onClick={() => openCommentFor(sec.heading)}
                  className="inline-flex items-center gap-1 rounded-md border border-zinc-700 bg-zinc-800/80 px-2 py-1 text-[11px] text-zinc-300 hover:border-sky-500/50 hover:text-sky-200 cursor-pointer"
                >
                  <MessageSquarePlus className="h-3 w-3" />
                  Comment
                </button>
              </div>

              <div className="space-y-1 text-sm text-zinc-200 leading-relaxed">
                {sec.lines.map((line, lIdx) => {
                  const trimmed = line.trim();
                  if (!trimmed) return <div key={lIdx} className="h-1.5" />;
                  const isBullet = /^[-*]\s+/.test(trimmed);
                  return (
                    <div
                      key={lIdx}
                      onClick={() => openCommentFor(sec.heading, trimmed)}
                      title="Click line to quote in a comment"
                      className={`group flex items-start gap-2 rounded px-1.5 py-0.5 hover:bg-zinc-800/70 cursor-pointer ${
                        isBullet ? "pl-4" : ""
                      }`}
                    >
                      <span className="flex-1 whitespace-pre-wrap">{line}</span>
                      <span className="opacity-0 group-hover:opacity-100 text-[10px] text-sky-400 shrink-0">
                        + quote
                      </span>
                    </div>
                  );
                })}
              </div>

              {matchedNotes.length > 0 && (
                <div className="space-y-1.5 pt-1">
                  {matchedNotes.map((n, nIdx) => (
                    <div
                      key={nIdx}
                      className="flex items-start gap-2 rounded-lg border border-indigo-500/30 bg-indigo-950/30 px-3 py-2 text-xs text-indigo-200"
                    >
                      <Sparkles className="h-3.5 w-3.5 mt-0.5 shrink-0 text-indigo-400" />
                      <div>
                        <span className="font-semibold uppercase tracking-wide text-[10px] text-indigo-300 mr-1.5">
                          Agent Note{n.severity ? ` (${n.severity})` : ""}:
                        </span>
                        <span>{n.note || n.text}</span>
                      </div>
                    </div>
                  ))}
                </div>
              )}

              {secComments.length > 0 && (
                <div className="space-y-1.5 pt-1">
                  {secComments.map(({ c, idx }) => (
                    <div
                      key={idx}
                      className="flex items-start justify-between gap-2 rounded-lg border border-amber-500/30 bg-amber-950/25 px-3 py-2 text-xs text-amber-100"
                    >
                      <div className="space-y-0.5">
                        {c.quote && (
                          <div className="text-[11px] italic text-amber-300/80 border-l-2 border-amber-500/50 pl-2">
                            “{c.quote}”
                          </div>
                        )}
                        <div>{c.comment}</div>
                      </div>
                      <button
                        type="button"
                        onClick={() => removeComment(idx)}
                        className="text-zinc-400 hover:text-rose-400 p-0.5 cursor-pointer"
                        title="Remove comment"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  ))}
                </div>
              )}

              {draftHeading === sec.heading && (
                <div className="rounded-lg border border-sky-500/40 bg-zinc-950/80 p-3 space-y-2">
                  <div className="flex items-center justify-between text-xs text-sky-300">
                    <span>Comment on “{sec.heading}”</span>
                    <button
                      type="button"
                      onClick={() => setDraftHeading(null)}
                      className="text-zinc-400 hover:text-zinc-200 cursor-pointer"
                    >
                      Cancel
                    </button>
                  </div>
                  {draftQuote && (
                    <div className="text-[11px] italic text-zinc-400 border-l-2 border-sky-500/50 pl-2">
                      “{draftQuote}”
                    </div>
                  )}
                  <textarea
                    rows={2}
                    value={draftText}
                    onChange={(e) => setDraftText(e.target.value)}
                    placeholder="Write specific revision guidance..."
                    className="w-full rounded-md border border-zinc-700 bg-zinc-900 px-2.5 py-1.5 text-xs text-zinc-100 focus:border-sky-500 focus:outline-none"
                  />
                  <div className="flex justify-end">
                    <button
                      type="button"
                      onClick={addComment}
                      disabled={!draftText.trim()}
                      className="rounded-md bg-sky-600 px-3 py-1 text-xs font-medium text-white hover:bg-sky-500 disabled:opacity-40 cursor-pointer"
                    >
                      Add Comment
                    </button>
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
