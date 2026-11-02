# Schedule API

The daemon exposes an authenticated local HTTP API at `http://127.0.0.1:7639`
by default. Configure `api_listen` / `services.invocateur.apiListen` to change
its loopback address and port. Every request requires:

```http
Authorization: Bearer <contents of STATE_DIRECTORY/api-token>
Content-Type: application/json
```

The daemon generates a private token on first start and retains it across
restarts. `STATE_DIRECTORY/api.json` contains the actual URL, including the port
when configured with `:0`. Tokens are not stored in Nix configuration. Requests
with browser `Origin` headers are rejected; the API is for local agent clients.
The CLI uses this API and discovers its credentials automatically.

| Method | Endpoint | Result |
| --- | --- | --- |
| GET | `/health` | API health |
| GET | `/v1/schedules` | `{ "schedules": [resource, ...] }` |
| POST | `/v1/schedules` | Create a task; HTTP 201 and resource |
| GET | `/v1/schedules/NAME` | Read a resource |
| PATCH | `/v1/schedules/NAME` | Atomically merge task fields |
| PUT | `/v1/schedules/NAME` | Replace a task definition |
| DELETE | `/v1/schedules/NAME` | Delete a task; HTTP 204 |
| POST | `/v1/schedules/NAME/resume` | Acknowledge recovery and schedule future work |

Task names are immutable unique identifiers, with 1–64 ASCII letters, digits,
underscores, or hyphens. A create collision returns HTTP 409 and never replaces
an existing schedule.

Example POST body for a coding agent:

```json
{
  "name": "weekday-review",
  "cron": "0 9 * * MON-FRI",
  "timezone": "America/New_York",
  "directory": "~/repos/my-project",
  "prompt": "Review recent changes and follow AGENTS.md."
}
```

Example POST body for a command:

```json
{
  "name": "fetch-updates",
  "every_seconds": 3600,
  "directory": "~/repos/my-project",
  "command": ["git", "fetch", "--all"],
  "timeout_seconds": 120
}
```

| Field | Meaning / default |
| --- | --- |
| `name` | Required unique identifier |
| `enabled` | Boolean; default `true` |
| `cron` | Cron expression; configure this or `every_seconds` |
| `every_seconds` | Positive interval; configure this or `cron` |
| `timezone` | IANA zone for cron; default `UTC` |
| `directory` | Existing working directory; default service `repos_directory` |
| `prompt` | Agent instructions; configure this or `command` |
| `command` | Literal executable and argv; configure this or `prompt` |
| `timeout_seconds` | Command timeout; default service `command_timeout_seconds` |
| `run_immediately` | Write-time option to make the next occurrence due now; stored as `false` afterward |
| `workspace_name` | Agent workspace template; default `{task_name} {scheduled_at}` |
| `tab_name` | Agent tab/window template; default `{agent_name}` |

Name templates support `{task_name}`, `{scheduled_at}`, `{agent_name}`, `{agent}`,
`{multiplexer}`, and `{session}`. Commands use no shell unless explicitly provided
in argv. Agent tasks use the service's configured agent and multiplexer.

A resource contains `task`, `revision`, `created_at`, `updated_at`, `next_run`, and
`last_run`. Disabled tasks have no `next_run`. `last_run` includes a dispatch ID,
occurrence time, status, optional agent/pane identifiers, and any error. Definitions
and execution state persist together, reserved before dispatch, so edits do not
race with the runner and restarts do not replay an occurrence.

Reads and writes return an `ETag` containing the quoted revision. Send it in
`If-Match` when editing, deleting, or resuming. A stale revision returns HTTP 412
with no mutation. The CLI does this automatically. Direct API clients may omit
`If-Match` for an unconditional atomic update.

For example, switching an interval to cron requires clearing the old timing field
in the same PATCH:

```json
{"cron": "0 10 * * MON-FRI", "every_seconds": null}
```

Similarly, switching command to agent uses `{"prompt":"...","command":[]}`.
The CLI's timing and action flags clear the previous alternative automatically.
PATCH accepts task fields, not resource metadata. Editing a definition recalculates
its next occurrence from the current time; `run_immediately` explicitly makes it
due now. Disabled drafts may omit timing/action fields until enabled. A task being dispatched cannot
be edited, deleted, or resumed; these operations return HTTP 409. Other resources
remain available. Resume only applies to `needs-attention` and never reruns the
interrupted occurrence.

Errors are JSON objects with an `error` string. Statuses include 400 for invalid
input, 401 for missing/invalid credentials, 403 for browser-origin requests, 404
for unknown resources, 409 for duplicate names or active dispatch conflicts, 412
for stale revisions, and 413 for bodies over 1 MiB. Configuration remains TOML;
task resources and API/CLI messages use JSON.
