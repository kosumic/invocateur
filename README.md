# invocateur

A Rust daemon and Home Manager systemd user service. It polls GitHub through your
local authenticated `gh` CLI and starts a coding agent in a background Herdr
workspace when **you** post a configured spell.

## Install without Nix

Install a current stable Rust toolchain (Cargo), a C compiler/linker, Git,
[GitHub CLI](https://cli.github.com/), [Herdr](https://herdr.dev/), and the coding
agent you want to use. The agent executable must be available to the Herdr server.

Install Invocateur and authenticate with GitHub:

```bash
cargo install --locked --git https://github.com/kosumic/invocateur
gh auth login
mkdir -p ~/.config/invocateur
```

Create `~/.config/invocateur/config.json`:

```json
{
  "spells": ["summon"],
  "agent": "codex",
  "repos_directory": "~/repos",
  "poll_interval_seconds": 60,
  "acknowledgement_message": "🪄 Invocateur started {agent} for this task.\n\nTriggered by {comment_url}."
}
```

These are the defaults; all settings are optional in the JSON file. Standalone
settings use snake_case names, such as `agent_args`, `herdr_session`,
`initial_lookback_seconds`, and `discover_github_repositories`. Additional
checkout paths can be configured with `"repositories": {"owner/repo": "~/path"}`.

Validate the configuration without contacting GitHub or starting agents:

```bash
~/.cargo/bin/invocateur --config ~/.config/invocateur/config.json --check-config
```

Start Herdr normally, leave its default session running, then run Invocateur:

```bash
~/.cargo/bin/invocateur --config ~/.config/invocateur/config.json
```

Keep this process running to poll for new spell comments. Set `herdr_session`
if you use a different existing Herdr session. Invocateur does not start Herdr.

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
ExecStart=%h/.cargo/bin/invocateur --config %h/.config/invocateur/config.json
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

After editing `config.json`, restart the service:

```bash
systemctl --user restart invocateur
```

Keep Herdr running when using the service. On Unix systems without systemd, use the
foreground command above or configure your operating system's process manager.

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
  spells = [ "summon" ];
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

All four settings have defaults: `summon`, `codex`, `~/repos`, and 60 seconds.
`agent` is a Herdr-supported agent kind, such as `codex` or `claude`. Install the
chosen agent in the Herdr server's environment; `extraPackages` also makes packages
available to the polling service and defaults to `[ pkgs.codex ]`.

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

Customize it with `acknowledgement_message` in JSON or
`services.invocateur.acknowledgementMessage` in Home Manager. The nonempty message
supports `{agent}`, `{comment_url}`, `{repository}`, and `{issue_number}`. You can
include a machine name directly in each machine's message. A hidden marker keeps
acknowledgement comments from triggering agents, even if a custom message includes
a spell on its own line. Inline review triggers are acknowledged in the PR's
conversation, with a link back to the triggering review comment.

Before starting an agent, Invocateur claims the triggering comment by adding a 👀
reaction as the authenticated GitHub account. GitHub's
[reaction API](https://docs.github.com/en/rest/reactions/reactions#create-reaction-for-an-issue-comment)
returns `201` for a new reaction and `200` when that account already added it.
Only the service receiving `201` starts an agent. This coordinates simultaneous
services without relying on a shared filesystem or a race-prone comment lookup.

Run the updated version on every machine, using the same `gh` account. The claim
is per triggering comment; a new spell comment requests another agent. Reserve
your own 👀 reaction on spell comments for Invocateur: an existing one counts as
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
summon
Fix the crash described here and check the relevant build.
```

Spells are case-sensitive standalone lines. Multiple spells can be configured.
Markdown blockquotes (including nested quotes and lazy continuation lines without
another `>`), fenced/indented code blocks, inline code, and quoted strings such as
`"summon"` do not trigger agents. Quote boundaries are parsed with CommonMark;
a fresh unquoted spell after a Markdown quote still works. Comments containing
raw HTML `<blockquote>` or `<q>` containers are ignored entirely. Only comments from
the account returned by `gh api user` are accepted. Issue/PR descriptions and
review summary bodies are not triggers. The agent receives the issue/PR title,
description, comment link, and the rest of your comment as its task; each text
section is limited to 24,000 UTF-8 bytes.

The service searches for your recently updated conversations and reviews across
GitHub and watches their repositories. Set `discoverGithubRepositories = false`
to restrict polling to local and explicitly configured repositories. Search
indexing can add delay beyond the polling interval. Incomplete search responses
or results above GitHub's 1,000-result limit are reported without advancing the
discovery cursor, so results are not silently discarded. Already discovered
repositories continue to be polled if discovery fails.

The service also discovers GitHub `origin` remotes under `reposDirectory`, in both
`~/repos/project` and `~/repos/owner/project` layouts. Discovery runs on every poll.
Explicit `repositories` entries override discovered paths. A missing checkout is
shallow cloned on the first matching comment unless `cloneMissing = false`.
Repositories found through GitHub use `reposDirectory/owner/repo` if there is no
matching local checkout, which avoids collisions between owners. Existing checkouts are reused without pulling, resetting,
or creating worktrees; origin must match the watched repository.

The [GitHub comment API](https://docs.github.com/en/rest/issues/comments)
includes issue and PR conversation comments. Inline review comments use the
separate review-comment endpoint; set `reviewComments = false` to disable those.
Each poll makes one identity request, two paginated GitHub searches, and at least
two paginated requests per watched repository. Discovery uses GitHub's
[commenter and review search qualifiers](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests),
including private repositories accessible through your `gh` login.
`githubHost` defaults to `github.com` and supports authenticated enterprise hosts.

On first seeing a repository the daemon processes future comment updates only.
Set `initialLookback = 3600` to include the previous hour for newly watched
repositories. Subsequent restarts resume the saved cursor, including downtime.
Polling uses a five-minute overlap for delayed API visibility. Editing a comment
to add a spell can trigger it, but each comment launches at most once, even if
edited again. A new comment summons another agent.

State lives in `$XDG_STATE_HOME/invocateur/state.json` (normally
`~/.local/state/invocateur/state.json`), with a lock preventing concurrent
instances. State is written before launching. Interrupted or ambiguous Herdr
operations are recorded as `reserved`, `claimed`, or `needs-attention` and are never retried
automatically. Inspect the saved pane/agent and logs; continue the existing agent
manually, or post a new spell comment after resolving the problem. An unavailable
Herdr server or failed GitHub read is retried on the next poll. Do not delete state
to retry a launch: that loses deduplication history. State retains launch records
so old comment edits cannot accidentally launch duplicate agents.

```bash
systemctl --user status invocateur
journalctl --user -u invocateur -f
```

## Standalone CLI options

The package and reusable module are exported as `packages.<system>.invocateur`
and `homeManagerModules.default`. The standalone binary accepts JSON settings
with snake_case names, with the same defaults as the module:

```bash
nix run github:kosumic/invocateur -- --config /path/to/config.json --check-config
nix run github:kosumic/invocateur -- --config /path/to/config.json --once
```

`--once` performs one poll without sleeping. `--check-config` validates settings
without using GitHub or Herdr. `--state-dir /absolute/path` overrides the state
directory. Use a separate state directory for manual checks while the service runs.
Launch records also retain the acknowledgement URL or acknowledgement error;
`claimed-elsewhere` means the shared GitHub reaction was already present.

```json
{
  "spells": ["summon"],
  "agent": "codex",
  "repos_directory": "~/repos",
  "poll_interval_seconds": 60
}
```

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
