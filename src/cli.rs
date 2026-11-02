//! Agent-facing CLI. All schedule operations use the same HTTP API.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{env, fs, io::Read, path::PathBuf, time::Duration};

const HELP: &str = "invo schedule <create|list|get|update|replace|delete|enable|disable|resume> [NAME] [OPTIONS]\n\nCreate/update options:\n  --cron EXPR | --every-seconds N\n  --timezone IANA_ZONE  --directory PATH\n  --prompt TEXT | -- COMMAND [ARG ...]\n  --run-immediately  --enable  --disable\n  --timeout-seconds N  --workspace-name TEMPLATE  --tab-name TEMPLATE\n  --file FILE.json    Read a JSON task or patch; use - for stdin\n\nConnection options (before a command delimiter):\n  --state-dir PATH    Locate api.json and api-token (defaults to XDG state)\n  --url http://127.0.0.1:PORT  --token-file PATH\n\nAll output is JSON. Updates use If-Match to avoid concurrent lost edits.\n";

pub fn state_dir(value: Option<PathBuf>) -> Result<PathBuf> {
    crate::expand_home(&value.unwrap_or_else(|| {
        env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~/.local/state"))
            .join("invocateur")
    }))
}

pub fn run(arguments: Vec<String>, inherited_state: Option<PathBuf>) -> Result<()> {
    if arguments.is_empty()
        || arguments
            .first()
            .is_some_and(|value| matches!(value.as_str(), "--help" | "-h"))
    {
        print!("{HELP}");
        return Ok(());
    }
    let mut args = arguments.into_iter();
    let verb = args.next().unwrap();
    let mut name = None;
    let mut fields = serde_json::Map::new();
    let mut state = inherited_state;
    let mut url = env::var("INVOCATEUR_API_URL").ok();
    let mut token_file = None;
    while let Some(arg) = args.next() {
        let value = |args: &mut std::vec::IntoIter<String>| {
            args.next().with_context(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--state-dir" => state = Some(PathBuf::from(value(&mut args)?)),
            "--url" => url = Some(value(&mut args)?),
            "--token-file" => token_file = Some(PathBuf::from(value(&mut args)?)),
            "--file" => {
                let path = value(&mut args)?;
                let bytes = if path == "-" {
                    let mut bytes = vec![];
                    std::io::stdin()
                        .take(crate::api::MAX_BODY + 1)
                        .read_to_end(&mut bytes)?;
                    bytes
                } else {
                    fs::read(path)?
                };
                ensure!(
                    bytes.len() as u64 <= crate::api::MAX_BODY,
                    "task input exceeds 1 MiB"
                );
                let input: Value =
                    serde_json::from_slice(&bytes).context("task input must be JSON")?;
                fields.extend(
                    input
                        .as_object()
                        .context("task input must be an object")?
                        .clone(),
                );
            }
            "--cron" => {
                fields.insert("cron".into(), json!(value(&mut args)?));
                fields.insert("every_seconds".into(), Value::Null);
            }
            "--every-seconds" => {
                fields.insert(
                    "every_seconds".into(),
                    json!(
                        value(&mut args)?
                            .parse::<u64>()
                            .context("interval must be an integer")?
                    ),
                );
                fields.insert("cron".into(), Value::Null);
            }
            "--timezone" | "--directory" | "--timeout-seconds" | "--workspace-name"
            | "--tab-name" => {
                let field = arg.trim_start_matches("--").replace('-', "_");
                let text = value(&mut args)?;
                fields.insert(
                    field,
                    if arg == "--timeout-seconds" {
                        json!(text.parse::<u64>().context("timeout must be an integer")?)
                    } else {
                        json!(text)
                    },
                );
            }
            "--prompt" => {
                fields.insert("prompt".into(), json!(value(&mut args)?));
                fields.insert("command".into(), json!([]));
            }
            "--run-immediately" => {
                fields.insert("run_immediately".into(), json!(true));
            }
            "--enable" => {
                fields.insert("enabled".into(), json!(true));
            }
            "--disable" => {
                fields.insert("enabled".into(), json!(false));
            }
            "--" => {
                let command: Vec<_> = args.collect();
                ensure!(
                    !command.is_empty(),
                    "-- needs a command and optional arguments"
                );
                fields.insert("command".into(), json!(command));
                fields.insert("prompt".into(), Value::Null);
                break;
            }
            _ if !arg.starts_with('-') && name.is_none() => name = Some(arg),
            _ => bail!("unknown schedule argument {arg}; use invo schedule --help"),
        }
    }
    ensure!(
        matches!(
            verb.as_str(),
            "create"
                | "list"
                | "get"
                | "update"
                | "replace"
                | "delete"
                | "enable"
                | "disable"
                | "resume"
        ),
        "unknown schedule command {verb}"
    );
    let state = state_dir(state)?;
    let url = match url {
        Some(url) => url,
        None => {
            let endpoint: Value = serde_json::from_slice(
                &fs::read(state.join("api.json"))
                    .context("schedule API not running; start invo serve first or pass --url")?,
            )?;
            endpoint["url"]
                .as_str()
                .context("invalid API endpoint metadata")?
                .into()
        }
    };
    let authority = url
        .strip_prefix("http://")
        .context("the local API URL must start with http://")?
        .trim_end_matches('/');
    let address: std::net::SocketAddr = authority
        .parse()
        .context("API URL must contain a literal loopback IP address and port")?;
    ensure!(
        address.ip().is_loopback(),
        "the local API client only sends credentials to loopback addresses"
    );
    let token = match env::var("INVOCATEUR_API_TOKEN") {
        Ok(token) => token,
        Err(_) => fs::read_to_string(crate::expand_home(
            &token_file.unwrap_or_else(|| state.join("api-token")),
        )?)
        .context("cannot read API token; start the service or set --token-file")?,
    };
    let client = Client {
        url: format!("http://{address}"),
        token: token.trim().into(),
        agent: ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .build()
            .into(),
    };
    if verb == "list" {
        ensure!(
            name.is_none() && fields.is_empty(),
            "list does not accept a task or fields"
        );
        return print_json(&client.request("GET", "/v1/schedules", None, None)?);
    }
    if verb == "create" {
        if let Some(name) = name {
            ensure!(
                fields
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|existing| existing == name),
                "task file name does not match NAME"
            );
            fields.insert("name".into(), json!(name));
        }
        return print_json(&client.request(
            "POST",
            "/v1/schedules",
            Some(Value::Object(fields)),
            None,
        )?);
    }
    let name = name.context("this schedule command needs NAME")?;
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid schedule name"
    );
    let path = format!("/v1/schedules/{name}");
    if verb == "get" {
        ensure!(fields.is_empty(), "get does not accept task fields");
        return print_json(&client.request("GET", &path, None, None)?);
    }
    let current = client.request("GET", &path, None, None)?;
    let revision = current["revision"]
        .as_u64()
        .context("API response has no revision")?;
    match verb.as_str() {
        "delete" => {
            ensure!(fields.is_empty(), "delete does not accept task fields");
            client.request("DELETE", &path, None, Some(revision))?;
            print_json(&json!({"deleted":name}))
        }
        "resume" => {
            ensure!(fields.is_empty(), "resume does not accept task fields");
            print_json(&client.request("POST", &format!("{path}/resume"), None, Some(revision))?)
        }
        "enable" | "disable" => {
            ensure!(
                fields.is_empty(),
                "enable/disable do not accept task fields"
            );
            print_json(&client.request(
                "PATCH",
                &path,
                Some(json!({"enabled":verb == "enable"})),
                Some(revision),
            )?)
        }
        "update" | "replace" => {
            ensure!(!fields.is_empty(), "update needs fields or --file");
            print_json(&client.request(
                if verb == "replace" { "PUT" } else { "PATCH" },
                &path,
                Some(Value::Object(fields)),
                Some(revision),
            )?)
        }
        _ => unreachable!(),
    }
}
fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
struct Client {
    url: String,
    token: String,
    agent: ureq::Agent,
}
impl Client {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        revision: Option<u64>,
    ) -> Result<Value> {
        let url = format!("{}{path}", self.url);
        let authorization = format!("Bearer {}", self.token);
        let revision = revision
            .map(|revision| format!("\"{revision}\""))
            .unwrap_or_else(|| "*".into());
        let mut response = match method {
            "GET" => self
                .agent
                .get(&url)
                .header("Authorization", &authorization)
                .call(),
            "DELETE" => self
                .agent
                .delete(&url)
                .header("Authorization", &authorization)
                .header("If-Match", &revision)
                .call(),
            "POST" => self
                .agent
                .post(&url)
                .header("Authorization", &authorization)
                .header("If-Match", &revision)
                .send_json(body.unwrap_or(json!({}))),
            "PUT" => self
                .agent
                .put(&url)
                .header("Authorization", &authorization)
                .header("If-Match", &revision)
                .send_json(body.unwrap_or(json!({}))),
            "PATCH" => self
                .agent
                .patch(&url)
                .header("Authorization", &authorization)
                .header("If-Match", &revision)
                .send_json(body.unwrap_or(json!({}))),
            _ => unreachable!(),
        }
        .context("cannot reach Invocateur schedule API; check that the service is running")?;
        let status = response.status().as_u16();
        let value = if status == 204 {
            Value::Null
        } else {
            response.body_mut().read_json::<Value>()?
        };
        ensure!(
            (200..300).contains(&status),
            "schedule API HTTP {status}: {}",
            value["error"].as_str().unwrap_or("request failed")
        );
        Ok(value)
    }
}
