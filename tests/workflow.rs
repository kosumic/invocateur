//! Behavioral tests drive the binary through fake local CLIs; no network or live agent is used.
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "invocateur-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("checkout/.git")).unwrap();
        let f = Self { root };
        f.write("issues.json", &json!([[]]));
        f.write("reviews.json", &json!([[]]));
        f.write(
            "search.json",
            &json!([{"total_count":0,"incomplete_results":false,"items":[]}]),
        );
        let shell = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .unwrap();
        for (name, script) in [
            (
                "gh",
                r#"
method=GET
body=
content=
previous=
for value do
  if test "$previous" = '--method'; then method="$value"; fi
  if test "$previous" = '--raw-field'; then
    case "$value" in body=*) body="${value#body=}" ;; content=*) content="${value#content=}" ;; esac
  fi
  previous="$value"
done
for arg do
  case "$arg" in
    clone)
      mkdir -p "$4/.git"
      printf '%s\n' "$4" >> "$FIXTURE/clones"
      exit 0 ;;
    user) printf '{"login":"alice"}'; exit 0 ;;
    search/issues\?*) cat "$FIXTURE/search.json"; exit 0 ;;
    repos/alice/project/issues/comments\?*) cat "$FIXTURE/issues.json"; exit 0 ;;
    repos/alice/project/pulls/comments\?*)
      if test -f "$FIXTURE/api-fails"; then echo 'rate limited' >&2; exit 1; fi
      cat "$FIXTURE/reviews.json"; exit 0 ;;
    repos/alice/project/*/comments/*/reactions)
      test "$method" = POST && test "$content" = eyes || exit 2
      if test -f "$FIXTURE/claim-fails"; then echo 'claim forbidden' >&2; exit 1; fi
      if test -f "$FIXTURE/claim-barrier"; then
        mkdir "$FIXTURE/claim-ready-$$"
        attempts=0
        while test "$attempts" -lt 500; do
          set -- "$FIXTURE"/claim-ready-*
          if test "$#" -ge 2; then break; fi
          attempts=$((attempts + 1))
          sleep 0.01
        done
        test "$attempts" -lt 500 || exit 3
      fi
      key="${arg#repos/alice/project/}"
      key=$(printf '%s' "$key" | tr / _)
      if mkdir "$FIXTURE/reaction-$key" 2>/dev/null; then
        if test -f "$FIXTURE/claim-lost-response"; then echo 'connection lost after claim' >&2; exit 1; fi
        status='201 Created'
      else status='200 OK'; fi
      printf 'HTTP/2.0 %s\r\nContent-Type: application/json\r\n\r\n{"id":7,"content":"eyes","user":{"login":"alice"}}' "$status"
      exit 0 ;;
    repos/alice/project/issues/42/comments)
      test "$method" = POST && test -s "$FIXTURE/prompts" || exit 2
      if test -f "$FIXTURE/ack-fails"; then echo 'comment forbidden' >&2; exit 1; fi
      count=0
      if test -f "$FIXTURE/ack-count"; then read -r count < "$FIXTURE/ack-count"; fi
      count=$((count + 1))
      printf '%s\n' "$count" > "$FIXTURE/ack-count"
      printf '%s' "$body" > "$FIXTURE/ack-$count.txt"
      if test -f "$FIXTURE/ack-lost-response"; then echo 'connection lost after comment' >&2; exit 1; fi
      printf '{"html_url":"https://github.com/alice/project/issues/42#issuecomment-ack-%s"}' "$count"
      exit 0 ;;
    repos/alice/project/issues/42) printf '{"title":"Fix the bug","body":"Issue context"}'; exit 0 ;;
  esac
done
echo "unexpected gh request: $*" >&2
exit 2
"#,
            ),
            ("git", r#"printf 'git@github.com:alice/project.git\n'"#),
            (
                "herdr",
                r#"
test "$1" = '--session' && test "$2" = 'test-session' || exit 2
shift 2
case "$1 $2" in
  'workspace list')
    if test -f "$FIXTURE/server-down"; then exit 1; fi
    printf '{"result":{"workspaces":[]}}' ;;
  'workspace create')
    printf 'workspace\n' >> "$FIXTURE/launches"
    printf '%s\n' "$@" >> "$FIXTURE/workspace-args"
    printf '{"result":{"root_pane":{"pane_id":"w7:p9"}}}' ;;
  'agent start')
    printf '%s\n' "$@" >> "$FIXTURE/start-args"
    if test -f "$FIXTURE/agent-fails"; then echo 'agent_not_ready' >&2; exit 1; fi
    printf '{"result":{}}' ;;
  'agent prompt')
    printf '%s\n' "$4" >> "$FIXTURE/prompts"
    printf '{"result":{}}' ;;
  *) exit 2 ;;
esac
"#,
            ),
        ] {
            let path = f.root.join(name);
            fs::write(&path, format!("#!{}\nset -eu\n{script}\n", shell.display())).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        f.config(json!({}));
        f
    }
    fn write(&self, name: &str, value: &Value) {
        fs::write(self.root.join(name), serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn config(&self, overrides: Value) {
        let mut cfg = json!({
            "repositories": { "alice/project": self.root.join("checkout") },
            "discover_repositories": false,
            "discover_github_repositories": false,
            "initial_lookback_seconds": 3600,
            "herdr_session": "test-session",
            "gh_path": self.root.join("gh"),
            "git_path": self.root.join("git"),
            "herdr_path": self.root.join("herdr"),
        });
        cfg.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        self.write("config.json", &cfg);
    }
    fn command(&self, state: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_invocateur"));
        command
            .arg("--config")
            .arg(self.root.join("config.json"))
            .arg("--state-dir")
            .arg(self.root.join(state))
            .arg("--once")
            .env("FIXTURE", &self.root);
        command
    }
    fn run(&self) -> Output {
        self.command("state").output().unwrap()
    }
    fn success(&self) {
        let output = self.run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn launches(&self) -> usize {
        fs::read_to_string(self.root.join("launches"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn state(&self) -> Value {
        self.state_for("state")
    }
    fn state_for(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.root.join(name).join("state.json")).unwrap()).unwrap()
    }
    fn acknowledgements(&self) -> usize {
        fs::read_to_string(self.root.join("ack-count"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }
    fn comment(id: u64, author: &str, body: &str) -> Value {
        json!({"id":id, "user":{"login":author}, "body":body,
            "updated_at": (Utc::now() - Duration::seconds(10)).to_rfc3339(),
            "html_url":format!("https://github.com/alice/project/issues/42#issuecomment-{id}"),
            "issue_url":"https://api.github.com/repos/alice/project/issues/42"})
    }
}

#[test]
fn own_standalone_spells_only_paginated_and_deduplicated_across_restarts() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([
            [
                Fixture::comment(1, "mallory", "summon"),
                Fixture::comment(2, "alice", "please summon this")
            ],
            [
                Fixture::comment(3, "alice", "> summon"),
                Fixture::comment(4, "alice", "```text\nsummon\n```"),
                Fixture::comment(5, "alice", "    summon"),
                Fixture::comment(
                    6,
                    "ALICE",
                    "summon\nFix this; $(touch /tmp/never) `literal`\nwith a second line"
                )
            ]
        ]),
    );
    f.success();
    f.success();
    assert_eq!(f.launches(), 1);
    let prompt = fs::read_to_string(f.root.join("prompts")).unwrap();
    assert!(
        prompt.contains("Issue context")
            && prompt.contains("Fix this; $(touch /tmp/never) `literal`\nwith a second line")
    );
    let start = fs::read_to_string(f.root.join("start-args")).unwrap();
    assert!(start.contains("codex") && start.contains("w7:p9"));
    assert_eq!(f.acknowledgements(), 1);
    let ack = fs::read_to_string(f.root.join("ack-1.txt")).unwrap();
    assert!(ack.contains("Invocateur started codex") && ack.contains("#issuecomment-6"));
    assert_eq!(
        f.state()["repositories"]["github.com/alice/alice/project"]["launches"]["i-6"]["acknowledgement_url"],
        "https://github.com/alice/project/issues/42#issuecomment-ack-1"
    );
}

#[test]
fn configurable_spell_agent_arguments_directory_and_interval() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("custom/repos/project/.git")).unwrap();
    f.config(json!({"spells":["abracadabra", "/fix"], "agent":"claude", "agent_args":["--model", "example"],
        "poll_interval_seconds":7, "repos_directory":f.root.join("custom/repos"), "repositories":{}, "discover_repositories":true}));
    f.write(
        "issues.json",
        &json!([[
            Fixture::comment(1, "alice", "summon"),
            Fixture::comment(2, "alice", "/fix\nDo the work")
        ]]),
    );
    f.success();
    assert_eq!(f.launches(), 1);
    let start = fs::read_to_string(f.root.join("start-args")).unwrap();
    assert!(start.contains("claude\n") && start.contains("--\n--model\nexample"));
    // The daemon must actually wait for its configured interval between polls.
    f.config(json!({"poll_interval_seconds":1, "initial_lookback_seconds":0}));
    let mut child = Command::new(env!("CARGO_BIN_EXE_invocateur"))
        .arg("--config")
        .arg(f.root.join("config.json"))
        .arg("--state-dir")
        .arg(f.root.join("daemon-state"))
        .env("FIXTURE", &f.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let state_path = f.root.join("daemon-state/state.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut cursors = std::collections::BTreeSet::new();
    while std::time::Instant::now() < deadline && cursors.len() < 2 {
        if let Ok(bytes) = fs::read(&state_path)
            && let Ok(state) = serde_json::from_slice::<Value>(&bytes)
            && let Some(cursor) =
                state["repositories"]["github.com/alice/alice/project"]["cursor"].as_str()
        {
            cursors.insert(cursor.to_owned());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        cursors.len(),
        2,
        "daemon did not poll twice at a one-second interval"
    );
}

#[test]
fn inline_review_comments_and_edits_can_summon() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "ordinary comment")]]),
    );
    f.success();
    assert_eq!(f.launches(), 0);
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon\nEdited task")]]),
    );
    let mut review = Fixture::comment(1, "alice", "summon\nFix this line");
    review.as_object_mut().unwrap().remove("issue_url");
    review["pull_request_url"] = json!("https://api.github.com/repos/alice/project/pulls/42");
    f.write("reviews.json", &json!([[review]]));
    f.success();
    f.success();
    assert_eq!(
        f.launches(),
        2,
        "issue and review comment IDs must be independent"
    );
    assert_eq!(f.acknowledgements(), 2);
}

#[test]
fn default_startup_ignores_history() {
    let f = Fixture::new();
    f.config(json!({"initial_lookback_seconds":0}));
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    f.success();
    assert_eq!(f.launches(), 0);
    let mut future = Fixture::comment(2, "alice", "summon");
    future["updated_at"] = json!((Utc::now() + Duration::seconds(1)).to_rfc3339());
    f.write("issues.json", &json!([[future]]));
    f.success();
    assert_eq!(f.launches(), 1);
}

#[test]
fn failed_poll_or_unavailable_server_is_retried_without_losing_comments() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    fs::write(f.root.join("api-fails"), "").unwrap();
    assert!(!f.run().status.success());
    assert_eq!(f.launches(), 0);
    fs::remove_file(f.root.join("api-fails")).unwrap();
    fs::write(f.root.join("server-down"), "").unwrap();
    assert!(!f.run().status.success());
    fs::remove_file(f.root.join("server-down")).unwrap();
    f.success();
    assert_eq!(f.launches(), 1);
}

#[test]
fn ambiguous_launch_failure_is_recorded_and_never_automatically_duplicated() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    fs::write(f.root.join("agent-fails"), "").unwrap();
    f.success();
    fs::remove_file(f.root.join("agent-fails")).unwrap();
    f.success();
    assert!(
        f.command("another-machine")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(f.launches(), 1);
    let record = &f.state()["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"];
    assert_eq!(record["status"], "needs-attention");
    assert_eq!(record["pane"], "w7:p9");
    assert_eq!(f.acknowledgements(), 0);
}

#[test]
fn invalid_config_and_corrupted_state_fail_without_launching() {
    let f = Fixture::new();
    f.config(json!({"poll_interval_seconds":0}));
    assert!(!f.run().status.success());
    f.config(json!({"spells":[""]}));
    assert!(!f.run().status.success());
    f.config(json!({"acknowledgement_message":"  "}));
    assert!(!f.run().status.success());
    f.config(json!({}));
    fs::create_dir_all(f.root.join("state")).unwrap();
    fs::write(f.root.join("state/state.json"), "corrupted").unwrap();
    assert!(!f.run().status.success());
    assert_eq!(f.launches(), 0);
}

#[test]
fn comments_across_github_discover_and_clone_new_repositories() {
    let f = Fixture::new();
    f.config(
        json!({"repositories":{}, "discover_github_repositories":true, "initial_lookback_seconds":0,
        "repos_directory":f.root.join("repos")}),
    );
    f.success(); // Establish the global cutoff before GitHub indexes the new comment.
    f.write(
        "search.json",
        &json!([{"total_count":1,"incomplete_results":false,"items":[
        {"repository_url":"https://api.github.com/repos/alice/project"}]}]),
    );
    let mut comment = Fixture::comment(1, "alice", "summon\nImplement the fix");
    comment["updated_at"] = json!((Utc::now() + Duration::seconds(1)).to_rfc3339());
    f.write("issues.json", &json!([[comment]]));
    // A failed read after discovery must not forget the repository when search moves on.
    fs::write(f.root.join("api-fails"), "").unwrap();
    assert!(!f.run().status.success());
    fs::remove_file(f.root.join("api-fails")).unwrap();
    f.write(
        "search.json",
        &json!([{"total_count":0,"incomplete_results":false,"items":[]}]),
    );
    f.success();
    f.success();
    assert_eq!(f.launches(), 1);
    let clones = fs::read_to_string(f.root.join("clones")).unwrap();
    assert_eq!(
        clones.lines().collect::<Vec<_>>(),
        [f.root.join("repos/alice/project").to_str().unwrap()]
    );
}

#[test]
fn global_discovery_reuses_matching_checkout_and_respects_clone_missing() {
    let f = Fixture::new();
    let directory = f.root.join("repos/existing-project");
    fs::create_dir_all(directory.join(".git")).unwrap();
    f.config(
        json!({"repositories":{}, "discover_github_repositories":true, "discover_repositories":true,
        "repos_directory":f.root.join("repos")}),
    );
    f.write(
        "search.json",
        &json!([{"total_count":1,"incomplete_results":false,"items":[
        {"repository_url":"https://api.github.com/repos/alice/project"}]}]),
    );
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    f.success();
    assert_eq!(f.launches(), 1);
    assert!(!f.root.join("clones").exists());
    assert!(
        fs::read_to_string(f.root.join("workspace-args"))
            .unwrap()
            .contains(directory.to_str().unwrap())
    );

    let other = Fixture::new();
    other.config(
        json!({"repositories":{}, "discover_github_repositories":true, "clone_missing":false,
        "repos_directory":other.root.join("repos")}),
    );
    other.write(
        "search.json",
        &json!([{"total_count":1,"incomplete_results":false,"items":[
        {"repository_url":"https://api.github.com/repos/alice/project"}]}]),
    );
    other.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    assert!(!other.run().status.success());
    assert!(!other.root.join("clones").exists());
    assert_eq!(other.launches(), 0);
}

#[test]
fn simultaneous_services_with_independent_state_launch_and_acknowledge_once() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    fs::write(f.root.join("claim-barrier"), "").unwrap();
    let first = f.command("machine-a").spawn().unwrap();
    let second = f.command("machine-b").spawn().unwrap();
    assert!(first.wait_with_output().unwrap().status.success());
    assert!(second.wait_with_output().unwrap().status.success());
    assert_eq!(f.launches(), 1);
    assert_eq!(f.acknowledgements(), 1);
    let statuses: std::collections::BTreeSet<_> = ["machine-a", "machine-b"].into_iter().map(|name| {
        f.state_for(name)["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"]["status"].as_str().unwrap().to_owned()
    }).collect();
    assert_eq!(
        statuses,
        ["started".to_owned(), "claimed-elsewhere".to_owned()].into()
    );
    fs::remove_file(f.root.join("claim-barrier")).unwrap();
    // A fresh machine has no local history and must still respect the shared claim.
    assert!(f.command("machine-c").output().unwrap().status.success());
    assert_eq!(f.launches(), 1);
    assert_eq!(f.acknowledgements(), 1);
}

#[test]
fn custom_acknowledgements_expand_placeholders_and_cannot_summon_agents() {
    let f = Fixture::new();
    f.config(json!({"acknowledgement_message":"summon\n{agent} is working on {repository}#{issue_number}\n{comment_url}"}));
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "summon")]]),
    );
    f.success();
    let body = fs::read_to_string(f.root.join("ack-1.txt")).unwrap();
    assert!(body.contains("summon\ncodex is working on alice/project#42\nhttps://github.com/alice/project/issues/42#issuecomment-1"));
    f.write(
        "issues.json",
        &json!([[
            Fixture::comment(1, "alice", "summon"),
            Fixture::comment(2, "alice", &body)
        ]]),
    );
    assert!(
        f.command("other-machine")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        f.launches(),
        1,
        "acknowledgements must never become new tasks"
    );
    assert_eq!(f.acknowledgements(), 1);
}

#[test]
fn a_failed_or_ambiguous_claim_never_launches_an_agent() {
    for failure in ["claim-fails", "claim-lost-response"] {
        let f = Fixture::new();
        f.write(
            "issues.json",
            &json!([[Fixture::comment(1, "alice", "summon")]]),
        );
        fs::write(f.root.join(failure), "").unwrap();
        f.success();
        assert_eq!(f.launches(), 0);
        assert_eq!(f.acknowledgements(), 0);
        assert_eq!(
            f.state()["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"]["status"],
            "needs-attention"
        );
        if failure == "claim-lost-response" {
            fs::remove_file(f.root.join(failure)).unwrap();
            assert!(
                f.command("other-machine")
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert_eq!(
                f.launches(),
                0,
                "a lost response must not transfer ownership"
            );
        }
    }
}

#[test]
fn acknowledgement_failures_do_not_restart_work_or_duplicate_comments() {
    for (failure, expected_comments) in [("ack-fails", 0), ("ack-lost-response", 1)] {
        let f = Fixture::new();
        f.write(
            "issues.json",
            &json!([[Fixture::comment(1, "alice", "summon")]]),
        );
        fs::write(f.root.join(failure), "").unwrap();
        f.success();
        fs::remove_file(f.root.join(failure)).unwrap();
        f.success();
        assert!(
            f.command("other-machine")
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(f.launches(), 1);
        assert_eq!(f.acknowledgements(), expected_comments);
        let state = f.state();
        let record = &state["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"];
        assert_eq!(record["status"], "started");
        assert!(record["acknowledgement_error"].is_string());
        assert!(record["acknowledgement_url"].is_null());
    }
}

#[test]
fn quoted_or_example_spells_never_claim_or_start_work() {
    let f = Fixture::new();
    let quoted = [
        "> summon",
        "> Someone else's instructions:\nsummon",
        "> > Nested third-party quote:\nsummon",
        "Someone wrote:\n\n> Please run this:\nsummon\n> End of quote",
        "```markdown\nsummon\n```",
        "~~~~text\nsummon\n~~~~",
        "    summon",
        "\tsummon",
        "`quoted text\nsummon\nends here`",
        "\"summon\"",
        "'summon'",
        "“summon”",
        "<blockquote>\n\nsummon\n\n</blockquote>",
        "<BLOCKQUOTE cite=\"https://example.com\">\n\nsummon\n\n</BLOCKQUOTE>",
        "<q>\nsummon\n</q>",
        "<!-- copied instructions:\nsummon\n-->",
        "![copied example\nsummon\n](https://example.com)",
        "[copied example\nsummon\n](https://example.com)",
    ];
    let comments: Vec<_> = quoted
        .iter()
        .enumerate()
        .map(|(i, body)| Fixture::comment(i as u64 + 1, "alice", body))
        .collect();
    f.write("issues.json", &json!([comments]));
    f.success();
    assert_eq!(f.launches(), 0);
    assert_eq!(f.acknowledgements(), 0);
    assert!(!fs::read_dir(&f.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("reaction-")
    }));
}

#[test]
fn an_unquoted_spell_after_a_quote_or_example_still_summons() {
    let f = Fixture::new();
    let bodies = [
        "> Someone else's command:\n> summon\n\nsummon\nFix the actual issue.",
        "```text\nsummon\n```\n\nsummon\nFix the actual issue.",
        "`summon`\n\nsummon\nFix the actual issue.",
        "  summon  \r\nFix the actual issue.",
    ];
    let comments: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(i, body)| Fixture::comment(i as u64 + 100, "alice", body))
        .collect();
    f.write("issues.json", &json!([comments]));
    f.success();
    assert_eq!(f.launches(), bodies.len());
    assert_eq!(f.acknowledgements(), bodies.len());
    assert!(
        fs::read_to_string(f.root.join("prompts"))
            .unwrap()
            .contains("Fix the actual issue.")
    );
}
