# invocateur

The CLI command is `invo`.

A Rust daemon that polls GitHub through your local authenticated `gh` CLI and
starts a coding agent in **Herdr (default), tmux, or Zellij** when you post a
configured bot mention. It also runs scheduled coding-agent prompts and commands.
A Home Manager module provides a systemd user service.

## Install without Nix

Install a current stable Rust toolchain (Cargo), a C compiler/linker, Git,
[GitHub CLI](https://cli.github.com/), your chosen multiplexer, and the coding
agent you want to use. Herdr is the default multiplexer.

Install Invocateur and authenticate with GitHub:

```bash
cargo install --locked --git https://github.com/kosumic/invocateur
gh auth login
mkdir -p ~/.config/invocateur
```

Create `~/.config/invocateur/config.toml`:

```toml
multiplexer = "herdr"
workspace_name = "{repository}#{issue_number} {agent_name}"
tab_name = "{agent_name}"
bot_name = "invo"
use_hostname_as_bot_name = false
agent = "codex"
repos_directory = "~/repos"
poll_interval_seconds = 60
acknowledgement_message = "🪄 Invocateur started {agent} for this task.\n\nTriggered by {comment_url}."
```

These are the defaults; all settings are optional in the TOML file. Standalone
settings use snake_case names, such as `agent_args`, `herdr_session`,
`initial_lookback_seconds`, and `discover_github_repositories`. Additional
checkout paths can be configured with a TOML table (put this after your other settings):

```toml
[repositories]
"owner/repo" = "~/path"
```

TOML supports comments (`#`) and multiline strings, which are useful for custom
acknowledgements. Configuration is TOML only; Home Manager generates TOML
from its Nix options automatically. If upgrading from a JSON config, convert its
contents to TOML and update your service's `--config` path to `config.toml`.

Validate the configuration without contacting GitHub or starting agents:

```bash
~/.cargo/bin/invo --config ~/.config/invocateur/config.toml --check-config
```

With the default backend, start Herdr normally and leave its default session
running, then run Invocateur:

```bash
~/.cargo/bin/invo --config ~/.config/invocateur/config.toml
```

Keep this process running to poll for new bot mentions. Set `herdr_session`
if you use a different existing Herdr session. See the backend settings below for
tmux and Zellij. Invocateur requires an existing multiplexer session.

### Run as a systemd user service on Linux

Create the unit directory:

```bash
mkdir -p ~/.config/systemd/user
```

Save this as `~/.config/systemd/user/invocateur.service`:

```ini
[Unit]
Description=Summon coding agents from GitHub comments

[Service]
ExecStart=%h/.cargo/bin/invo --config %h/.config/invocateur/config.toml
Environment="PATH=%h/.cargo/bin:%h/.local/bin:/usr/local/bin:/usr/bin:/bin"
Restart=on-failure
RestartSec=15
UMask=0077

[Install]
WantedBy=default.target
```

The executable path assumes the default Cargo installation directory. Adjust
`ExecStart` and `PATH` if Invocateur, `gh`, Git, or Herdr are installed elsewhere.
Stop any foreground Invocateur instance before enabling the service:

```bash
systemctl --user daemon-reload
systemctl --user enable --now invocateur
journalctl --user -u invocateur -f
```

After editing `config.toml`, restart the service:

```bash
systemctl --user restart invocateur
```

Keep the configured multiplexer session running. On Unix systems without systemd, use the
foreground command above or configure your operating system's process manager.

## Multiplexers and names

Set `multiplexer` in TOML or `services.invocateur.multiplexer` in Home Manager:

| Backend | Session setting / default | Agent location |
| --- | --- | --- |
| `herdr` | `herdr_session` / `default` | New background workspace and named tab |
| `tmux` | `tmux_session` / `invocateur` | New detached window in the existing session |
| `zellij` | `zellij_session` / `invocateur` | New background tab in the existing session |

The Home Manager session options are `herdrSession`, `tmuxSession`, and
`zellijSession`. It installs the selected multiplexer package. Standalone users
can override binary paths with `herdr_path`, `tmux_path`, and `zellij_path`.

Create the selected alternative's session before using it:

```bash
# tmux, when multiplexer = "tmux"
tmux new-session -d -s invocateur

# Zellij, when multiplexer = "zellij"
zellij attach --create-background invocateur
```

Attach later with `tmux attach -t invocateur` or `zellij attach invocateur`.
Herdr validates agent readiness and submits the task through its agent API.
tmux and Zellij run the agent executable with `agent_args` followed by the task
as one final positional argument, so use an agent CLI that supports an initial
prompt argument. The executable is resolved from Invocateur's `PATH` (or an
absolute `agent` path); task text is passed literally, without shell expansion.
Approval and trust dialogs remain interactive. The Zellij backend uses the
`new-tab --no-focus` initial-command API verified with Zellij 0.45.1.

Configure names using `workspace_name` / `tab_name` in TOML, or `workspaceName` /
`tabName` in Home Manager. Defaults are `{repository}#{issue_number} {agent_name}`
and `{agent_name}`. Workspace names apply to Herdr; tab names apply to Herdr and
Zellij tabs or tmux windows. Session names are configured separately.

Name templates support `{repository}`, `{issue_number}`, `{agent}`,
`{agent_name}`, `{comment_id}`, `{multiplexer}`, and `{session}`. For example:

```toml
multiplexer = "zellij"
zellij_session = "invocateur"
workspace_name = "{repository} issue {issue_number}"
tab_name = "issue-{issue_number}-{comment_id}"
```

Rendered names must be nonempty, single-line, and at most 256 UTF-8 bytes. The
internal agent identity and GitHub claim key remain independent of display names.
Changing backends or names does not cause an already claimed comment to run again.

## Install with Nix and Home Manager

Import `invocateur.homeManagerModules.default` in your Home Manager configuration
and enable `services.invocateur`. The systemd user service requires Linux.

Add the flake input, following your configuration's Nixpkgs if desired:

```nix
inputs.invocateur = {
  url = "github:kosumic/invocateur";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

In your Home Manager module:

```nix
imports = [ inputs.invocateur.homeManagerModules.default ];
```

```nix
services.invocateur = {
  enable = true;
  multiplexer = "herdr"; # or "tmux" / "zellij"
  workspaceName = "{repository}#{issue_number} {agent_name}";
  tabName = "{agent_name}";
  botName = "invo";
  useHostnameAsBotName = false;
  agent = "codex";
  agentArgs = [];
  acknowledgementMessage = "🪄 Invocateur started {agent} for this task.\n\nTriggered by {comment_url}.";
  reposDirectory = "${config.home.homeDirectory}/repos";
  pollInterval = 60; # seconds between completed polls
  discoverGithubRepositories = true;

  # Optional paths outside reposDirectory, or repositories to watch before cloning.
  repositories = {
    "kosumi/nixpkgs" = "~/nixpkgs";
    "owner/project" = "~/repos/project";
  };
};
```

The bot name, agent, repository directory, and polling interval default to
`invo`, `codex`, `~/repos`, and 60 seconds.
`agent` is a Herdr-supported agent kind, such as `codex` or `claude`, or an
executable name/absolute path for tmux and Zellij. `extraPackages` makes agent
packages available to the polling service and defaults to `[ pkgs.codex ]`.
Herdr also needs its agent executable in the server's environment.

Log in with `gh auth login`, and start Herdr normally before using the service.
The default Herdr session is `default`; set `herdrSession` to target another
already running session. The daemon does not start or restart Herdr itself.

## Acknowledgements and multiple machines

After successfully starting an agent and submitting its task, Invocateur posts a
new comment on the issue or PR. The default message is:

```text
🪄 Invocateur started codex for this task.

Triggered by https://github.com/owner/repo/issues/42#issuecomment-123.
```

Customize it with `acknowledgement_message` in TOML or
`services.invocateur.acknowledgementMessage` in Home Manager. The nonempty message
supports `{agent}`, `{comment_url}`, `{repository}`, `{issue_number}`,
`{multiplexer}`, and `{session}`. You can
include a machine name directly in each machine's message. A hidden marker keeps
acknowledgement comments from triggering agents, even if a custom message includes
a bot mention. Inline review triggers are acknowledged in the PR's
conversation, with a link back to the triggering review comment.

Before starting an agent, Invocateur claims the triggering comment by adding a 👀
reaction as the authenticated GitHub account. GitHub's
[reaction API](https://docs.github.com/en/rest/reactions/reactions#create-reaction-for-an-issue-comment)
returns `201` for a new reaction and `200` when that account already added it.
Only the service receiving `201` starts an agent. This coordinates simultaneous
services without relying on a shared filesystem or a race-prone comment lookup.

Run the updated version on every machine, using the same `gh` account. The claim
is per triggering comment; a new mention comment requests another agent. Reserve
your own 👀 reaction on mention comments for Invocateur: an existing one counts as
an existing claim, including one added manually. Other users' reactions do not
prevent your account from claiming a task.

Claims do not expire and remain after startup failures or lost responses. This
prevents another machine from starting duplicate work while the first machine's
status is uncertain. Check Herdr and service logs before requesting a new agent.
Deleting an acknowledgement does not release its claim. All instances must be
upgraded; older versions do not participate in this protocol.

The `gh` account/token must be allowed to create reactions and issue/PR comments.
Fine-grained tokens need the relevant Issues/Pull requests write permissions for
the repositories being watched, including private repositories. A claim failure
prevents agent startup. If acknowledgement posting fails after an agent starts,
the agent stays running and the error is recorded locally. Ambiguous comment
POSTs are not retried automatically, to avoid posting duplicate acknowledgements.

## Usage and behavior

Post a comment on an issue, PR conversation, or inline PR review:

```text
@invo Fix the crash described here and check the relevant build.
```

Set `bot_name = "invo"` in TOML or `botName = "invo"` in Home
Manager to configure the name (without `@`). Mentions are case-insensitive and
can appear on their own line or alongside instructions. The name need not be a
GitHub account; authentication and acknowledgement comments use your local `gh`
account. Plain `summon` no longer triggers an agent.

To address each machine by hostname, enable the optional toggle (off by default):

```toml
use_hostname_as_bot_name = true
```

Or with Home Manager:

```nix
services.invocateur.useHostnameAsBotName = true;
```

This overrides `bot_name` / `botName` with the short system hostname at startup.
For example, `thinker` or `thinker.example.com` uses `@thinker`. It reads the
operating system hostname directly, independently of the `HOSTNAME` environment
variable. The effective name must use letters, digits, underscores, or hyphens;
invalid names fail configuration validation. Disable the toggle to use the
configured name again. `--check-config` and startup logs show the effective
trigger. Machines retain the same GitHub claim mechanism to prevent duplicates.

Markdown blockquotes (including nested quotes and lazy continuation lines without
another `>`), fenced/indented code blocks, inline code, links, images, and quoted
strings such as `"@invo"` do not trigger agents. Email addresses and longer
handles such as `@invo-other` do not match. A fresh unquoted mention after a
Markdown quote still works. Comments containing
raw HTML `<blockquote>` or `<q>` containers are ignored entirely. Only comments from
the account returned by `gh api user` are accepted. Issue/PR descriptions and
review summary bodies are not triggers. The agent receives only `Work on it:`
followed by the triggering comment's link. It reads the issue or PR and your
instructions from GitHub; Invo does not fetch or paste the title, description,
or comment text into the launch prompt.

The service searches for your recently updated conversations and reviews across
GitHub and inspects their repositories. Set `discoverGithubRepositories = false`
to restrict polling to local and explicitly configured repositories. Search
indexing can add delay beyond the polling interval. Incomplete search responses
or results above GitHub's 1,000-result limit are reported without advancing the
discovery cursor, so results are not silently discarded. Pending failed polls and
explicit repository mappings continue to be polled if discovery fails.

The service also discovers GitHub `origin` remotes under `reposDirectory`, in both
`~/repos/project` and `~/repos/owner/project` layouts. In global mode these paths
are a checkout catalog: only repositories with recent matching GitHub search
results, pending retries, or explicit mappings get comment API queries. An idle
poll does not scan the checkout catalog. With global discovery disabled, all
locally discovered repositories are polled directly.
Explicit `repositories` entries override discovered paths. A missing checkout is
shallow cloned on the first matching comment unless `cloneMissing = false`.
Repositories found through GitHub use `reposDirectory/owner/repo` if there is no
matching local checkout, which avoids collisions between owners. Existing checkouts are reused without pulling, resetting,
or creating worktrees; origin must match the watched repository.

The [GitHub comment API](https://docs.github.com/en/rest/issues/comments)
includes issue and PR conversation comments. Inline review comments use the
separate review-comment endpoint; set `reviewComments = false` to disable those.
Each global poll makes one identity request, two paginated GitHub searches, and
two paginated comment queries per pending or explicitly watched repository
(one when review comments are disabled). Discovery uses GitHub's
[commenter and review search qualifiers](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests),
including private repositories accessible through your `gh` login.
`githubHost` defaults to `github.com` and supports authenticated enterprise hosts.

On first seeing a repository the daemon processes future comment updates only.
Set `initialLookback = 3600` to include the previous hour for newly watched
repositories. Subsequent restarts resume the saved cursor, including downtime.
Polling uses a five-minute overlap for delayed API visibility. Editing a comment
to add a bot mention can trigger it, but each comment launches at most once, even if
edited again. A new comment summons another agent.

State lives in `$XDG_STATE_HOME/invocateur/state.json` (normally
`~/.local/state/invocateur/state.json`), with a lock preventing concurrent
instances. State is written before launching. Interrupted or ambiguous Herdr
operations are recorded as `reserved`, `claimed`, or `needs-attention` and are never retried
automatically. Inspect the saved multiplexer, session, pane/tab, and logs; continue the existing agent
manually, or post a new mention comment after resolving the problem. An unavailable
Herdr server or failed GitHub read is retried on the next poll. Do not delete state
to retry a launch: that loses deduplication history. State retains launch records
so old comment edits cannot accidentally launch duplicate agents.

```bash
systemctl --user status invocateur
journalctl --user -u invocateur -f
```

## Standalone CLI options

The package and reusable module are exported as `packages.<system>.invocateur`
and `homeManagerModules.default`. The standalone binary accepts TOML settings
with snake_case names, with the same defaults as the module:

```bash
nix run github:kosumic/invocateur -- --config /path/to/config.toml --check-config
nix run github:kosumic/invocateur -- --config /path/to/config.toml --once
```

`--once` performs one poll without sleeping. `--check-config` validates settings
without using GitHub or Herdr. `--state-dir /absolute/path` overrides the state
directory. Use a separate state directory for manual checks while the service runs.
Launch records also retain the acknowledgement URL or acknowledgement error;
`claimed-elsewhere` means the shared GitHub reaction was already present.

```toml
bot_name = "invo"
agent = "codex"
repos_directory = "~/repos"
poll_interval_seconds = 60
```

## Scheduled tasks managed by agents

Agents manage schedules dynamically through the local HTTP API or the
`invo schedule` CLI. Definitions and run state are persisted outside Nix;
create, update, enable, disable, and delete operations take effect in the running
service without a restart. New installations start with no schedules.

The service configuration remains TOML. API resources, task input files, and CLI
output use JSON. Nix and TOML configure only the service, such as the API address,
agent, multiplexer, and polling intervals. Jobs are not declared in either file.

With the service running, create an agent task:

```bash
invo schedule create weekday-review \
  --cron '0 9 * * MON-FRI' --timezone America/New_York \
  --directory ~/repos/my-project \
  --prompt 'Review recent changes and follow AGENTS.md.'
```

Create a command task; everything after `--` is literal command argv:

```bash
invo schedule create fetch-updates \
  --every-seconds 3600 --directory ~/repos/my-project \
  -- git fetch --all
```

Read and modify schedules:

```bash
invo schedule list
invo schedule get weekday-review
invo schedule update weekday-review --cron '0 10 * * MON-FRI'
invo schedule disable weekday-review
invo schedule enable weekday-review
invo schedule delete weekday-review
```

`create --file task.json` accepts a task object; `update NAME --file patch.json`
accepts a partial task object. Use `--file -` to read JSON from stdin.
`replace NAME --file task.json` replaces a definition. Output is always JSON,
and CLI edits use the resource revision to prevent concurrent lost updates.
See `invo schedule --help` and the [HTTP API reference](docs/api.md).

The API defaults to `http://127.0.0.1:7639`. Its address and private token are
discovered automatically from `$XDG_STATE_HOME/invocateur/api.json` and
`api-token` (normally under `~/.local/state/invocateur`). Pass
`invo --state-dir /path schedule ...` for another service state directory,
or use `--url` and `--token-file`. The environment variables
`INVOCATEUR_API_URL` and `INVOCATEUR_API_TOKEN` are also supported.
The CLI talks to the API, so the service must be running.

Service settings, if needed:

```toml
api_enabled = true
api_listen = "127.0.0.1:7639"
schedule_check_interval_seconds = 1
# Disable the GitHub trigger for an API/schedule-only service:
github_enabled = false
```

`services.invocateur.apiEnabled`, `apiListen`, and `scheduleCheckInterval` expose
the same controls in Home Manager. A new profile does not replace the schedule
store. Keep executables needed by tasks in the service's PATH; Nix users can add
them with `services.invocateur.extraPackages`.

Each task needs a unique `name`. Enabled tasks require exactly one of
`cron` / `every_seconds` and exactly one of `prompt` / `command`. Cron uses an IANA timezone (default `UTC`);
intervals wait until the previous dispatch finishes before counting down again.
The directory defaults to `repos_directory` and must already exist; scheduled
tasks do not clone repositories. Agent tasks use the service's configured agent,
arguments, multiplexer, and existing session.

New tasks first run at the next cron time or after one interval. Pass
`--run-immediately` on create or update to make the next occurrence due now.
Missed occurrences coalesce into one run, and restarts do not replay the same
occurrence. The scheduler checks for dynamic changes every second by default,
independently of GitHub polling. Schedules are machine-local.

The API remains responsive while tasks execute. Edits and deletion of a task
currently dispatching return HTTP 409; retry after dispatch finishes. Other tasks
can be created and edited meanwhile. Command tasks run serially, with a timeout
that kills their process group. Long commands can delay polling and dispatch;
agent sessions continue in the background and may overlap future occurrences.

Interrupted or ambiguous dispatches pause with `needs-attention`. Inspect the
resource's `last_run` and the process or pane, then use
`invo schedule resume NAME` after resolving the previous work. Resume
schedules a future occurrence; it does not replay the interrupted one. Failed
completed commands wait for their next occurrence. Deleting a schedule removes
future work and its stored history; it does not cancel an agent already running.

## Triggers

The service schedules triggers through the `Trigger` interface in
[`src/triggers/mod.rs`](src/triggers/mod.rs). Each trigger owns its prerequisite
checks, state, and task dispatch; task types are not restricted by the scheduler.
Built-in triggers are [GitHub](src/triggers/github.rs) and
[scheduled tasks](src/triggers/schedule.rs). Future triggers can implement
this interface for other sources and tasks without depending on GitHub or `gh`.

If `gh` is missing or its authenticated account cannot be verified, the GitHub
trigger logs an error and skips its poll before reading repositories or claiming
work. The daemon stays running and retries at the next polling interval; other
registered triggers still run. Install `gh` (or set `gh_path`) and use
`gh auth login --hostname github.com` as needed. Token authentication through
`GH_TOKEN` also works; the trigger verifies identity with `gh api user` rather
than requiring credentials in a particular local login store.

`--once` polls every registered trigger and returns a nonzero status if any
trigger fails. `--check-config` does not require `gh` or a GitHub login.

## Design references

- [Claude Code Action tracking comments](https://github.com/anthropics/claude-code-action/blob/d535f5172dec02b204866b393d6b20b9b2aaf98d/src/github/operations/comments/create-initial.ts)
  provide a useful reference for acknowledgement and progress reporting. Its
  [trigger matcher](https://github.com/anthropics/claude-code-action/blob/d535f5172dec02b204866b393d6b20b9b2aaf98d/src/github/validation/trigger.ts)
  uses regular expressions; Invocateur independently parses Markdown to keep
  quoted examples from triggering agents.
- OpenHands Automation separates
  [event deduplication](https://github.com/OpenHands/automation/blob/407bf8a021cce90656ee8d92292bd742b550080f/openhands/automation/ingest.py)
  from [atomic worker claims and durable run state](https://github.com/OpenHands/automation/blob/407bf8a021cce90656ee8d92292bd742b550080f/openhands/automation/dispatcher.py).
  Invocateur follows that separation with a GitHub reaction claim, durable local
  launch records, and a separate acknowledgement comment. The comment text is
  a status message, not the mutual-exclusion mechanism.

## Development

```bash
nix develop
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
nix build .#invocateur
```

The [multiplexer adapter contracts](docs/multiplexers.md) describe the typed
request/response boundary, native identifier validation, error handling, and
durable launch checkpoints.
