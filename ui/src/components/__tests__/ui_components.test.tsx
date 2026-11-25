import "@testing-library/jest-dom/vitest";
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createStateStore } from "@json-render/core";
import { JSONUIProvider, Renderer } from "@json-render/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { starnessRegistry } from "../../registry";
import {
  ConversationView,
  LiveTracesContext,
  normalizeMessages,
  PromptInputView,
} from "../AiElements";
import { DiffReviewView } from "../DiffReview";
import { MarkdownReviewView } from "../MarkdownReview";
import type { TraceRecord } from "../../api";

// Polyfill ResizeObserver for jsdom (used by StickToBottom / Radix primitives)
if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = class ResizeObserver {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}

afterEach(() => {
  cleanup();
});

const SAMPLE_DIFF = `diff --git a/src/router.rs b/src/router.rs
--- a/src/router.rs
+++ b/src/router.rs
@@ -12,7 +12,9 @@ pub fn route_event(state: &State, ev: &Event) -> Result<State> {
     if state.is_terminal() {
-        return Ok(state.clone());
+        return Err(Error::AlreadyTerminal(state.id.clone()));
     }
+    metrics::increment_counter!("events_routed");
     state.apply(ev)
 }`;

describe("DiffReviewView", () => {
  it("anchors a comment on deleted line 13 without duplicating onto added line 13", () => {
    const store = createStateStore({ diff_comments: [] });
    render(
      <TooltipProvider>
        <JSONUIProvider
          registry={starnessRegistry}
          store={store}
          handlers={{ send_event: async () => {} }}
        >
          <DiffReviewView
            props={{
              title: "Patch Review",
              diff: SAMPLE_DIFF,
              comments_path: "/diff_comments",
              approve_event: "APPROVE",
              revise_event: "REVISE",
            }}
          />
        </JSONUIProvider>
      </TooltipProvider>,
    );

    // Click the deleted line 13 (`-        return Ok(state.clone());`)
    fireEvent.click(screen.getByText(/return Ok\(state\.clone\(\)\)/));
    const textarea = screen.getByPlaceholderText("Write review comment...");
    fireEvent.change(textarea, { target: { value: "Only on deleted line 13" } });
    fireEvent.click(screen.getByRole("button", { name: "Add Comment" }));

    // Must appear ONCE (not duplicated onto `+ return Err(...)` which also has lineNum 13)
    const rendered = screen.getAllByText("Only on deleted line 13");
    expect(rendered).toHaveLength(1);
  });
});

describe("MarkdownReviewView", () => {
  it("opens inline comment editor beneath clicked line, saves, and deletes cleanly", () => {
    const store = createStateStore({ review_comments: [] });
    render(
      <TooltipProvider>
        <JSONUIProvider
          registry={starnessRegistry}
          store={store}
          handlers={{ send_event: async () => {} }}
        >
          <MarkdownReviewView
            props={{
              title: "Plan Review",
              markdown: "# Title\nFirst line\nSecond line",
              comments_path: "/review_comments",
              approve_event: "APPROVE",
              revise_event: "REVISE",
            }}
          />
        </JSONUIProvider>
      </TooltipProvider>,
    );

    fireEvent.click(screen.getByText("First line"));
    const box = screen.getByPlaceholderText(/Write revision feedback/i);
    fireEvent.change(box, { target: { value: "Clarify this sentence" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(screen.getByText("Clarify this sentence")).toBeInTheDocument();
    expect(screen.getByText("1 comment")).toBeInTheDocument();

    // Delete comment
    fireEvent.click(screen.getByTitle("Delete comment"));
    expect(screen.queryByText("Clarify this sentence")).not.toBeInTheDocument();
  });
});

describe("ConversationView & normalizeMessages", () => {
  it("merges role:tool results into matching call_id and unwraps $starness_llm_msg ReasoningContent", () => {
    const rawMsgs = [
      { role: "user", text: "Check status" },
      {
        $starness_llm_msg: {
          role: "Assistant",
          content: [
            { ReasoningContent: "Need git status first" },
            { Text: "Running git status." },
            {
              ToolCall: {
                call_id: "call_1",
                fn_name: "bash",
                fn_arguments: { command: "git status" },
              },
            },
          ],
        },
      },
      {
        role: "tool",
        call_id: "call_1",
        fn_name: "bash",
        content: { code: 0, stdout: "clean" },
      },
    ];
    const msgs = normalizeMessages(rawMsgs);

    // Tool result merged into assistant turn -> only 2 normalized turns, not 3
    expect(msgs).toHaveLength(2);
    expect(msgs[1].reasoning).toBe("Need git status first");
    expect(msgs[1].toolCalls).toHaveLength(1);
    expect(msgs[1].toolCalls[0].name).toBe("bash");
    expect(msgs[1].toolCalls[0].output).toEqual({ code: 0, stdout: "clean" });

    render(
      <TooltipProvider>
        <ConversationView title="Chat" messages={rawMsgs} />
      </TooltipProvider>,
    );
    // Only ONE `bash` tool header rendered
    expect(screen.getAllByText("bash")).toHaveLength(1);
    expect(screen.getByText("Running git status.")).toBeInTheDocument();
  });

  it("streams live agent_text even if service_started was evicted from traces ring buffer", () => {
    const traces: TraceRecord[] = [
      {
        seq: 501,
        t_ms: 1000,
        machine: "chat_harness",
        kind: "service_progress",
        stream: "agent_thought",
        chunk: "Thinking step",
      },
      {
        seq: 502,
        t_ms: 1010,
        machine: "chat_harness",
        kind: "service_progress",
        stream: "agent_text",
        chunk: "Live streamed reply",
      },
    ];

    render(
      <TooltipProvider>
        <LiveTracesContext.Provider value={{ traces, isActive: true }}>
          <ConversationView title="Chat" messages={[{ role: "user", text: "Hi" }]} />
        </LiveTracesContext.Provider>
      </TooltipProvider>,
    );

    expect(screen.getByText("Live streamed reply")).toBeInTheDocument();
  });
});

describe("PromptInputView", () => {
  it("dispatches submit_event with field_name payload and supports interrupt_event", async () => {
    const onDispatch = vi.fn().mockResolvedValue(undefined);
    render(
      <TooltipProvider>
        <PromptInputView
          label="Prompt"
          placeholder="Ask something"
          submitEvent="PROMPT"
          fieldName="text"
          interruptEvent="INTERRUPT"
          onDispatch={onDispatch}
        />
      </TooltipProvider>,
    );

    const textarea = screen.getByPlaceholderText("Ask something");
    fireEvent.change(textarea, { target: { value: "Hello agent" } });
    fireEvent.submit(textarea.closest("form")!);

    await waitFor(() => {
      expect(onDispatch).toHaveBeenCalledWith("PROMPT", { text: "Hello agent" });
    });

    fireEvent.click(screen.getByRole("button", { name: /Stop/i }));
    expect(onDispatch).toHaveBeenCalledWith("INTERRUPT", {});
  });

  it("accepts drag-and-drop file attachments, uploads them on submit, and supports removal", async () => {
    const uploaded = {
      path: "/tmp/starness-uploads/inst-1/1-screenshot.png",
      name: "screenshot.png",
      mime: "image/png",
      size: 9,
    };
    const onUploadFile = vi.fn().mockResolvedValue(uploaded);
    const onDispatch = vi.fn().mockResolvedValue(undefined);

    // jsdom does not implement fetch("blob:..."); provide a tiny data URL shim for createObjectURL
    const origCreateObjectURL = URL.createObjectURL;
    const origRevokeObjectURL = URL.revokeObjectURL;
    URL.createObjectURL = vi.fn(() => "data:image/png;base64,aGVsbG8gcG5n");
    URL.revokeObjectURL = vi.fn();

    try {
      render(
        <TooltipProvider>
          <PromptInputView
            label="Prompt"
            placeholder="Drop files here"
            submitEvent="PROMPT"
            fieldName="text"
            allowAttachments
            onUploadFile={onUploadFile}
            onDispatch={onDispatch}
          />
        </TooltipProvider>,
      );

      const textarea = screen.getByPlaceholderText("Drop files here");
      const form = textarea.closest("form")!;

      const file = new File(["hello png"], "screenshot.png", { type: "image/png" });
      const dropEv = new Event("drop", { bubbles: true, cancelable: true });
      Object.defineProperty(dropEv, "dataTransfer", {
        value: { types: ["Files"], files: [file] },
      });
      act(() => {
        form.dispatchEvent(dropEv);
      });

      expect(screen.getByTestId("prompt-attachment-chip")).toHaveTextContent("screenshot.png");

      // Test removal button works
      fireEvent.click(screen.getByRole("button", { name: "Remove screenshot.png" }));
      expect(screen.queryByTestId("prompt-attachment-chip")).not.toBeInTheDocument();

      // Re-drop and submit without text (attachment-only prompt)
      const dropEv2 = new Event("drop", { bubbles: true, cancelable: true });
      Object.defineProperty(dropEv2, "dataTransfer", {
        value: { types: ["Files"], files: [file] },
      });
      act(() => {
        form.dispatchEvent(dropEv2);
      });
      expect(screen.getByTestId("prompt-attachment-chip")).toHaveTextContent("screenshot.png");

      fireEvent.submit(form);

      await waitFor(() => {
        expect(onUploadFile).toHaveBeenCalledTimes(1);
        expect(onDispatch).toHaveBeenCalledWith("PROMPT", {
          text: "",
          files: [uploaded],
        });
      });
      const [blobArg, nameArg, mimeArg] = onUploadFile.mock.calls[0];
      expect(blobArg).toBeInstanceOf(Blob);
      expect(nameArg).toBe("screenshot.png");
      expect(mimeArg).toBe("image/png");
    } finally {
      URL.createObjectURL = origCreateObjectURL;
      URL.revokeObjectURL = origRevokeObjectURL;
    }
  });
});

describe("ConversationView attachments", () => {
  it("renders attachment badges on user messages", () => {
    render(
      <TooltipProvider>
        <ConversationView
          title="Chat"
          messages={[
            {
              role: "user",
              text: "Check this screenshot",
              attachments: [{ path: "/tmp/shot.png", name: "shot.png", mime: "image/png" }],
            },
          ]}
        />
      </TooltipProvider>,
    );
    expect(screen.getByText("shot.png")).toBeInTheDocument();
    expect(screen.getByText("Check this screenshot")).toBeInTheDocument();
  });
});

describe("JSONUIProvider state-transition stability", () => {
  it("keeps Conversation mounted when updating store and switching view specs under same instance key", () => {
    const store = createStateStore({
      messages: [{ role: "user", text: "Hello" }],
    });
    const workingSpec = {
      root: "root",
      elements: {
        root: { type: "Stack", props: {}, children: ["transcript"] },
        transcript: {
          type: "Conversation",
          props: { title: "Chat", messages: { $state: "/messages" } },
        },
      },
    };
    const gatherSpec = {
      root: "root",
      elements: {
        root: { type: "Stack", props: {}, children: ["transcript", "btn"] },
        transcript: {
          type: "Conversation",
          props: { title: "Chat", messages: { $state: "/messages" } },
        },
        btn: {
          type: "Button",
          props: { label: "Done", event: "EXIT" },
        },
      },
    };

    const { rerender } = render(
      <TooltipProvider>
        <JSONUIProvider
          key="inst-1"
          registry={starnessRegistry}
          store={store}
          handlers={{ send_event: async () => {} }}
        >
          <Renderer spec={workingSpec} registry={starnessRegistry} />
        </JSONUIProvider>
      </TooltipProvider>,
    );

    const logElBefore = screen.getByRole("log");

    // Simulate state transition working -> gather_input with in-place store update
    act(() => {
      store.update({
        "/messages": [
          { role: "user", text: "Hello" },
          { role: "assistant", text: "World" },
        ],
      });
    });
    rerender(
      <TooltipProvider>
        <JSONUIProvider
          key="inst-1"
          registry={starnessRegistry}
          store={store}
          handlers={{ send_event: async () => {} }}
        >
          <Renderer spec={gatherSpec} registry={starnessRegistry} />
        </JSONUIProvider>
      </TooltipProvider>,
    );

    const logElAfter = screen.getByRole("log");
    expect(logElAfter).toBe(logElBefore);
    expect(screen.getByText("World")).toBeInTheDocument();
  });

  it("stays at current scrollTop when new messages arrive instead of yanking to bottom", () => {
    const { rerender } = render(
      <TooltipProvider>
        <ConversationView
          title="Chat"
          messages={[
            { role: "user", text: "First" },
            { role: "assistant", text: "Reply 1" },
          ]}
        />
      </TooltipProvider>,
    );

    const logEl = screen.getByRole("log");
    const scroller = logEl.firstElementChild as HTMLElement;
    Object.defineProperty(scroller, "scrollHeight", { value: 1200, configurable: true });
    Object.defineProperty(scroller, "clientHeight", { value: 400, configurable: true });
    scroller.scrollTop = 120;
    fireEvent.scroll(scroller);

    // Rerender with an additional message — scrollTop must remain at 120
    rerender(
      <TooltipProvider>
        <ConversationView
          title="Chat"
          messages={[
            { role: "user", text: "First" },
            { role: "assistant", text: "Reply 1" },
            { role: "assistant", text: "Reply 2 (longer)" },
          ]}
        />
      </TooltipProvider>,
    );

    expect(scroller.scrollTop).toBe(120);
  });
});
