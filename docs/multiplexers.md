# Multiplexer adapter contracts

Invocateur uses typed Rust request and response models over the supported CLIs.
The common boundary is `LaunchRequest` → `LaunchResponse`, with a `LaunchTarget`
variant identifying a Herdr pane, tmux pane, or Zellij tab. Backend-specific ID
newtypes validate the native identifier format before it can become a command
target. Pane and tab identifiers are not interchangeable.

| Adapter | Requests | Responses |
| --- | --- | --- |
| Herdr | `ListWorkspaces`, `CreateWorkspace`, `RenameTab`, `StartAgent`, `PromptAgent` | Typed JSON envelopes and operation-specific bodies |
| tmux | `HasSession`, `NewWindow` | Successful session check or validated `%N` pane ID from `new-window -P -F` |
| Zellij | `QueryTabs`, `NewTab` | Successful session check or validated unsigned tab ID |

Herdr requests associate their response type at compile time. The decoder accepts
unknown extension fields, requires object-shaped envelopes and results, and
rejects missing or conflicting success/error fields. A successful result also
requires a successful CLI exit. Structured remote failures retain their code and
message even when the CLI exits nonzero. The Herdr CLI continues to provide its
existing interactive-startup readiness checks.

A workspace-creation response may contain a valid pane alongside missing or
malformed tab metadata. The adapter checkpoints that validated pane before
checking the tab, preserving a reference for inspection without issuing more
commands. Pane and tab IDs must identify the same workspace. Invalid identifiers
or ambiguous responses never produce a started acknowledgement or an automatic
second dispatch.

The triggers persist targets in the existing formats (`wN:pN`, `%N`, and
`tab:N`). State files and the schedule HTTP API retain their current formats.
No direct/private multiplexer wire protocol is used by these adapters.
