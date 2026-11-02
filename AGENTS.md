This is a Rust daemon with independent GitHub and scheduled-task triggers. GitHub
uses the local `gh` CLI. Agent tasks start in Herdr (default), tmux, or Zellij;
schedules can also run literal command argv. Schedules are managed dynamically
through the local API and invo schedule CLI, never static service config. The Nix flake provides a package and
a Home Manager systemd user service module.

Use `nix develop` for the development environment. Run `cargo fmt --check`,
`cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`,
and `nix build` before shipping changes. Tests use simulated CLIs; do not start
live agents or post GitHub comments while testing.

Keep the bot name, agent selection, repository paths, and polling intervals
configurable with defaults. Preserve author verification and durable launch
deduplication. Test observable behavior rather than implementation details.
