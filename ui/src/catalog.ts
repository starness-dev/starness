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
    Confirm: {
      props: z.object({
        title: z.string(),
        message: z.string().optional(),
        confirm_label: z.string().optional(),
        cancel_label: z.string().optional(),
        confirm_event: z.string(),
        cancel_event: z.string().optional(),
      }),
      description: "Confirmation card that dispatches confirm_event or cancel_event.",
    },
    Button: {
      props: z.object({
        label: z.string(),
        event: z.string(),
        payload: z.record(z.string(), z.unknown()).optional(),
        variant: z.enum(["primary", "secondary", "danger", "ghost"]).optional(),
      }),
      description: "Interactive button dispatching a Starness statechart event.",
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
