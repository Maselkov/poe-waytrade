//! A headless Chromium we send pathofexile.com requests through.
//!
//! pathofexile.com sits behind Cloudflare, which refuses non-browser HTTP
//! clients. Like Awakened PoE Trade (which uses Electron's Chromium), we
//! use a real browser: plain headless Chromium with its own user agent, no
//! disguise. Requests are `fetch()` calls made from a page on the site, so
//! cookies and headers behave exactly as in a browser tab.
//!
//! Chromium is driven over the DevTools pipe (`--remote-debugging-pipe`):
//! it reads NUL-terminated JSON commands on fd 3 and writes replies and
//! events on fd 4.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The page we make requests from. Any same-origin URL works; this one is
/// small JSON.
const ORIGIN_PAGE: &str = "https://www.pathofexile.com/api/trade/data/leagues";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub struct HttpResponse {
    pub status: u16,
    /// Lower-cased names.
    pub headers: HashMap<String, String>,
    pub body: String,
}

pub struct Browser {
    child: Child,
    to_chrome: File,
    from_chrome: File,
    buf: Vec<u8>,
    next_id: u64,
    session: String,
}

/// `POE_WAYTRADE_CHROMIUM`, else the first Chromium/Chrome on PATH.
pub fn find_binary() -> Option<String> {
    if let Ok(path) = std::env::var("POE_WAYTRADE_CHROMIUM") {
        return Some(path);
    }
    let path = std::env::var("PATH").unwrap_or_default();
    ["chromium", "google-chrome-stable", "google-chrome"].iter().find_map(|name| {
        path.split(':')
            .map(|dir| Path::new(dir).join(name))
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    })
}

/// Spawns a command from a thread that lives as long as the daemon.
///
/// Chromium gets `PR_SET_PDEATHSIG` so it never outlives us, but Linux
/// sends that signal when the *thread* that forked it exits, not the
/// process. Requests are each handled on a short-lived thread, so a browser
/// spawned from one would be killed as soon as that request was answered.
fn spawn_lasting(command: Command) -> io::Result<Child> {
    type Job = (Command, mpsc::Sender<io::Result<Child>>);
    static SPAWNER: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    let spawner = SPAWNER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        thread::spawn(move || {
            for (mut command, reply) in rx {
                let _ = reply.send(command.spawn());
            }
        });
        Mutex::new(tx)
    });
    let (reply_tx, reply_rx) = mpsc::channel();
    spawner.lock().unwrap().send((command, reply_tx)).map_err(|_| io::Error::other("spawner thread is gone"))?;
    reply_rx.recv().map_err(|_| io::Error::other("spawner thread is gone"))?
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

impl Browser {
    pub fn launch(binary: &str, profile: &Path) -> Result<Browser, String> {
        let err = |what: &str, e: io::Error| format!("{what}: {e}");
        std::fs::create_dir_all(profile).map_err(|e| err("profile dir", e))?;
        let (cmd_read, cmd_write) = pipe().map_err(|e| err("pipe", e))?;
        let (res_read, res_write) = pipe().map_err(|e| err("pipe", e))?;

        let mut command = Command::new(binary);
        command
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--remote-debugging-pipe",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let (child_in, child_out) = (cmd_read.as_raw_fd(), res_write.as_raw_fd());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                // Move both ends out of the way first so neither dup2
                // clobbers the other, then put them on fds 3 and 4.
                let a = libc::fcntl(child_in, libc::F_DUPFD, 10);
                let b = libc::fcntl(child_out, libc::F_DUPFD, 10);
                if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Don't outlive the daemon.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = spawn_lasting(command).map_err(|e| err(binary, e))?;
        drop(cmd_read);
        drop(res_write);

        let mut browser = Browser {
            child,
            to_chrome: File::from(cmd_write),
            from_chrome: File::from(res_read),
            buf: Vec::new(),
            next_id: 0,
            session: String::new(),
        };
        browser.open_origin_page()?;
        Ok(browser)
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn open_origin_page(&mut self) -> Result<(), String> {
        let target = self.command("Target.createTarget", json!({ "url": ORIGIN_PAGE }), false)?;
        let target_id = target["targetId"].as_str().ok_or("no targetId")?.to_string();
        let attached =
            self.command("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), false)?;
        self.session = attached["sessionId"].as_str().ok_or("no sessionId")?.to_string();

        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let state = self.evaluate("[document.readyState, location.host]")?;
            if state[0] == "complete" && state[1] == "www.pathofexile.com" {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err("pathofexile.com didn't load".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Evaluates a JS expression (awaiting promises) in the page.
    pub fn evaluate(&mut self, expression: &str) -> Result<Value, String> {
        let res = self.command(
            "Runtime.evaluate",
            json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }),
            true,
        )?;
        if let Some(ex) = res.get("exceptionDetails") {
            let text = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("JS exception");
            return Err(text.to_string());
        }
        Ok(res["result"]["value"].clone())
    }

    /// A same-origin `fetch()` from the page. `path` is like "/api/trade/...".
    pub fn fetch(&mut self, method: &str, path: &str, body: Option<&str>) -> Result<HttpResponse, String> {
        let js = format!(
            "(async () => {{
                const r = await fetch({path}, {{
                    method: {method},
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: {body},
                }});
                const headers = {{}};
                r.headers.forEach((v, k) => headers[k] = v);
                return {{ status: r.status, headers, body: await r.text() }};
            }})()",
            path = json!(path),
            method = json!(method),
            body = body.map(|b| json!(b).to_string()).unwrap_or_else(|| "undefined".into()),
        );
        let v = self.evaluate(&js)?;
        Ok(HttpResponse {
            status: v["status"].as_u64().unwrap_or(0) as u16,
            headers: v["headers"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap_or("").to_string())).collect())
                .unwrap_or_default(),
            body: v["body"].as_str().unwrap_or("").to_string(),
        })
    }

    fn command(&mut self, method: &str, params: Value, in_session: bool) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if in_session {
            msg["sessionId"] = json!(self.session);
        }
        let mut bytes = msg.to_string().into_bytes();
        bytes.push(0);
        self.to_chrome.write_all(&bytes).map_err(|e| format!("chromium pipe: {e}"))?;

        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let reply = self.read_message(deadline)?;
            if reply["id"].as_u64() != Some(id) {
                continue; // an event, or a reply to something we gave up on
            }
            if let Some(e) = reply.get("error") {
                return Err(format!("{method}: {}", e["message"].as_str().unwrap_or("error")));
            }
            return Ok(reply["result"].clone());
        }
    }

    fn read_message(&mut self, deadline: Instant) -> Result<Value, String> {
        loop {
            if let Some(end) = self.buf.iter().position(|&b| b == 0) {
                let msg: Vec<u8> = self.buf.drain(..=end).collect();
                return serde_json::from_slice(&msg[..msg.len() - 1])
                    .map_err(|e| format!("chromium sent bad JSON: {e}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("chromium didn't answer in time".into());
            }
            let mut pfd = libc::pollfd { fd: self.from_chrome.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if ready < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("poll: {e}"));
            }
            if ready == 0 {
                continue;
            }
            let mut chunk = [0u8; 64 * 1024];
            match self.from_chrome.read(&mut chunk) {
                Ok(0) => return Err("chromium exited".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("chromium pipe: {e}")),
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
