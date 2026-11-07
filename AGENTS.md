This is a Rust daemon with independent GitHub and scheduled-task triggers. GitHub
uses the local `gh` CLI. Agent tasks start in Herdr; schedules can also run
literal command argv. Schedules are managed dynamically
through the local API and invo schedule CLI, never static service config. The Nix
flake provides a package and a Home Manager systemd user service module.

Use `nix develop` for the development environment. Run `cargo fmt --check`,
`cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`,
and `nix build` before shipping changes. Also run `nix develop -c actionlint`
and `nix develop -c alejandra --check .` for workflow and Nix changes. CI runs
checks across all supported systems. Keep Actions pinned to full commit SHAs;
privileged dependency automation must not check out or execute pull request code.
Tests use simulated CLIs; do not start live agents or post GitHub comments while testing.

Keep the bot name, agent selection, repository paths, and polling intervals
configurable with defaults. Preserve author verification and durable launch
deduplication, including per-comment claims for input forwarded to an existing
conversation agent. Herdr is the only supported multiplexer. Test observable
behavior rather than implementation details.
