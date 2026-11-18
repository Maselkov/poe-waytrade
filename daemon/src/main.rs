//! Price lookups for the poe-overlay Vulkan layer.
//!
//! Listens on the abstract Unix socket `@poe-overlay`. Abstract sockets live
//! in the network namespace rather than the filesystem, so the layer can
//! reach us from inside Steam's runtime container. One request per
//! connection; both directions are framed as a little-endian u32 length
//! followed by UTF-8:
//!
//!   request:  "PRICE\n" + item text as copied from the game
//!   response: tab-separated lines
//!     status   ok | not_found | unsupported | loading | error | bad_request
//!     message  <text>                                  (optional)
//!     title    <item name>                             (status ok)
//!     price    <label> <chaos> <divine> <listings|"">  (status ok, 1+)
//!     league   <name>
//!     age      <minutes since prices were fetched>
//!
//!   request:  "PING\n"   (sent by the layer at game start to wake us up)
//!   response: status ready | loading
//!
//! Normally started by systemd socket activation (poe-overlay-daemon.socket):
//! systemd owns the socket, starts us on the first connection, and we exit
//! after a while without requests. Run by hand, we bind the socket ourselves
//! and stay up.

mod item;
mod lookup;
mod ninja;

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const SOCKET_NAME: &[u8] = b"poe-overlay";
const MAX_FRAME: usize = 256 * 1024;
const REFRESH: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(60);
/// When socket-activated, exit after this long without requests.
const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);

struct State {
    db: RwLock<Option<Arc<ninja::PriceDb>>>,
    last_error: Mutex<Option<String>>,
    last_request: Mutex<Instant>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("query") {
        std::process::exit(match query(args.get(2)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("query failed: {e}");
                1
            }
        });
    }

    let state = Arc::new(State {
        db: RwLock::new(None),
        last_error: Mutex::new(None),
        last_request: Mutex::new(Instant::now()),
    });

    let listener = match systemd_listener() {
        Some(l) => {
            let idle = std::env::var("POE_OVERLAY_IDLE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_IDLE);
            let watched = state.clone();
            thread::spawn(move || exit_when_idle(&watched, idle));
            eprintln!("socket-activated; exiting after {}s idle", idle.as_secs());
            l
        }
        None => {
            let addr = SocketAddr::from_abstract_name(SOCKET_NAME).expect("socket name");
            match UnixListener::bind_addr(&addr) {
                Ok(l) => {
                    eprintln!("listening on @poe-overlay");
                    l
                }
                Err(e) => {
                    eprintln!("can't listen on @poe-overlay ({e}); is another daemon running?");
                    std::process::exit(1);
                }
            }
        }
    };

    let refresher = state.clone();
    thread::spawn(move || refresh_loop(&refresher));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        thread::spawn(move || {
            if let Err(e) = handle(&state, stream) {
                eprintln!("client: {e}");
            }
        });
    }
}

/// The listening socket systemd passed us (sd_listen_fds convention: fd 3),
/// if we were socket-activated.
fn systemd_listener() -> Option<UnixListener> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || fds < 1 {
        return None;
    }
    // SAFETY: single-threaded at this point; children shouldn't inherit these.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    // SAFETY: systemd hands us fd 3 and nothing else owns it.
    Some(unsafe { UnixListener::from_raw_fd(3) })
}

/// Exiting is safe: systemd keeps the socket open, queues new connections,
/// and starts us again for the next one.
fn exit_when_idle(state: &State, idle: Duration) {
    loop {
        thread::sleep(Duration::from_secs(10).min(idle));
        if state.last_request.lock().unwrap().elapsed() >= idle {
            eprintln!("no requests for {}s, exiting", idle.as_secs());
            std::process::exit(0);
        }
    }
}

fn refresh_loop(state: &State) {
    loop {
        let result = match std::env::var("POE_LEAGUE") {
            Ok(league) => Ok(league),
            Err(_) => ninja::current_league(),
        }
        .and_then(|league| ninja::fetch(&league));

        match result {
            Ok(db) => {
                eprintln!(
                    "loaded {} exchange prices and {} item names for {}",
                    db.exchange.len(),
                    db.items.len(),
                    db.league
                );
                *state.db.write().unwrap() = Some(Arc::new(db));
                *state.last_error.lock().unwrap() = None;
                thread::sleep(REFRESH);
            }
            Err(e) => {
                eprintln!("price refresh failed: {e}");
                *state.last_error.lock().unwrap() = Some(e);
                thread::sleep(RETRY);
            }
        }
    }
}

fn handle(state: &State, mut stream: UnixStream) -> io::Result<()> {
    // Abstract sockets have no file permissions; only serve our own user.
    if peer_uid(&stream) != Some(unsafe { libc::getuid() }) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "peer is another user"));
    }
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request = read_frame(&mut stream)?;
    *state.last_request.lock().unwrap() = Instant::now();
    let response = if let Some(text) = request.strip_prefix("PRICE\n") {
        respond(state, text)
    } else if request == "PING\n" {
        let ready = state.db.read().unwrap().is_some();
        format!("status\t{}\n", if ready { "ready" } else { "loading" })
    } else {
        "status\tbad_request\nmessage\tUnknown request\n".to_string()
    };
    write_frame(&mut stream, &response)
}

fn respond(state: &State, text: &str) -> String {
    let mut out = String::new();
    let Some(item) = item::parse(text) else {
        return "status\tbad_request\nmessage\tNot PoE item text\n".to_string();
    };
    let Some(db) = state.db.read().unwrap().clone() else {
        let message = match &*state.last_error.lock().unwrap() {
            Some(e) => format!("Couldn't load prices: {e}"),
            None => "Loading prices from poe.ninja...".to_string(),
        };
        let status = if message.starts_with("Couldn't") { "error" } else { "loading" };
        return format!("status\t{status}\nmessage\t{}\n", clean(&message));
    };

    match lookup::lookup(&db, &item) {
        lookup::Outcome::Found { title, prices } => {
            let _ = writeln!(out, "status\tok\ntitle\t{}", clean(&title));
            for p in prices {
                let listings = p.listings.map(|n| n.to_string()).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "price\t{}\t{:.2}\t{:.4}\t{listings}",
                    clean(&p.label),
                    p.chaos,
                    p.divine
                );
            }
        }
        lookup::Outcome::NotFound(m) => {
            let _ = writeln!(out, "status\tnot_found\nmessage\t{}", clean(&m));
        }
        lookup::Outcome::Unsupported(m) => {
            let _ = writeln!(out, "status\tunsupported\nmessage\t{}", clean(&m));
        }
    }
    let _ = writeln!(out, "league\t{}", clean(&db.league));
    let _ = writeln!(out, "age\t{}", db.fetched_at.elapsed().as_secs() / 60);
    out
}

/// Fields can't contain the protocol's separators.
fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn read_frame(stream: &mut UnixStream) -> io::Result<String> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

fn write_frame(stream: &mut UnixStream, body: &str) -> io::Result<()> {
    stream.write_all(&(body.len() as u32).to_le_bytes())?;
    stream.write_all(body.as_bytes())
}

/// `poe-overlay-daemon query [file]`: send item text (file or stdin) to the
/// running daemon and print its answer, for testing without the game.
fn query(path: Option<&String>) -> io::Result<()> {
    let text = match path {
        Some(p) => std::fs::read_to_string(p)?,
        None => io::read_to_string(io::stdin())?,
    };
    let addr = SocketAddr::from_abstract_name(SOCKET_NAME)?;
    let mut stream = UnixStream::connect_addr(&addr)?;
    write_frame(&mut stream, &format!("PRICE\n{text}"))?;
    print!("{}", read_frame(&mut stream)?);
    Ok(())
}
