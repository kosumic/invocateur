# invocateur

A Rust daemon and Home Manager systemd user service. It polls GitHub through your
local authenticated `gh` CLI and starts a coding agent in a background Herdr
workspace when **you** post a configured spell.

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

Post a comment on an issue, PR conversation, or inline PR review:

```text
summon
Fix the crash described here and check the relevant build.
```

Spells are case-sensitive standalone lines. Multiple spells can be configured.
Quoted lines and Markdown code blocks do not trigger agents. Only comments from
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
operations are recorded as `reserved` or `needs-attention` and are never retried
automatically. Inspect the saved pane/agent and logs; continue the existing agent
manually, or post a new spell comment after resolving the problem. An unavailable
Herdr server or failed GitHub read is retried on the next poll. Do not delete state
to retry a launch: that loses deduplication history. State retains launch records
so old comment edits cannot accidentally launch duplicate agents.

```bash
systemctl --user status invocateur
journalctl --user -u invocateur -f
```

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

```json
{
  "spells": ["summon"],
  "agent": "codex",
  "repos_directory": "~/repos",
  "poll_interval_seconds": 60
}
```

Development checks:

```bash
nix develop
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
nix build .#invocateur
```
