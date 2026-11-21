import { useMemo, useState } from "react";
import { useActions, useStateStore } from "@json-render/react";
import { Check, FileCode, MessageSquarePlus, RotateCcw, Sparkles, Trash2 } from "lucide-react";

export interface DiffComment {
  file: string;
  line: number;
  quote: string;
  comment: string;
}

export interface DiffAgentNote {
  file?: string;
  line?: number;
  severity?: string;
  note?: string;
  text?: string;
}

interface DiffLine {
  kind: "add" | "del" | "ctx" | "hunk";
  file: string;
  lineNum: number;
  text: string;
}

interface DiffFileBlock {
  file: string;
  lines: DiffLine[];
}

function parseUnifiedDiff(raw: string): DiffFileBlock[] {
  const blocks: DiffFileBlock[] = [];
  let currentFile = "patch";
  let currentLines: DiffLine[] = [];
  let newLineNum = 0;

  const flush = () => {
    if (currentLines.length > 0) {
      blocks.push({ file: currentFile, lines: currentLines });
      currentLines = [];
    }
  };

  for (const line of raw.split(/\r?\n/)) {
    if (line.startsWith("diff --git ")) {
      flush();
      const parts = line.split(" ");
      currentFile = (parts[3] ?? parts[2] ?? "patch").replace(/^b\//, "");
      continue;
    }
    if (line.startsWith("+++ ")) {
      const nextName = line.slice(4).trim().replace(/^b\//, "");
      if (nextName && nextName !== "/dev/null") {
        if (currentLines.length > 0 && nextName !== currentFile) {
          flush();
        }
        currentFile = nextName;
      }
      continue;
    }
    if (line.startsWith("--- ") || line.startsWith("index ")) {
      continue;
    }
    if (line.startsWith("@@")) {
      const match = /\+(\d+)/.exec(line);
      newLineNum = match ? Number(match[1]) : 1;
      currentLines.push({ kind: "hunk", file: currentFile, lineNum: newLineNum, text: line });
      continue;
    }
    if (line.startsWith("+")) {
      currentLines.push({ kind: "add", file: currentFile, lineNum: newLineNum, text: line });
      newLineNum += 1;
    } else if (line.startsWith("-")) {
      currentLines.push({ kind: "del", file: currentFile, lineNum: newLineNum, text: line });
    } else if (line.length > 0 || currentLines.length > 0) {
      currentLines.push({ kind: "ctx", file: currentFile, lineNum: newLineNum, text: line });
      if (newLineNum > 0) newLineNum += 1;
    }
  }
  flush();
  return blocks;
}

export interface DiffReviewProps {
  title: string;
  diff: unknown;
  agent_notes?: unknown;
  comments_path?: string;
  approve_event: string;
  revise_event: string;
  approve_label?: string;
  revise_label?: string;
}

export function DiffReviewView({ props }: { props: DiffReviewProps }) {
  const store = useStateStore();
  const { execute } = useActions();
  const commentsPath = props.comments_path || "/draft_comments";

  const diffText = typeof props.diff === "string" ? props.diff : String(props.diff ?? "");
  const files = useMemo(() => parseUnifiedDiff(diffText), [diffText]);
  const agentNotes: DiffAgentNote[] = Array.isArray(props.agent_notes)
    ? (props.agent_notes as DiffAgentNote[])
    : [];
  const rawComments = store.get(commentsPath);
  const comments: DiffComment[] = Array.isArray(rawComments)
    ? (rawComments as DiffComment[])
    : [];

  const [draftTarget, setDraftTarget] = useState<{ file: string; line: number; quote: string } | null>(null);
  const [draftText, setDraftText] = useState("");
  const [busy, setBusy] = useState(false);

  const addComment = () => {
    if (!draftTarget || !draftText.trim()) return;
    const next: DiffComment[] = [
      ...comments,
      {
        file: draftTarget.file,
        line: draftTarget.line,
        quote: draftTarget.quote,
        comment: draftText.trim(),
      },
    ];
    store.set(commentsPath, next);
    setDraftTarget(null);
    setDraftText("");
  };

  const removeComment = (idx: number) => {
    store.set(
      commentsPath,
      comments.filter((_, i) => i !== idx),
    );
  };

  const dispatch = async (eventName: string) => {
    setBusy(true);
    try {
      await execute({
        action: "send_event",
        params: { event: eventName, payload: { comments } },
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
          <p className="text-xs text-zinc-400">Click any diff line to attach a file &amp; line-anchored comment.</p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={() => dispatch(props.revise_event)}
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
            onClick={() => dispatch(props.approve_event)}
            className="inline-flex items-center gap-1.5 rounded-lg bg-emerald-600 px-3.5 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:opacity-50 cursor-pointer"
          >
            <Check className="h-3.5 w-3.5" />
            {props.approve_label || "Approve"}
          </button>
        </div>
      </div>

      <div className="divide-y divide-zinc-800">
        {files.map((fb, fIdx) => (
          <div key={`${fb.file}-${fIdx}`} className="bg-zinc-950/60">
            <div className="flex items-center gap-2 border-b border-zinc-800/80 bg-zinc-900/70 px-4 py-2 font-mono text-xs text-sky-300">
              <FileCode className="h-3.5 w-3.5" />
              {fb.file}
            </div>
            <div className="font-mono text-xs divide-y divide-zinc-900/40">
              {fb.lines.map((dl, lIdx) => {
                const bg =
                  dl.kind === "add"
                    ? "bg-emerald-950/35 text-emerald-200"
                    : dl.kind === "del"
                      ? "bg-rose-950/35 text-rose-200"
                      : dl.kind === "hunk"
                        ? "bg-sky-950/30 text-sky-300"
                        : "text-zinc-300";
                const lineComments = comments
                  .map((c, idx) => ({ c, idx }))
                  .filter(({ c }) => c.file === fb.file && c.line === dl.lineNum);
                const lineNotes = agentNotes.filter(
                  (n) => (!n.file || n.file === fb.file) && n.line === dl.lineNum,
                );
                return (
                  <div key={lIdx}>
                    <div
                      onClick={() =>
                        dl.kind !== "hunk" &&
                        setDraftTarget({ file: fb.file, line: dl.lineNum, quote: dl.text })
                      }
                      className={`group flex items-center gap-3 px-4 py-0.5 hover:brightness-125 ${bg} ${
                        dl.kind !== "hunk" ? "cursor-pointer" : ""
                      }`}
                    >
                      <span className="w-8 shrink-0 text-right text-[10px] text-zinc-500 select-none">
                        {dl.kind === "hunk" ? "" : dl.lineNum}
                      </span>
                      <span className="flex-1 whitespace-pre-wrap break-all">{dl.text}</span>
                      {dl.kind !== "hunk" && (
                        <MessageSquarePlus className="h-3 w-3 opacity-0 group-hover:opacity-100 text-sky-400 shrink-0" />
                      )}
                    </div>
                    {lineNotes.map((n, nIdx) => (
                      <div
                        key={nIdx}
                        className="mx-4 my-1 flex items-start gap-2 rounded border border-indigo-500/30 bg-indigo-950/30 px-3 py-1.5 font-sans text-xs text-indigo-200"
                      >
                        <Sparkles className="h-3.5 w-3.5 mt-0.5 shrink-0 text-indigo-400" />
                        <span>{n.note || n.text}</span>
                      </div>
                    ))}
                    {lineComments.map(({ c, idx }) => (
                      <div
                        key={idx}
                        className="mx-4 my-1 flex items-start justify-between gap-2 rounded border border-amber-500/30 bg-amber-950/30 px-3 py-1.5 font-sans text-xs text-amber-100"
                      >
                        <span>{c.comment}</span>
                        <button
                          type="button"
                          onClick={() => removeComment(idx)}
                          className="text-zinc-400 hover:text-rose-400 cursor-pointer"
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    ))}
                    {draftTarget &&
                      draftTarget.file === fb.file &&
                      draftTarget.line === dl.lineNum &&
                      draftTarget.quote === dl.text && (
                        <div className="mx-4 my-2 rounded-lg border border-sky-500/40 bg-zinc-900 p-3 font-sans space-y-2">
                          <div className="flex items-center justify-between text-xs text-sky-300">
                            <span>
                              Comment on {fb.file}:{dl.lineNum}
                            </span>
                            <button
                              type="button"
                              onClick={() => setDraftTarget(null)}
                              className="text-zinc-400 hover:text-zinc-200 cursor-pointer"
                            >
                              Cancel
                            </button>
                          </div>
                          <textarea
                            rows={2}
                            value={draftText}
                            onChange={(e) => setDraftText(e.target.value)}
                            placeholder="Write review comment..."
                            className="w-full rounded border border-zinc-700 bg-zinc-950 px-2.5 py-1.5 text-xs text-zinc-100 focus:border-sky-500 focus:outline-none"
                          />
                          <div className="flex justify-end">
                            <button
                              type="button"
                              onClick={addComment}
                              disabled={!draftText.trim()}
                              className="rounded bg-sky-600 px-3 py-1 text-xs font-medium text-white hover:bg-sky-500 disabled:opacity-40 cursor-pointer"
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
        ))}
      </div>
    </div>
  );
}
