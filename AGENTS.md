This is a Rust daemon that polls the user's GitHub comments with the local `gh`
CLI and starts coding agents in Herdr. The Nix flake provides a package and a
Home Manager systemd user service module.

Use `nix develop` for the development environment. Run `cargo fmt --check`,
`cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`,
and `nix build` before shipping changes. Tests use simulated CLIs; do not start
live agents or post GitHub comments while testing.

Keep spell matching, agent selection, repository paths, and polling intervals
configurable with defaults. Preserve author verification and durable launch
deduplication. Test observable behavior rather than implementation details.
