import { defineRegistry, useActions, useStateStore } from "@json-render/react";
import { starnessCatalog } from "./catalog";
import { MarkdownReviewView, renderInlineMarkdown } from "./components/MarkdownReview";
import { DiffReviewView } from "./components/DiffReview";

const ALERT_VARIANTS: Record<string, string> = {
  info: "border-sky-500/40 bg-sky-950/30 text-sky-200",
  success: "border-emerald-500/40 bg-emerald-950/30 text-emerald-200",
  warning: "border-amber-500/40 bg-amber-950/30 text-amber-200",
  danger: "border-rose-500/40 bg-rose-950/30 text-rose-200",
};

const BADGE_VARIANTS: Record<string, string> = {
  default: "bg-zinc-800 text-zinc-200 border-zinc-700",
  success: "bg-emerald-500/15 text-emerald-300 border-emerald-500/40",
  warning: "bg-amber-500/15 text-amber-200 border-amber-500/40",
  danger: "bg-rose-500/15 text-rose-200 border-rose-500/40",
  info: "bg-sky-500/15 text-sky-200 border-sky-500/40",
};

const BUTTON_VARIANTS: Record<string, string> = {
  primary: "bg-sky-600 hover:bg-sky-500 text-white border-transparent",
  secondary: "bg-zinc-800 hover:bg-zinc-700 text-zinc-100 border-zinc-700",
  danger: "bg-rose-600/90 hover:bg-rose-500 text-white border-transparent",
  ghost: "bg-transparent hover:bg-zinc-800 text-zinc-300 border-zinc-700/60",
};

const GAP_CLASSES: Record<string, string> = {
  sm: "gap-2",
  md: "gap-4",
  lg: "gap-6",
};

export const { registry: starnessRegistry } = defineRegistry(starnessCatalog, {
  components: {
    Stack: ({ props, children }) => {
      const dir = props.direction === "horizontal" ? "flex-row flex-wrap items-center" : "flex-col";
      const gap = GAP_CLASSES[props.gap ?? "md"] ?? "gap-4";
      return <div className={`flex ${dir} ${gap}`}>{children}</div>;
    },

    Card: ({ props, children }) => (
      <div className="rounded-xl border border-zinc-800 bg-zinc-900/75 p-4 space-y-3">
        {(props.title || props.description) && (
          <div className="space-y-0.5">
            {props.title && <h3 className="text-sm font-semibold text-zinc-100">{props.title}</h3>}
            {props.description && <p className="text-xs text-zinc-400">{props.description}</p>}
          </div>
        )}
        <div className="space-y-3">{children}</div>
      </div>
    ),

    Markdown: ({ props }) => (
      <div className="rounded-lg border border-zinc-800 bg-zinc-950/60 p-4 space-y-1.5">
        {props.title && <h4 className="text-xs font-semibold uppercase tracking-wide text-zinc-400">{props.title}</h4>}
        <div className="space-y-1 text-sm text-zinc-200 leading-relaxed">
          {String(props.content ?? "")
            .split(/\r?\n/)
            .map((line, idx) => (
              <div key={idx}>{line ? renderInlineMarkdown(line) : <span className="inline-block h-2" />}</div>
            ))}
        </div>
      </div>
    ),

    KeyValue: ({ props }) => {
      const entries: Array<[string, string]> = [];
      if (props.items && typeof props.items === "object" && !Array.isArray(props.items)) {
        for (const [k, v] of Object.entries(props.items as Record<string, unknown>)) {
          entries.push([k, typeof v === "string" ? v : JSON.stringify(v)]);
        }
      } else if (Array.isArray(props.items)) {
        for (const item of props.items) {
          if (Array.isArray(item) && item.length >= 2) {
            entries.push([String(item[0]), String(item[1])]);
          }
        }
      }
      const gridCols =
        entries.length === 3
          ? "grid-cols-1 sm:grid-cols-3"
          : entries.length >= 4
            ? "grid-cols-2 sm:grid-cols-4"
            : "grid-cols-1 sm:grid-cols-2";
      return (
        <div className="space-y-2">
          {props.title && <h4 className="text-xs font-semibold uppercase tracking-wide text-zinc-400">{props.title}</h4>}
          <dl className={`grid ${gridCols} gap-2`}>
            {entries.map(([k, v]) => (
              <div key={k} className="rounded-lg border border-zinc-800/80 bg-zinc-950/50 px-3 py-2">
                <dt className="text-[11px] font-medium text-zinc-400">{k}</dt>
                <dd className="text-xs text-zinc-100 break-all mt-0.5">{v}</dd>
              </div>
            ))}
          </dl>
        </div>
      );
    },

    Table: ({ props }) => {
      const rows = Array.isArray(props.rows) ? props.rows : [];
      return (
        <div className="space-y-2 overflow-x-auto">
          {props.title && <h4 className="text-xs font-semibold uppercase tracking-wide text-zinc-400">{props.title}</h4>}
          <table className="w-full text-left text-xs border-collapse">
            <thead>
              <tr className="border-b border-zinc-800 text-zinc-400">
                {props.columns.map((col) => (
                  <th key={col} className="py-2 pr-4 font-medium">{col}</th>
                ))}
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800/60">
              {rows.map((row, idx) => (
                <tr key={idx}>
                  {Array.isArray(row)
                    ? row.map((cell, cIdx) => (
                        <td key={cIdx} className="py-2 pr-4 text-zinc-200">{String(cell ?? "")}</td>
                      ))
                    : props.columns.map((col) => (
                        <td key={col} className="py-2 pr-4 text-zinc-200">
                          {String((row as Record<string, unknown>)?.[col] ?? "")}
                        </td>
                      ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    },

    Badge: ({ props }) => {
      const style = BADGE_VARIANTS[props.variant ?? "default"] ?? BADGE_VARIANTS.default;
      return (
        <span className={`inline-flex items-center rounded-full border px-2.5 py-0.5 text-[11px] font-medium ${style}`}>
          {String(props.label ?? "")}
        </span>
      );
    },

    Alert: ({ props }) => {
      const style = ALERT_VARIANTS[props.variant ?? "info"] ?? ALERT_VARIANTS.info;
      const msg = props.message != null ? String(props.message) : "";
      return (
        <div className={`rounded-xl border px-4 py-3 space-y-1 ${style}`}>
          <div className="text-xs font-semibold">{props.title}</div>
          {msg && <div className="text-xs opacity-90">{msg}</div>}
        </div>
      );
    },

    TextInput: ({ props }) => {
      const store = useStateStore();
      const val = String(store.get(props.bind) ?? "");
      return (
        <label className="block space-y-1">
          <span className="text-xs font-medium text-zinc-300">
            {props.label}
            {props.required && <span className="text-rose-400 ml-0.5">*</span>}
          </span>
          <input
            type="text"
            value={val}
            placeholder={props.placeholder}
            onChange={(e) => store.set(props.bind, e.target.value)}
            className="w-full rounded-lg border border-zinc-700 bg-zinc-950 px-3 py-1.5 text-sm text-zinc-100 focus:border-sky-500 focus:outline-none"
          />
        </label>
      );
    },

    Textarea: ({ props }) => {
      const store = useStateStore();
      const val = String(store.get(props.bind) ?? "");
      return (
        <label className="block space-y-1">
          <span className="text-xs font-medium text-zinc-300">
            {props.label}
            {props.required && <span className="text-rose-400 ml-0.5">*</span>}
          </span>
          <textarea
            rows={props.rows ?? 4}
            value={val}
            placeholder={props.placeholder}
            onChange={(e) => store.set(props.bind, e.target.value)}
            className="w-full rounded-lg border border-zinc-700 bg-zinc-950 px-3 py-1.5 text-sm text-zinc-100 focus:border-sky-500 focus:outline-none"
          />
        </label>
      );
    },

    Select: ({ props }) => {
      const store = useStateStore();
      const val = String(store.get(props.bind) ?? props.options[0] ?? "");
      return (
        <label className="block space-y-1">
          <span className="text-xs font-medium text-zinc-300">
            {props.label}
            {props.required && <span className="text-rose-400 ml-0.5">*</span>}
          </span>
          <select
            value={val}
            onChange={(e) => store.set(props.bind, e.target.value)}
            className="w-full rounded-lg border border-zinc-700 bg-zinc-950 px-3 py-1.5 text-sm text-zinc-100 focus:border-sky-500 focus:outline-none"
          >
            {props.options.map((opt) => (
              <option key={opt} value={opt}>{opt}</option>
            ))}
          </select>
        </label>
      );
    },

    MultiSelect: ({ props }) => {
      const store = useStateStore();
      const raw = store.get(props.bind);
      const selected: string[] = Array.isArray(raw) ? raw.map(String) : [];
      const toggle = (id: string) => {
        const next = selected.includes(id)
          ? selected.filter((x) => x !== id)
          : [...selected, id];
        store.set(props.bind, next);
      };
      return (
        <div className="space-y-1.5">
          <div className="text-xs font-medium text-zinc-300">{props.label}</div>
          <div className="space-y-1">
            {props.options.map((opt) => {
              const checked = selected.includes(opt.id);
              return (
                <label
                  key={opt.id}
                  className="flex items-start gap-2.5 rounded-lg border border-zinc-800 bg-zinc-950/60 px-3 py-2 text-xs cursor-pointer hover:border-zinc-700"
                >
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={() => toggle(opt.id)}
                    className="mt-0.5 accent-sky-500"
                  />
                  <div>
                    <div className="text-zinc-100 font-medium">{opt.label}</div>
                    {opt.description && <div className="text-zinc-400 text-[11px]">{opt.description}</div>}
                  </div>
                </label>
              );
            })}
          </div>
        </div>
      );
    },

    Confirm: ({ props }) => {
      const { execute } = useActions();
      return (
        <div className="rounded-xl border border-amber-500/30 bg-amber-950/20 p-4 space-y-3">
          <div className="space-y-1">
            <h4 className="text-sm font-semibold text-amber-200">{props.title}</h4>
            {props.message && <p className="text-xs text-zinc-300">{props.message}</p>}
          </div>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => execute({ action: "send_event", params: { event: props.confirm_event } })}
              className="rounded-lg bg-emerald-600 px-3 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 cursor-pointer"
            >
              {props.confirm_label || "Confirm"}
            </button>
            {props.cancel_event && (
              <button
                type="button"
                onClick={() => execute({ action: "send_event", params: { event: props.cancel_event } })}
                className="rounded-lg border border-zinc-700 bg-zinc-800 px-3 py-1.5 text-xs font-medium text-zinc-200 hover:bg-zinc-700 cursor-pointer"
              >
                {props.cancel_label || "Cancel"}
              </button>
            )}
          </div>
        </div>
      );
    },

    Button: ({ props }) => {
      const { execute } = useActions();
      const style = BUTTON_VARIANTS[props.variant ?? "primary"] ?? BUTTON_VARIANTS.primary;
      return (
        <button
          type="button"
          onClick={() =>
            execute({
              action: "send_event",
              params: { event: props.event, payload: props.payload ?? {} },
            })
          }
          className={`self-start inline-flex items-center justify-center rounded-lg border px-3.5 py-1.5 text-xs font-medium transition cursor-pointer ${style}`}
        >
          {props.label}
        </button>
      );
    },

    MarkdownReview: ({ props }) => <MarkdownReviewView props={props} />,
    DiffReview: ({ props }) => <DiffReviewView props={props} />,
  },
  actions: {
    send_event: async () => {},
  },
});
