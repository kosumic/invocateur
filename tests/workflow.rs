//! Behavioral tests drive the binary through fake local CLIs; no network or live agent is used.
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(0);
static MOCK_TOOLS: OnceLock<PathBuf> = OnceLock::new();
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
        // Publish immutable executables before any test can spawn a child.
        // Concurrent fixture writes and process creation can otherwise inherit a writable
        // script descriptor briefly and make exec fail with ETXTBSY on Linux.
        MOCK_TOOLS.get_or_init(|| {
        let directory = PathBuf::from(option_env!("CARGO_TARGET_TMPDIR").unwrap_or("target/tmp"))
            .join(format!("invocateur-tools-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
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
    user)
      printf 'user\n' >> "$FIXTURE/user-calls"
      if test -f "$FIXTURE/gh-logged-out"; then
        echo 'not logged in; run gh auth login' >&2
        exit 1
      fi
      printf '{"login":"alice"}'; exit 0 ;;
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
    printf '{"result":{"tab":{"tab_id":"w7:t1"},"root_pane":{"pane_id":"w7:p9"}}}' ;;
  'tab rename')
    test "$3" = w7:t1 || exit 2
    printf '%s\n' "$4" >> "$FIXTURE/tab-names"
    printf '{"result":{}}' ;;
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
            ("scheduled-command", r#"
printf 'run\n' >> "$FIXTURE/scheduled-runs"
printf '%s\0' "$@" > "$FIXTURE/scheduled-argv"
printf '%s\n' "$PWD" > "$FIXTURE/scheduled-cwd"
case "${1:-}" in
  fail) echo 'scheduled task failed' >&2; exit 1 ;;
  wait) printf '%s\n' "$$" > "$FIXTURE/scheduled-pid"; sleep 30 ;;
esac
printf 'scheduled output\n'
"#),
            ("agent", r#"
printf '%s\0' "$@" > "$FIXTURE/agent-argv"
printf '%s\n' "$PWD" > "$FIXTURE/agent-cwd"
for argument do last="$argument"; done
printf '%s\n' "$last" >> "$FIXTURE/prompts"
"#),
            ("tmux", r#"
case "$1" in
  has-session)
    test "$2" = '-t' && test "$3" = '=test-tmux' || exit 2
    test ! -f "$FIXTURE/server-down"
    exit $? ;;
  new-window)
    printf '%s\n' "$@" > "$FIXTURE/multiplexer-args"
    test ! -f "$FIXTURE/multiplexer-fails" || exit 1
    shift
    directory=
    while test "$#" -gt 0; do
      case "$1" in
        --) shift; break ;;
        -c) directory="$2"; shift 2 ;;
        -d|-P) shift ;;
        *) shift 2 ;;
      esac
    done
    printf 'tmux\n' >> "$FIXTURE/launches"
    (cd "$directory" && "$@")
    printf '%%7\n' ;;
  *) exit 2 ;;
esac
"#),
            ("zellij", r#"
test "$1" = '--session' && test "$2" = 'test-zellij' && test "$3" = action || exit 2
case "$4" in
  query-tab-names)
    test ! -f "$FIXTURE/server-down" || exit 1
    printf 'existing-tab\n' ;;
  new-tab)
    printf '%s\n' "$@" > "$FIXTURE/multiplexer-args"
    test ! -f "$FIXTURE/multiplexer-fails" || exit 1
    shift 4
    directory=
    while test "$#" -gt 0; do
      case "$1" in
        --) shift; break ;;
        --cwd) directory="$2"; shift 2 ;;
        --no-focus) shift ;;
        *) shift 2 ;;
      esac
    done
    printf 'zellij\n' >> "$FIXTURE/launches"
    (cd "$directory" && "$@")
    printf '3\n' ;;
  *) exit 2 ;;
esac
"#),
        ] {
            let path = directory.join(name);
            fs::write(&path, format!("#!{}\nset -eu\n{script}\n", shell.display())).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        directory
        });
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
            "gh_path": MOCK_TOOLS.get().unwrap().join("gh"),
            "git_path": MOCK_TOOLS.get().unwrap().join("git"),
            "herdr_path": MOCK_TOOLS.get().unwrap().join("herdr"),
            "tmux_path": MOCK_TOOLS.get().unwrap().join("tmux"),
            "zellij_path": MOCK_TOOLS.get().unwrap().join("zellij"),
            "tmux_session": "test-tmux",
            "zellij_session": "test-zellij",
        });
        cfg.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        fs::write(
            self.root.join("config.toml"),
            toml::to_string(&cfg).unwrap(),
        )
        .unwrap();
    }
    fn command(&self, state: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_invocateur"));
        command
            .arg("--config")
            .arg(self.root.join("config.toml"))
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
fn own_mentions_only_paginated_and_deduplicated_across_restarts() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([
            [
                Fixture::comment(1, "mallory", "@invo"),
                Fixture::comment(2, "alice", "please summon this")
            ],
            [
                Fixture::comment(3, "alice", "> @invo"),
                Fixture::comment(4, "alice", "```text\n@invo\n```"),
                Fixture::comment(5, "alice", "    @invo"),
                Fixture::comment(
                    6,
                    "ALICE",
                    "@invo\nFix this; $(touch /tmp/never) `literal`\nwith a second line"
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
    assert_eq!(
        fs::read_to_string(f.root.join("tab-names")).unwrap(),
        "gh-i-6\n"
    );
    assert!(
        fs::read_to_string(f.root.join("workspace-args"))
            .unwrap()
            .contains("alice/project#42 gh-i-6")
    );
    assert_eq!(f.acknowledgements(), 1);
    let ack = fs::read_to_string(f.root.join("ack-1.txt")).unwrap();
    assert!(ack.contains("Invocateur started codex") && ack.contains("#issuecomment-6"));
    assert_eq!(
        f.state()["repositories"]["github.com/alice/alice/project"]["launches"]["i-6"]["acknowledgement_url"],
        "https://github.com/alice/project/issues/42#issuecomment-ack-1"
    );
}

#[test]
fn configurable_mention_agent_arguments_directory_and_interval() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("custom/repos/project/.git")).unwrap();
    f.config(json!({"bot_name":"helper", "agent":"claude", "agent_args":["--model", "example"],
        "poll_interval_seconds":7, "repos_directory":f.root.join("custom/repos"), "repositories":{}, "discover_repositories":true}));
    f.write(
        "issues.json",
        &json!([[
            Fixture::comment(1, "alice", "@invo"),
            Fixture::comment(2, "alice", "@helper Do the work")
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
        .arg(f.root.join("config.toml"))
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
        &json!([[Fixture::comment(1, "alice", "@invo\nEdited task")]]),
    );
    let mut review = Fixture::comment(1, "alice", "@invo\nFix this line");
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
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    f.success();
    assert_eq!(f.launches(), 0);
    let mut future = Fixture::comment(2, "alice", "@invo");
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
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
    f.config(json!({"bot_name":""}));
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
    let mut comment = Fixture::comment(1, "alice", "@invo\nImplement the fix");
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
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
    f.config(json!({"acknowledgement_message":"@invo\n{agent} is working on {repository}#{issue_number}\n{comment_url}"}));
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    f.success();
    let body = fs::read_to_string(f.root.join("ack-1.txt")).unwrap();
    assert!(body.contains("@invo\ncodex is working on alice/project#42\nhttps://github.com/alice/project/issues/42#issuecomment-1"));
    f.write(
        "issues.json",
        &json!([[
            Fixture::comment(1, "alice", "@invo"),
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
            &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
            &json!([[Fixture::comment(1, "alice", "@invo")]]),
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
fn quoted_or_example_mentions_never_claim_or_start_work() {
    let f = Fixture::new();
    let quoted = [
        "> @invo",
        "> Someone else's instructions:\n@invo",
        "> > Nested third-party quote:\n@invo",
        "Someone wrote:\n\n> Please run this:\n@invo\n> End of quote",
        "```markdown\n@invo\n```",
        "~~~~text\n@invo\n~~~~",
        "    @invo",
        "\t@invo",
        "`quoted text\n@invo\nends here`",
        "\"@invo\"",
        "'@invo'",
        "“@invo”",
        "Someone said \"please @invo fix this\".",
        "Someone said 'please @invo fix this'.",
        "Someone said ‘please @invo fix this’.",
        "email@invo.com",
        "@invo.example",
        "@invo-other fix this",
        "@invo_extra fix this",
        "@invo2 fix this",
        "@@invo fix this",
        "\\@invo fix this",
        "summon",
        "<blockquote>\n\n@invo\n\n</blockquote>",
        "<BLOCKQUOTE cite=\"https://example.com\">\n\n@invo\n\n</BLOCKQUOTE>",
        "<q>\n@invo\n</q>",
        "<!-- copied instructions:\n@invo\n-->",
        "![copied example\n@invo\n](https://example.com)",
        "[copied example\n@invo\n](https://example.com)",
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
fn an_unquoted_mention_after_a_quote_or_example_still_summons() {
    let f = Fixture::new();
    let bodies = [
        "> Someone else's command:\n> @invo\n\n@invo\nFix the actual issue.",
        "```text\n@invo\n```\n\n@invo\nFix the actual issue.",
        "`@invo`\n\n@invo\nFix the actual issue.",
        "  @invo  \r\nFix the actual issue.",
        "@invo Fix the actual issue.",
        "Please @InVo Fix the actual issue.",
        "I'm asking @invo Fix the actual issue.",
        "(@invo) Fix the actual issue.",
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

#[test]
fn herdr_workspace_and_tab_names_are_configurable_templates() {
    let f = Fixture::new();
    f.config(
        json!({"workspace_name":"Work on {repository}#{issue_number}",
        "tab_name":"{agent}-{agent_name}-{comment_id}-{multiplexer}-{session}"}),
    );
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    f.success();
    assert!(
        fs::read_to_string(f.root.join("workspace-args"))
            .unwrap()
            .contains("Work on alice/project#42")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("tab-names")).unwrap(),
        "codex-gh-i-1-1-herdr-test-session\n"
    );
    let state = f.state();
    let record = &state["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"];
    assert_eq!(record["workspace_name"], "Work on alice/project#42");
    assert_eq!(record["tab_name"], "codex-gh-i-1-1-herdr-test-session");
}

#[test]
fn tmux_and_zellij_launch_native_agents_with_literal_arguments_and_names() {
    for (backend, target) in [("tmux", "%7"), ("zellij", "tab:3")] {
        let f = Fixture::new();
        let injected = f.root.join("must-not-exist");
        let task = format!(
            "Do not execute $(touch {}) or `touch {}`.\nKeep this apostrophe: ' and this semicolon: ;",
            injected.display(),
            injected.display()
        );
        f.config(
            json!({"multiplexer":backend, "agent":MOCK_TOOLS.get().unwrap().join("agent"),
            "agent_args":["--model", "a model with spaces", "literal ' \" $HOME ;"],
            "tab_name":"Review {repository}#{issue_number}",
            "acknowledgement_message":"Working through {multiplexer} in {session}: {comment_url}"}),
        );
        f.write(
            "issues.json",
            &json!([[Fixture::comment(1, "alice", &format!("@invo\n{task}"))]]),
        );
        f.success();
        assert_eq!(f.launches(), 1, "{backend}");
        assert_eq!(f.acknowledgements(), 1, "{backend}");
        assert!(
            !injected.exists(),
            "task text was interpreted as a shell command"
        );
        let bytes = fs::read(f.root.join("agent-argv")).unwrap();
        let argv: Vec<_> = bytes
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| std::str::from_utf8(value).unwrap())
            .collect();
        assert_eq!(
            &argv[..3],
            ["--model", "a model with spaces", "literal ' \" $HOME ;"]
        );
        assert!(argv.last().unwrap().contains(&task));
        let cwd = fs::read_to_string(f.root.join("agent-cwd")).unwrap();
        assert_eq!(
            fs::canonicalize(cwd.trim()).unwrap(),
            fs::canonicalize(f.root.join("checkout")).unwrap()
        );
        assert!(
            fs::read_to_string(f.root.join("multiplexer-args"))
                .unwrap()
                .contains("Review alice/project#42")
        );
        let state = f.state();
        let record = &state["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"];
        assert_eq!(record["pane"], target);
        assert_eq!(record["multiplexer"], backend);
        assert_eq!(record["tab_name"], "Review alice/project#42");
        assert!(
            fs::read_to_string(f.root.join("ack-1.txt"))
                .unwrap()
                .contains(&format!("Working through {backend} in test-{backend}"))
        );
    }
}

#[test]
fn unavailable_alternative_sessions_are_retried_without_claiming_work() {
    for backend in ["tmux", "zellij"] {
        let f = Fixture::new();
        f.config(json!({"multiplexer":backend,"agent":MOCK_TOOLS.get().unwrap().join("agent")}));
        f.write(
            "issues.json",
            &json!([[Fixture::comment(1, "alice", "@invo")]]),
        );
        fs::write(f.root.join("server-down"), "").unwrap();
        assert!(!f.run().status.success());
        assert_eq!(f.launches(), 0);
        assert!(!fs::read_dir(&f.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("reaction-")
        }));
        fs::remove_file(f.root.join("server-down")).unwrap();
        f.success();
        assert_eq!(f.launches(), 1);
        assert_eq!(f.acknowledgements(), 1);
    }
}

#[test]
fn changing_multiplexer_does_not_repeat_a_claimed_comment() {
    let f = Fixture::new();
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    f.success();
    for backend in ["tmux", "zellij"] {
        f.config(json!({"multiplexer":backend,"agent":MOCK_TOOLS.get().unwrap().join("agent")}));
        f.success();
        assert!(
            f.command(&format!("machine-{backend}"))
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    assert_eq!(f.launches(), 1);
    assert_eq!(f.acknowledgements(), 1);
}

#[test]
fn alternative_launcher_failures_keep_the_shared_claim() {
    for backend in ["tmux", "zellij"] {
        let f = Fixture::new();
        f.config(json!({"multiplexer":backend,"agent":MOCK_TOOLS.get().unwrap().join("agent")}));
        f.write(
            "issues.json",
            &json!([[Fixture::comment(1, "alice", "@invo")]]),
        );
        fs::write(f.root.join("multiplexer-fails"), "").unwrap();
        f.success();
        fs::remove_file(f.root.join("multiplexer-fails")).unwrap();
        assert!(
            f.command("another-machine")
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(f.launches(), 0);
        assert_eq!(f.acknowledgements(), 0);
        assert_eq!(
            f.state()["repositories"]["github.com/alice/alice/project"]["launches"]["i-1"]["status"],
            "needs-attention"
        );
    }
}

#[test]
fn invalid_backend_or_names_cannot_start_work() {
    let f = Fixture::new();
    for config in [
        json!({"multiplexer":"unknown"}),
        json!({"workspace_name":""}),
        json!({"tab_name":"two\nlines"}),
    ] {
        f.config(config);
        assert!(!f.run().status.success());
    }
    f.config(json!({"workspace_name":"x".repeat(257)}));
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    assert!(!f.run().status.success());
    assert_eq!(f.launches(), 0);
    assert!(!fs::read_dir(&f.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("reaction-")
    }));
}

#[test]
fn global_mode_uses_local_checkouts_for_reuse_without_polling_idle_repositories() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("repos/project/.git")).unwrap();
    f.config(
        json!({"repositories":{}, "discover_github_repositories":true, "discover_repositories":true,
        "repos_directory":f.root.join("repos")}),
    );
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    f.success();
    assert_eq!(
        f.launches(),
        0,
        "an idle local checkout should not be polled in global mode"
    );
    f.write(
        "search.json",
        &json!([{"total_count":1,"incomplete_results":false,"items":[
        {"repository_url":"https://api.github.com/repos/alice/project"}]}]),
    );
    f.success();
    assert_eq!(f.launches(), 1);
    assert!(
        !f.root.join("clones").exists(),
        "the matching checkout should be reused"
    );
    f.write(
        "search.json",
        &json!([{"total_count":0,"incomplete_results":false,"items":[]}]),
    );
    f.write(
        "issues.json",
        &json!([[Fixture::comment(2, "alice", "@invo")]]),
    );
    f.success();
    assert_eq!(
        f.launches(),
        1,
        "a successfully drained repository should leave the pending queue"
    );
}

#[test]
fn explicit_repository_mappings_are_polled_even_without_global_search_results() {
    let f = Fixture::new();
    f.config(json!({"discover_github_repositories":true}));
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@invo")]]),
    );
    f.success();
    assert_eq!(f.launches(), 1);
}

#[test]
fn toml_comments_literal_strings_tables_and_multiline_acknowledgements_work() {
    let f = Fixture::new();
    let path = f.root.join("config.toml");
    let base = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!(
            r#"# Custom invocation settings; the repository table is below.
bot_name = 'helper' # TOML comments are supported
agent_args = ['--model', 'example']
acknowledgement_message = '''
Working on {{repository}}#{{issue_number}}.
Agent: {{agent}}
'''
{base}"#
        ),
    )
    .unwrap();
    f.write(
        "issues.json",
        &json!([[Fixture::comment(1, "alice", "@helper Fix this")]]),
    );
    f.success();
    assert_eq!(f.launches(), 1);
    assert_eq!(f.acknowledgements(), 1);
    assert!(
        fs::read_to_string(f.root.join("ack-1.txt"))
            .unwrap()
            .contains("Working on alice/project#42.\nAgent: codex\n")
    );
    assert!(
        fs::read_to_string(f.root.join("start-args"))
            .unwrap()
            .contains("--model\nexample")
    );
}

#[test]
fn toml_parse_type_and_unknown_setting_errors_report_the_config_path() {
    let f = Fixture::new();
    let path = f.root.join("config.toml");
    for text in [
        "bot_name = \"",
        "poll_interval_seconds = 'sixty'",
        "poll_intervl_seconds = 60",
    ] {
        fs::write(&path, text).unwrap();
        let output = f.command("state").arg("--check-config").output().unwrap();
        assert!(!output.status.success(), "invalid config accepted: {text}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("invalid TOML config"), "{error}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
    assert_eq!(f.launches(), 0);
    assert_eq!(f.acknowledgements(), 0);
}

#[test]
fn empty_toml_and_readme_examples_validate_without_external_commands() {
    let f = Fixture::new();
    let readme = include_str!("../README.md");
    let examples = std::iter::once("").chain(
        readme
            .split("```toml\n")
            .skip(1)
            .map(|section| section.split("```").next().unwrap()),
    );
    for text in examples {
        fs::write(f.root.join("config.toml"), text).unwrap();
        let output = f
            .command("state")
            .arg("--check-config")
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "example: {text}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(f.launches(), 0);
    assert_eq!(f.acknowledgements(), 0);
}

#[test]
fn hostname_bot_name_toggle_routes_mentions_and_reports_the_effective_trigger() {
    let hostname = hostname::get().unwrap().into_string().unwrap();
    let short = hostname.split('.').next().unwrap();
    let configured = if short.eq_ignore_ascii_case("helper") {
        "another-helper"
    } else {
        "helper"
    };
    for enabled in [false, true] {
        let f = Fixture::new();
        f.config(json!({"bot_name":configured, "use_hostname_as_bot_name":enabled}));
        let (active, inactive) = if enabled {
            (short, configured)
        } else {
            (configured, short)
        };
        f.write(
            "issues.json",
            &json!([[
                Fixture::comment(
                    1,
                    "alice",
                    &format!("@{inactive} Work on the other machine")
                ),
                Fixture::comment(2, "alice", &format!("> @{active} Quoted task")),
                Fixture::comment(3, "alice", &format!("@{active} Fix the actual issue"))
            ]]),
        );
        let output = f
            .command("state")
            .env("HOSTNAME", "unrelated-environment-hostname")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(f.launches(), 1);
        assert_eq!(f.acknowledgements(), 1);
        let state = f.state();
        let launches = state["repositories"]["github.com/alice/alice/project"]["launches"]
            .as_object()
            .unwrap();
        assert!(launches.contains_key("i-3"));
        assert_eq!(launches.len(), 1);
        let check = f
            .command("unused-state")
            .arg("--check-config")
            .env("HOSTNAME", "unrelated-environment-hostname")
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(check.status.success());
        assert!(String::from_utf8_lossy(&check.stdout).contains(&format!("trigger: @{active}")));
        assert!(!f.root.join("unused-state").exists());
    }
}

#[test]
fn github_trigger_skips_missing_cli_or_login_then_recovers_without_restarting() {
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for missing_cli in [true, false] {
        let f = Fixture::new();
        let gh = if missing_cli {
            f.root.join("gh-installed-later")
        } else {
            fs::write(f.root.join("gh-logged-out"), "").unwrap();
            MOCK_TOOLS.get().unwrap().join("gh")
        };
        f.config(json!({"gh_path":gh, "poll_interval_seconds":1}));
        f.write(
            "issues.json",
            &json!([[Fixture::comment(1, "alice", "@invo Fix this after login")]]),
        );
        let log = f.root.join("daemon.log");
        let mut daemon = Daemon(
            Command::new(env!("CARGO_BIN_EXE_invocateur"))
                .arg("--config")
                .arg(f.root.join("config.toml"))
                .arg("--state-dir")
                .arg(f.root.join("state"))
                .env("FIXTURE", &f.root)
                .stdout(std::process::Stdio::null())
                .stderr(fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let expected = if missing_cli {
            "not installed or not in PATH"
        } else {
            "cannot verify GitHub authentication"
        };
        let error_log = loop {
            let text = fs::read_to_string(&log).unwrap();
            if text.contains(expected) {
                break text;
            }
            assert!(std::time::Instant::now() < deadline, "{text}");
            assert!(daemon.0.try_wait().unwrap().is_none(), "{text}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(error_log.contains("trigger github: skipping poll"));
        assert_eq!(f.launches(), 0);
        assert_eq!(f.acknowledgements(), 0);
        assert!(!f.root.join("state/state.json").exists());
        assert!(daemon.0.try_wait().unwrap().is_none());
        if missing_cli {
            std::os::unix::fs::symlink(MOCK_TOOLS.get().unwrap().join("gh"), &gh).unwrap();
        } else {
            fs::remove_file(f.root.join("gh-logged-out")).unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while f.acknowledgements() != 1 {
            assert!(std::time::Instant::now() < deadline);
            assert!(daemon.0.try_wait().unwrap().is_none());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(f.launches(), 1);
        assert!(daemon.0.try_wait().unwrap().is_none());
    }
}

fn scheduled_command(f: &Fixture, name: &str) -> Value {
    json!({"name":name, "every_seconds":3600, "directory":f.root.join("checkout"),
        "run_on_startup":true, "command":[MOCK_TOOLS.get().unwrap().join("scheduled-command")]})
}
fn schedule_state(f: &Fixture) -> Value {
    serde_json::from_slice(&fs::read(f.root.join("state/schedules.json")).unwrap()).unwrap()
}
fn scheduled_runs(f: &Fixture) -> usize {
    fs::read_to_string(f.root.join("scheduled-runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn scheduled_commands_use_literal_argv_and_directory_without_github_or_a_multiplexer() {
    let f = Fixture::new();
    let directory = f.root.join("plain-directory");
    fs::create_dir(&directory).unwrap();
    let injected = f.root.join("must-not-exist");
    let literal = format!(
        "$(touch {}) `touch {}` ' \" $HOME ;",
        injected.display(),
        injected.display()
    );
    let mut task = scheduled_command(&f, "command-task");
    task["directory"] = json!(directory);
    task["command"]
        .as_array_mut()
        .unwrap()
        .extend([json!("--message"), json!(literal)]);
    f.config(json!({"github_enabled":false, "gh_path":"/missing/gh", "herdr_path":"/missing/herdr", "scheduled_tasks":[task]}));
    f.success();
    f.success();
    assert_eq!(
        scheduled_runs(&f),
        1,
        "restart replayed a dispatched occurrence"
    );
    assert!(!injected.exists());
    assert!(!f.root.join("user-calls").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("scheduled-cwd"))
            .unwrap()
            .trim(),
        directory.to_str().unwrap()
    );
    let bytes = fs::read(f.root.join("scheduled-argv")).unwrap();
    let argv: Vec<_> = bytes
        .split(|b| *b == 0)
        .filter(|item| !item.is_empty())
        .map(|item| std::str::from_utf8(item).unwrap())
        .collect();
    assert_eq!(argv, ["--message", literal.as_str()]);
    assert_eq!(
        schedule_state(&f)["jobs"]["command-task"]["last_run"]["status"],
        "succeeded"
    );
}

#[test]
fn scheduled_agent_tasks_use_the_configured_agent_and_names_without_github() {
    let f = Fixture::new();
    f.config(
        json!({"github_enabled":false, "agent":"claude", "scheduled_tasks":[{
            "name":"review", "every_seconds":3600, "run_on_startup":true,
            "directory":f.root.join("checkout"), "prompt":"Review the changes; keep $HOME literal.",
            "workspace_name":"Scheduled {task_name}", "tab_name":"job-{task_name}"
        }]}),
    );
    f.success();
    f.success();
    assert_eq!(f.launches(), 1);
    assert_eq!(f.acknowledgements(), 0);
    assert!(!f.root.join("user-calls").exists());
    assert!(
        fs::read_to_string(f.root.join("start-args"))
            .unwrap()
            .contains("claude\n")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("tab-names")).unwrap(),
        "job-review\n"
    );
    assert!(
        fs::read_to_string(f.root.join("workspace-args"))
            .unwrap()
            .contains("Scheduled review")
    );
    assert!(
        fs::read_to_string(f.root.join("prompts"))
            .unwrap()
            .contains("Review the changes; keep $HOME literal.")
    );
    let state = schedule_state(&f);
    assert_eq!(state["jobs"]["review"]["last_run"]["status"], "started");
    assert_eq!(state["jobs"]["review"]["last_run"]["pane"], "w7:p9");
}

#[test]
fn scheduled_work_continues_when_github_is_unavailable_and_other_jobs_fail() {
    let f = Fixture::new();
    let mut failed = scheduled_command(&f, "fails");
    failed["command"]
        .as_array_mut()
        .unwrap()
        .push(json!("fail"));
    f.config(json!({"gh_path":"/missing/gh", "scheduled_tasks":[failed, scheduled_command(&f, "healthy")]}));
    let output = f.run();
    assert!(!output.status.success());
    assert_eq!(scheduled_runs(&f), 2);
    assert!(String::from_utf8_lossy(&output.stderr).contains("trigger github: skipping poll"));
    let state = schedule_state(&f);
    assert_eq!(state["jobs"]["fails"]["last_run"]["status"], "failed");
    assert_eq!(state["jobs"]["healthy"]["last_run"]["status"], "succeeded");
    assert!(!f.run().status.success());
    assert_eq!(
        scheduled_runs(&f),
        2,
        "one-shot retry replayed completed task occurrences"
    );
}

#[test]
fn new_schedules_wait_for_their_first_due_time_and_disabled_tasks_do_not_run() {
    let f = Fixture::new();
    let mut future = scheduled_command(&f, "future");
    future["run_on_startup"] = json!(false);
    let mut disabled = scheduled_command(&f, "disabled");
    disabled["enabled"] = json!(false);
    f.config(json!({"github_enabled":false, "scheduled_tasks":[future, disabled]}));
    f.success();
    f.success();
    assert_eq!(scheduled_runs(&f), 0);
    let state = schedule_state(&f);
    assert!(state["jobs"]["future"]["next_run"].as_str().is_some());
    assert!(state["jobs"].get("disabled").is_none());
}

#[test]
fn invalid_schedules_fail_configuration_checks_without_starting_tasks() {
    let f = Fixture::new();
    for task in [
        json!({"name":"missing-time", "command":["true"]}),
        json!({"name":"both-times", "cron":"0 9 * * *", "every_seconds":60, "command":["true"]}),
        json!({"name":"bad-cron", "cron":"not a cron", "command":["true"]}),
        json!({"name":"bad-zone", "cron":"0 9 * * *", "timezone":"Wrong/Zone", "command":["true"]}),
        json!({"name":"zero", "every_seconds":0, "command":["true"]}),
        json!({"name":"missing-action", "every_seconds":60}),
        json!({"name":"both-actions", "every_seconds":60, "prompt":"Review", "command":["true"]}),
    ] {
        f.config(json!({"github_enabled":false, "scheduled_tasks":[task]}));
        assert!(
            !f.command("state")
                .arg("--check-config")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let task = scheduled_command(&f, "duplicate");
    f.config(json!({"scheduled_tasks":[task.clone(), task]}));
    assert!(
        !f.command("state")
            .arg("--check-config")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(scheduled_runs(&f), 0);
    assert_eq!(f.launches(), 0);
}

struct ScheduleDaemon(std::process::Child);
impl Drop for ScheduleDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn schedule_daemon(f: &Fixture) -> ScheduleDaemon {
    ScheduleDaemon(
        Command::new(env!("CARGO_BIN_EXE_invocateur"))
            .arg("--config")
            .arg(f.root.join("config.toml"))
            .arg("--state-dir")
            .arg(f.root.join("state"))
            .env("FIXTURE", &f.root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn interval_schedules_repeat_without_increasing_github_poll_frequency() {
    let f = Fixture::new();
    let mut task = scheduled_command(&f, "repeat");
    task["every_seconds"] = json!(1);
    f.config(json!({"poll_interval_seconds":60, "scheduled_tasks":[task]}));
    let mut daemon = schedule_daemon(&f);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if scheduled_runs(&f) >= 2
            && let Ok(bytes) = fs::read(f.root.join("state/schedules.json"))
            && let Ok(state) = serde_json::from_slice::<Value>(&bytes)
            && state["jobs"]["repeat"]["last_run"]["status"] == "succeeded"
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        assert!(daemon.0.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(f.root.join("user-calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn interrupted_scheduled_dispatch_is_not_replayed_after_restart() {
    let f = Fixture::new();
    let mut task = scheduled_command(&f, "interrupted");
    task["command"].as_array_mut().unwrap().push(json!("wait"));
    f.config(json!({"github_enabled":false, "scheduled_tasks":[task.clone()]}));
    let mut daemon = schedule_daemon(&f);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid: i32 = loop {
        if let Ok(text) = fs::read_to_string(f.root.join("scheduled-pid"))
            && let Ok(pid) = text.trim().parse()
        {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline);
        assert!(daemon.0.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    // The PID was written by this test's mock command in its private process group.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    f.config(
        json!({"github_enabled":false, "scheduled_tasks":[task, scheduled_command(&f, "other")]}),
    );
    let output = f.run();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("previous dispatch needs attention"));
    assert_eq!(scheduled_runs(&f), 2, "interrupted command was replayed");
    assert_eq!(
        schedule_state(&f)["jobs"]["other"]["last_run"]["status"],
        "succeeded"
    );
}

#[test]
fn missed_schedule_occurrences_coalesce_instead_of_replaying_a_burst() {
    let f = Fixture::new();
    let mut task = scheduled_command(&f, "coalesce");
    task["every_seconds"] = json!(1);
    f.config(json!({"github_enabled":false, "scheduled_tasks":[task]}));
    f.success();
    std::thread::sleep(std::time::Duration::from_millis(2200));
    f.success();
    f.success();
    assert_eq!(scheduled_runs(&f), 2);
}

#[test]
fn timed_out_scheduled_commands_do_not_prevent_other_due_tasks() {
    let f = Fixture::new();
    let mut task = scheduled_command(&f, "timeout");
    task["command"].as_array_mut().unwrap().push(json!("wait"));
    task["timeout_seconds"] = json!(1);
    f.config(
        json!({"github_enabled":false, "scheduled_tasks":[task, scheduled_command(&f, "healthy")]}),
    );
    let start = std::time::Instant::now();
    let output = f.run();
    assert!(!output.status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out after 1s"));
    assert_eq!(
        schedule_state(&f)["jobs"]["healthy"]["last_run"]["status"],
        "succeeded"
    );
    assert_eq!(scheduled_runs(&f), 2);
}
