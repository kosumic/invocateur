# Multiplexer adapter contracts

Invocateur supports Herdr only, using typed Rust request and response models over
its public CLI. The common launch boundary is `LaunchRequest` → `LaunchResponse`,
with a validated Herdr pane ID in `LaunchTarget`. Pane, tab, and workspace IDs
have distinct Rust types and cannot be interchanged. Identifier segments use
Herdr's base-32 alphabet, including uppercase letters: `wJ`, `wM:pA9`, and
`wM:tJ0` are valid. Treat these handles as opaque rather than decimal counters.

| Requests | Responses |
| --- | --- |
| `ListWorkspaces` | Typed workspace summaries |
| `ListAgents` | Typed live identities, pane IDs, and lifecycle states |
| `CreateWorkspace` | Validated root pane and tab metadata |
| `RenameTab`, `StartAgent`, `PromptAgent` | Object-shaped acknowledgements |

Herdr requests associate their response type at compile time. The decoder accepts
unknown extension fields, requires object-shaped envelopes and results, and
rejects missing or conflicting success/error fields. A successful result also
requires a successful CLI exit. Structured remote failures retain their code and
message even when the CLI exits nonzero or writes its structured error to stderr.
The Herdr CLI continues to provide its existing interactive-startup readiness
checks.

A workspace-creation response may contain a valid pane alongside missing or
malformed tab metadata. The adapter checkpoints that validated pane before
checking the tab, preserving a reference for inspection without issuing more
commands. Pane and tab IDs must identify the same workspace. Invalid identifiers
or ambiguous responses never produce a started acknowledgement or an automatic
second dispatch.

GitHub conversation associations persist the owning agent's name and session.
Follow-ups look up that name in the live `agent list` response and use
`agent prompt` to submit input atomically. They never use raw shell input. Pane
moves are reflected in the live pane ID; blocked, unknown, ambiguous, or malformed
responses defer work before claiming the comment. A missing named agent permits
a replacement launch. After a claim, ambiguous dispatches retain their durable
record and are never automatically replayed.

New targets persist as `wN:pN`. Historical records from removed backends remain
readable for deduplication but are never used as active targets. The schedule
HTTP API keeps its current format. No private multiplexer wire protocol is used.
