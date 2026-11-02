//! Authenticated local HTTP control plane, independent of task execution.
use crate::triggers::schedule::{ApiError, Store, problem, write_json};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tiny_http::{Header, Request, Response, StatusCode};

pub const MAX_BODY: u64 = 1024 * 1024;

pub struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    endpoint: PathBuf,
}
impl Server {
    pub fn start(listen: &str, state_dir: &Path, store: Arc<Store>) -> Result<Self> {
        let server = tiny_http::Server::http(listen)
            .map_err(|error| anyhow::anyhow!("cannot start schedule API: {error}"))?;
        let address = server
            .server_addr()
            .to_ip()
            .context("schedule API needs a TCP address")?;
        let token_file = state_dir.join("api-token");
        let token = match fs::read_to_string(&token_file) {
            Ok(token) => token.trim().to_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 32];
                getrandom::fill(&mut bytes)
                    .map_err(|error| anyhow::anyhow!("cannot generate API token: {error}"))?;
                let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&token_file)?;
                file.write_all(token.as_bytes())?;
                file.sync_all()?;
                token
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid API token file"
        );
        fs::set_permissions(
            &token_file,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        let url = format!("http://{address}");
        let endpoint = state_dir.join("api.json");
        write_json(&endpoint, &json!({"url":url}))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new()
            .name("schedule-api".into())
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    match server.recv_timeout(Duration::from_millis(100)) {
                        Ok(Some(request)) => serve(request, &store, &token),
                        Ok(None) => {}
                        Err(error) => eprintln!("schedule API receive error: {error}"),
                    }
                }
            })?;
        eprintln!(
            "schedule API listening at {url}; credentials in {}",
            token_file.display()
        );
        Ok(Self {
            stop,
            thread: Some(thread),
            endpoint,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.endpoint);
    }
}
fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}
fn body(request: &mut Request) -> Result<Value> {
    if request
        .body_length()
        .is_some_and(|length| length as u64 > MAX_BODY)
    {
        return Err(problem(413, "request body exceeds 1 MiB"));
    }
    let mut bytes = vec![];
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BODY {
        return Err(problem(413, "request body exceeds 1 MiB"));
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| problem(400, format!("invalid JSON body: {error}")))
}
fn route(request: &mut Request, store: &Store) -> Result<(u16, Value, Option<u64>)> {
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap()
        .trim_end_matches('/')
        .to_owned();
    let method = request.method().as_str().to_owned();
    if path == "/health" && method == "GET" {
        return Ok((200, json!({"status":"ok"}), None));
    }
    if path == "/v1/schedules" {
        return match method.as_str() {
            "GET" => Ok((200, json!({"schedules":store.list()?}), None)),
            "POST" => {
                let job = store.create(body(request)?)?;
                Ok((201, serde_json::to_value(&job)?, Some(job.revision)))
            }
            _ => Err(problem(405, "use GET or POST for /v1/schedules")),
        };
    }
    let rest = path
        .strip_prefix("/v1/schedules/")
        .ok_or_else(|| problem(404, "API route not found"))?;
    let pieces: Vec<_> = rest.split('/').collect();
    let name = pieces[0];
    let revision = match header(request, "If-Match") {
        Some("*") | None => None,
        Some(value) => Some(
            value
                .trim_matches('"')
                .parse::<u64>()
                .map_err(|_| problem(400, "If-Match must be a quoted schedule revision"))?,
        ),
    };
    if pieces.len() == 2 && pieces[1] == "resume" && method == "POST" {
        let job = store.resume(name, revision)?;
        return Ok((200, serde_json::to_value(&job)?, Some(job.revision)));
    }
    if pieces.len() != 1 {
        return Err(problem(404, "API route not found"));
    }
    match method.as_str() {
        "GET" => {
            let job = store.get(name)?;
            Ok((200, serde_json::to_value(&job)?, Some(job.revision)))
        }
        "PUT" | "PATCH" => {
            let job = store.update(name, body(request)?, method == "PUT", revision)?;
            Ok((200, serde_json::to_value(&job)?, Some(job.revision)))
        }
        "DELETE" => {
            store.delete(name, revision)?;
            Ok((204, Value::Null, None))
        }
        _ => Err(problem(
            405,
            "use GET, PUT, PATCH, or DELETE for a schedule",
        )),
    }
}
fn serve(mut request: Request, store: &Store, token: &str) {
    let provided = header(&request, "Authorization")
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token)
        .unwrap_or("");
    let result = if !bool::from(provided.as_bytes().ct_eq(token.as_bytes())) {
        Err(problem(401, "a valid Bearer token is required"))
    } else if header(&request, "Origin").is_some() {
        Err(problem(
            403,
            "browser-origin requests are not supported by the local control API",
        ))
    } else {
        route(&mut request, store)
    };
    let (status, value, revision) = match result {
        Ok(result) => result,
        Err(error) => {
            let (status, message) = if let Some(problem) = error.downcast_ref::<ApiError>() {
                (problem.status, problem.to_string())
            } else {
                eprintln!("schedule API error: {error:#}");
                (500, "schedule store operation failed".into())
            };
            (status, json!({"error":message}), None)
        }
    };
    let output = if status == 204 {
        String::new()
    } else {
        value.to_string()
    };
    let mut response = Response::from_string(output)
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    if let Some(revision) = revision {
        response.add_header(Header::from_bytes("ETag", format!("\"{revision}\"")).unwrap());
    }
    if status == 401 {
        response.add_header(Header::from_bytes("WWW-Authenticate", "Bearer").unwrap());
    }
    if let Err(error) = request.respond(response) {
        eprintln!("schedule API response error: {error}");
    }
}
