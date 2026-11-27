# Changelog

## [0.1.1](https://github.com/starness-dev/starness/compare/v0.1.0...v0.1.1) (2026-11-27)


### Bug Fixes

* **mcp,release:** handle headless Linux keyring gracefully and use macos-15 runner ([126b005](https://github.com/starness-dev/starness/commit/126b005e552e651420827bbe8c0e2180f7df25ab))

## 0.1.0 (2026-11-27)


### Features

* add invoke, always, after, history, on_done, state_in, and built-in actions ([f167d29](https://github.com/starness-dev/starness/commit/f167d2909e55c8c2dc50a05fcfd852bdff1cde91))
* **cli:** add --trace JSONL streaming flag to starchart run ([7b1fc97](https://github.com/starness-dev/starness/commit/7b1fc974ccde6eb0b5e0f0758b14f54566d0235e))
* **cli:** add --ui view-sheet support and fullscreen Ratatui TUI ([cf8c1b5](https://github.com/starness-dev/starness/commit/cf8c1b53ddd536d7a41b4685b94604d50efbdab0))
* **cli:** add acp agent subcommand, async/context flags, and multi-agent ACP example ([ac2e1f3](https://github.com/starness-dev/starness/commit/ac2e1f3f8316f3d93f475dba8ca3b1aa3169d63d))
* **cli:** add interactive terminal TUI (starness run -i) for json-render specs and fallback event picker ([cde414e](https://github.com/starness-dev/starness/commit/cde414e7727235ce999b728f39b5894fbdef47b2))
* **cli:** add live --events-stdin event ingress and leaf final_state trace outputs ([e8ae389](https://github.com/starness-dev/starness/commit/e8ae389eb1322d3bf77be27c3cbd9645cf578812))
* **cli:** add mcp-bridge subcommand and full plan/review/implement/test workflow ([64230fe](https://github.com/starness-dev/starness/commit/64230fe3e8d1fafa2f22147b71085990d2750c25))
* **cli:** add starchart secrets keychain subcommand and E2E tests ([7b60b63](https://github.com/starness-dev/starness/commit/7b60b63f82d196016d2154afe7b20e44f0c1c283))
* **cli:** add starness mcp server, starness auth OAuth login, and --machine keychain scoping ([57cc0c5](https://github.com/starness-dev/starness/commit/57cc0c50c08f1706adf411a76d83edc5f87cc8b2))
* **cli:** upgrade Ratatui Table with focus persistence, clickable links, SVG snapshots, and remove Confirm ([b965874](https://github.com/starness-dev/starness/commit/b965874621c113e7046b8dda497b301a4562c267))
* **examples:** add coding_session_llm UI sheet, multi-tool queue, and AI showcase step ([15e9fa8](https://github.com/starness-dev/starness/commit/15e9fa8565b5751a14005cd1e8966685a4184118))
* **examples:** record escalation error details in multi_agent_acp ([d3d5beb](https://github.com/starness-dev/starness/commit/d3d5beb66c1213b009302a86c9e3a2c76ff88066))
* **examples:** upgrade coding_session_llm with parallel safe-first execution, approval gate, and output truncation ([d3db965](https://github.com/starness-dev/starness/commit/d3db965d7e400b42455176181212b4593f79f362))
* initial starchart typed Starlark statechart engine ([1e1396e](https://github.com/starness-dev/starness/commit/1e1396e248b335ddbe5feb8dba6f3ff7fe3b67a1))
* **ir,starlark:** add @starness/ui virtual module and static UI event validation ([adfa74b](https://github.com/starness-dev/starness/commit/adfa74b7069f30b341dae7c4093d46f7a19d525e))
* **ir:** validate PromptInput submit and interrupt events in UiViewSheet ([ed27254](https://github.com/starness-dev/starness/commit/ed27254fddcf8815c75965918f34d77df80c2422))
* **mcp:** add @starchart/mcp stdio and streamable HTTP client primitive ([f41979b](https://github.com/starness-dev/starness/commit/f41979b9b108b5e60805ee8c164d7a8f7b91474f))
* **runtime:** add built-in HTTP, exec, and ACP v1 drivers ([df1c595](https://github.com/starness-dev/starness/commit/df1c595cb02933bd11b99daed9efffb5b4460ab8))
* **runtime:** add decision-rs @starchart/decide driver with secret redaction ([ede8185](https://github.com/starness-dev/starness/commit/ede8185e85b87d0b573c25c856093f1d59067293))
* **runtime:** add genai @starness/llm driver with streaming and secret redaction ([cb6bf2b](https://github.com/starness-dev/starness/commit/cb6bf2bb6104df2c3b570f5410bdca3d01d6cd5e))
* **runtime:** add instance-scoped actor traces and lifecycle helpers ([e52007f](https://github.com/starness-dev/starness/commit/e52007ffe0dc8466fa650ee9176cf4179187a18d))
* **runtime:** add lossless TraceHub observer and service progress streaming ([a5e8255](https://github.com/starness-dev/starness/commit/a5e82558d93da75fdbe92381a6d9610b8c6343b8))
* **runtime:** add SQLite instance and trace persistence store ([af59112](https://github.com/starness-dev/starness/commit/af59112e5ea6de9b75e4a13594e7c8d389fb34da))
* **runtime:** add turn-scoped MCP server, nested context records, and ACP session follow-up ([dcfe21e](https://github.com/starness-dev/starness/commit/dcfe21e5a633b545bf04710dc6bd20421653c76a))
* **runtime:** adopt rmcp 3.11 for MCP client, stateless 2026-07-28 server, and OAuth 2.1 keychain auth ([7894d44](https://github.com/starness-dev/starness/commit/7894d445faa02157aee66612385e3156f8aa5177))
* **runtime:** attach optional UiViewSheet to ServeDefinition ([e3bc4e1](https://github.com/starness-dev/starness/commit/e3bc4e15d698c673bd95861e40d4e86920f9841f))
* **runtime:** expose active ui + allowed_events and serve embedded Web Studio ([027e82f](https://github.com/starness-dev/starness/commit/027e82fed2f06cafa6e7b94d05a90d892625bf53))
* **runtime:** implement bounded-concurrency invoke_all batch driver ([e86aff2](https://github.com/starness-dev/starness/commit/e86aff25d497460e88b494b6419a368cbb58d241))
* **runtime:** resolve env, file, and keychain secrets with trace/error redaction ([f8dae4e](https://github.com/starness-dev/starness/commit/f8dae4eaf63d5c6e99e558b50ef28337b59c54d8))
* **runtime:** support multi-turn messages, tool calling, attachments, and caching in @starness/llm ([ce16cfa](https://github.com/starness-dev/starness/commit/ce16cfaaf877e301a20e66238fb07eda8b21fbef))
* **serve:** add HTTP + SSE daemon and resumable --state-file persistence ([449633f](https://github.com/starness-dev/starness/commit/449633f821011c005046dab2e4c4e1cbad3acc79))
* **serve:** add instance file upload endpoint with sanitization and purge cleanup ([9bed1a9](https://github.com/starness-dev/starness/commit/9bed1a97da935c7157d61bfec9be5372c09b30c8))
* **serve:** route multi-machine instances on one port with SQLite persistence ([a31d1d2](https://github.com/starness-dev/starness/commit/a31d1d25984e8645af896ae8d2a45a33bc85388c))
* **starlark:** add @starchart/decide virtual module, providers, and guard helpers ([7ed3f2c](https://github.com/starness-dev/starness/commit/7ed3f2c79c72cd2c629b9d4b6c30adb3404ef5b1))
* **starlark:** add @starchart/secrets DSL module and IR context guard ([57b745c](https://github.com/starness-dev/starness/commit/57b745c67bdb0ce51eae94efb653ac4733b8e6b8))
* **starlark:** add @starness/llm virtual module, schema derivation, and record rehydration ([8f3e99b](https://github.com/starness-dev/starness/commit/8f3e99bf460fe933d04a8dcf7061a9724ec1e8c5))
* **starlark:** add dynamic invoke input builders and @starchart/* stdlib modules ([a0377ab](https://github.com/starness-dev/starness/commit/a0377ab17529feb5f7304f949dbb4e58badb7ad4))
* **starlark:** add invoke_all and batch_item prelude builtins ([6eceb87](https://github.com/starness-dev/starness/commit/6eceb87b9dc199b1d35bdb76582814c6bc7f9fab))
* **starlark:** add MCP tool DSL, record JSON schema derivation, and update/artifact helpers ([142b2af](https://github.com/starness-dev/starness/commit/142b2af37eddfed4812f0cf169ee6e1373a8a90b))
* **starlark:** add typed LlmMessage, LlmAttachment, and attachments_from to @starness/llm ([0926322](https://github.com/starness-dev/starness/commit/0926322081f57b45e6f28da1fda0b3928284252d))
* **starlark:** extend @starness/llm with messages, tools, attachments, and caching ([e7309b3](https://github.com/starness-dev/starness/commit/e7309b3efbe613b64eb7c771db89abe7deef0792))
* **starlark:** strengthen invoke callback typechecking and tool-call field validation ([6b2b987](https://github.com/starness-dev/starness/commit/6b2b987888d1263f5174ed94fe5715f1c93bb04c))
* **tui:** add AI Elements parity and structured TraceHub stream rendering ([304c93e](https://github.com/starness-dev/starness/commit/304c93e526a712abc21e2e9b8404e37481272299))
* **tui:** add live streaming Markdown in Conversation, Button shortcuts, and UI verification tests ([70f4ceb](https://github.com/starness-dev/starness/commit/70f4ceb57ace66c391f6e39b9d28a36014a14a36))
* **ui,tui:** add drag-and-drop file attachments and preserve Conversation scroll position ([0df1bfb](https://github.com/starness-dev/starness/commit/0df1bfb069bb6b044a570d52842b9c8fbccc3243))
* **ui:** add Button shortcut prop, resilient live streaming, and Vitest component suite ([8355174](https://github.com/starness-dev/starness/commit/835517414b0f533dcb1464b3b9effc5e851f0815))
* **ui:** add diagram zoom/pan/expand, clickable links in tables, instance machine labels, and remove Confirm ([ed91ebb](https://github.com/starness-dev/starness/commit/ed91ebb0e273ec255de1c8cb2af39f591528dae9))
* **ui:** add json-render Starness Studio with MarkdownReview and Mermaid diagram ([f85c075](https://github.com/starness-dev/starness/commit/f85c0759fcba9f1d0fb7a69f08f530fbba8692b7))
* **ui:** adopt shadcn AI Elements and polish MarkdownReview and DiffReview ([567513b](https://github.com/starness-dev/starness/commit/567513bad7cff86520d54b0edb5e0a4d2ff0a489))
* **ui:** move Diagram, Trace, and Context into right Inspector pane to match TUI ([b7f7f5e](https://github.com/starness-dev/starness/commit/b7f7f5e34d0d252be222e58b2bf56a9dc63181e0))
* **ui:** stream active turns inside Conversation, preserve scroll, and code-split with rust-embed ([45cb835](https://github.com/starness-dev/starness/commit/45cb8356d11eb2655377d8c45de16c76b226f775))
* **ui:** upgrade Web Studio to 15-widget catalog, per-line MarkdownReview, and active Mermaid highlight ([bd0982e](https://github.com/starness-dev/starness/commit/bd0982eaf2692e8c17b06b421cbd12377228a461))


### Bug Fixes

* **cli:** give display tables flexible height and wrap multi-button ActionBar cleanly ([83485f6](https://github.com/starness-dev/starness/commit/83485f657e5f056e8ffc730f1a1fe53eaad48adb))
* **examples:** use api.bsky.app default endpoint in bluesky_radar.star ([9fdbf0d](https://github.com/starness-dev/starness/commit/9fdbf0d6dd5f74e702236ed9db7d386b48d566b8))
* **release:** use simple release-please strategy with Cargo.toml workspace version updater ([da7c3d0](https://github.com/starness-dev/starness/commit/da7c3d08f7b75dcc999e04d4f574127478061c05))
* **secrets:** hide TTY echo on secrets set and fall back from env to OS keychain ([2328bd9](https://github.com/starness-dev/starness/commit/2328bd95e95ec43b6a803d121439eac13067d8af))


### Miscellaneous Chores

* **release:** set initial-version to 0.1.0 ([1d688ca](https://github.com/starness-dev/starness/commit/1d688caa49eac3379a43c6d1a0b527f62991ed45))
