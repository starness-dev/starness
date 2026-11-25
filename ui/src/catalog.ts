import { defineCatalog } from "@json-render/core";
import { schema } from "@json-render/react";
import { z } from "zod";

export const starnessCatalog = defineCatalog(schema, {
  components: {
    Stack: {
      props: z.object({
        direction: z.enum(["vertical", "horizontal"]).optional(),
        gap: z.enum(["sm", "md", "lg"]).optional(),
      }),
      slots: ["default"],
      description: "Flex layout container for grouping child elements.",
    },
    Card: {
      props: z.object({
        title: z.string().optional(),
        description: z.string().optional(),
      }),
      slots: ["default"],
      description: "Titled container card.",
    },
    Markdown: {
      props: z.object({
        title: z.string().optional(),
        content: z.string(),
      }),
      description: "Renders structured Markdown text.",
    },
    KeyValue: {
      props: z.object({
        title: z.string().optional(),
        items: z.unknown(),
      }),
      description: "Key-value metadata grid.",
    },
    Table: {
      props: z.object({
        title: z.string().optional(),
        columns: z.array(z.string()),
        rows: z.unknown(),
      }),
      description: "Structured tabular data display.",
    },
    Badge: {
      props: z.object({
        label: z.unknown(),
        variant: z.enum(["default", "success", "warning", "danger", "info"]).optional(),
      }),
      description: "Status badge pill.",
    },
    Alert: {
      props: z.object({
        title: z.string(),
        message: z.unknown().optional(),
        variant: z.enum(["info", "success", "warning", "danger"]).optional(),
      }),
      description: "Status callout banner.",
    },
    TextInput: {
      props: z.object({
        label: z.string(),
        bind: z.string(),
        value: z.unknown().optional(),
        placeholder: z.string().optional(),
        required: z.boolean().optional(),
      }),
      description: "Single-line text input bound to a JSON Pointer path.",
    },
    Textarea: {
      props: z.object({
        label: z.string(),
        bind: z.string(),
        value: z.unknown().optional(),
        placeholder: z.string().optional(),
        rows: z.number().optional(),
        required: z.boolean().optional(),
      }),
      description: "Multi-line text area bound to a JSON Pointer path.",
    },
    Select: {
      props: z.object({
        label: z.string(),
        bind: z.string(),
        value: z.unknown().optional(),
        options: z.array(z.string()),
        required: z.boolean().optional(),
      }),
      description: "Dropdown select input bound to a JSON Pointer path.",
    },
    MultiSelect: {
      props: z.object({
        label: z.string(),
        bind: z.string(),
        value: z.unknown().optional(),
        options: z.array(
          z.object({
            id: z.string(),
            label: z.string(),
            description: z.string().optional(),
          }),
        ),
      }),
      description: "Checklist of toggleable options bound to a list of IDs.",
    },
    Button: {
      props: z.object({
        label: z.string(),
        event: z.string(),
        payload: z.record(z.string(), z.unknown()).optional(),
        variant: z.enum(["primary", "secondary", "danger", "ghost"]).optional(),
        shortcut: z.string().optional(),
      }),
      description: "Interactive button dispatching a Starness statechart event, with optional keyboard shortcut (e.g. 'Esc').",
    },
    MarkdownReview: {
      props: z.object({
        title: z.string(),
        markdown: z.unknown(),
        agent_notes: z.unknown().optional(),
        comments_path: z.string().optional(),
        approve_event: z.string(),
        revise_event: z.string(),
        approve_label: z.string().optional(),
        revise_label: z.string().optional(),
      }),
      description:
        "Interactive Markdown artifact reviewer with section-anchored comments and approve/revise actions.",
    },
    DiffReview: {
      props: z.object({
        title: z.string(),
        diff: z.unknown(),
        agent_notes: z.unknown().optional(),
        comments_path: z.string().optional(),
        approve_event: z.string(),
        revise_event: z.string(),
        approve_label: z.string().optional(),
        revise_label: z.string().optional(),
      }),
      description:
        "Interactive unified diff reviewer with file & line-anchored comments and approve/revise actions.",
    },
    Conversation: {
      props: z.object({
        title: z.string().optional(),
        messages: z.unknown(),
        empty_text: z.string().optional(),
      }),
      description:
        "AI conversation transcript supporting user, assistant, reasoning, and tool messages (including @starness/llm message envelopes).",
    },
    PromptInput: {
      props: z.object({
        label: z.string().optional(),
        placeholder: z.string().optional(),
        submit_event: z.string(),
        submit_label: z.string().optional(),
        field_name: z.string().optional(),
        interrupt_event: z.string().optional(),
        interrupt_label: z.string().optional(),
        allow_attachments: z.boolean().optional(),
        accept: z.string().optional(),
      }),
      description:
        "AI prompt input bar dispatching a statechart event with the entered text, optional file attachments, and optional interrupt button.",
    },
    ToolCall: {
      props: z.object({
        name: z.string(),
        status: z.string().optional(),
        arguments: z.unknown().optional(),
        output: z.unknown().optional(),
        error: z.unknown().optional(),
        expanded: z.boolean().optional(),
      }),
      description:
        "Collapsible MCP/LLM/ACP tool call card displaying status, arguments, and output or error.",
    },
    Reasoning: {
      props: z.object({
        title: z.string().optional(),
        content: z.unknown(),
        duration_ms: z.unknown().optional(),
        expanded: z.boolean().optional(),
      }),
      description:
        "Collapsible model reasoning or decision rationale trace block.",
    },
    TaskList: {
      props: z.object({
        title: z.string().optional(),
        tasks: z.unknown(),
      }),
      description:
        "Step-by-step agent plan or orchestrator checklist with per-task status.",
    },
    Terminal: {
      props: z.object({
        title: z.string().optional(),
        command: z.string().optional(),
        stdout: z.unknown().optional(),
        stderr: z.unknown().optional(),
        exit_code: z.unknown().optional(),
      }),
      description:
        "Command header and monospace stdout/stderr output viewer with exit code badge.",
    },
  },
  actions: {
    send_event: {
      params: z.object({
        event: z.string(),
        payload: z.record(z.string(), z.unknown()).optional(),
      }),
      description: "Dispatch an event to the active Starness statechart instance.",
    },
  },
});
