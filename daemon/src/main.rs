//! Price lookups for the poe-waytrade Vulkan layer.
//!
//! Listens on the abstract Unix socket `@poe-waytrade`. Abstract sockets live
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
//!     price    <label> <chaos> <divine> <listings|""> [<unique>]  (status
//!              ok, 1+; an unidentified unique's candidates name theirs)
//!     league   <name>
//!     age      <minutes since prices were fetched>
//!     art      <width> <height>   (an unidentified unique whose base has
//!              uniques with different art: MATCHART can tell which it is;
//!              its size in cells)
//!
//!   request:  "MATCHART\n" + "<width> <height> <cursor x> <cursor y> <cell>"
//!             + "\n" + base64 of the pixels (RGB, 8 bits a channel, rows
//!             top to bottom) around the cursor when the item was copied
//!             ("cell": an inventory cell's pitch in those pixels) + "\n" +
//!             item text: which unique an unidentified one is, by its art
//!             (art.rs)
//!   response: like PRICE's for that unique (several, if they share their
//!             art), with "matched <name> <score>"; or status no_match
//!
//!   request:  "SCANFIND\n" + "<width> <height> <full width> <full height>"
//!             + "\n" + base64 of a grey frame (8 bits a pixel, rows top to
//!             bottom), scaled down from the game's full size: where item
//!             displays' tooltips are (scan.rs)
//!   response: status ok
//!     plate    <x> <y> <w> <h> <crop x> <crop y> <crop w> <crop h> <kind>
//!              (0+, in full-size pixels: a tooltip's title plate, and the
//!              crop to send to SCANITEM; kind: vault | curio)
//!
//!   request:  "SCANITEM\n" + "<width> <height> <plate x> <plate y> <plate
//!             w> <plate h>" + "\n" + base64 of the crop's pixels (RGB, 8
//!             bits a channel), the plate's box within it: the item in that
//!             tooltip, read and priced (poe.ninja, or a trade search for
//!             what it hasn't: Facetor's Lens, rare trinkets)
//!   response: status   ok | not_found | no_item | loading | error
//!     title    <item name>
//!     detail   <what else was read, e.g. "level 4, 23%, corrupted">
//!     value    <chaos> <divine> <short text, "3.2 div"> <ninja|trade>
//!              (status ok)
//!     message  <text>
//!     line     <a line read off the tooltip>   (0+, for the log)
//!
//!   request:  "AREA\n" + where the layer sees the game's Client.txt (its
//!             working directory's logs/, or empty)
//!   response: status ok | none
//!     area     <the area the player last entered>
//!     entered  <how many area changes so far: a new number is a new area>
//!     watch    <1|idle|0>   (1: one of the settings' auto_watch_areas;
//!              idle: a Heist area, watched slowly until a display shows)
//!     area_id  <the game's id for it, e.g. HeistMansion9>   (if logged)
//!     message  <why there's no area>   (status none)
//!
//!   request:  "PING\n"   (sent by the layer at game start to wake us up)
//!   response: status ready | loading
//!
//!   Rare/magic items get trade site search filters instead of a poe.ninja
//!   price (status "trade"); so do identified uniques, with their poe.ninja
//!   "price" lines too (if found) and filters only for mods that roll, and
//!   Viscera limbs, with theirs and a filter (with a maximum) per implicit.
//!   Nothing is searched until the layer asks:
//!     filter   <index> <1|0 enabled> <label> <minimum|""> <slider low>
//!              <slider high> <step> <1|0 max roll> <weapon|base|armour|total|enchant|
//!              implicit|exarch|eater|mod|fractured|crafted|map|<influence: shaper, elder, ...>>
//!              <maximum|""> <affix tier|""> <the item's value|"">
//!              <short name, for a column of the listings' values ("Ele res")>
//!              <1|0 slider drawn (else the range only bounds wheel steps)>
//!     option   base <base type> <category label>   (if the base can be
//!                                                  searched on its own)
//!     option   base_only <1|0>
//!     option   corrupted <any|no|yes>
//!     option   item_corrupted <1|0>   (the item is corrupted: the panel's
//!                                     corruption toggle is "yes" or "no" vs any)
//!     option   status <instant|available|any>   (instant buyout, + in person, any)
//!     option   base_preset <1|0>   (the "base item" preset is offered)
//!     option   preset <normal|base>
//!     option   six_link <1|0>   (only 6-linked items; sent for a 6-link only)
//!     option   unidentified <1|0>   (only unidentified items; sent for an
//!                                   unidentified item only)
//!     searched <1|0>
//!   and, once searched:
//!     summary  <amount> <currency name> <currency id> <chaos|""> <range>
//!              <capped 1|0>   (an unsold listing sets it: the item is worth less)
//!     listing  <amount> <currency name> <chaos value|""> <age, e.g. "5m"
//!              or "41h–5d"|""> <count> <currency id>   (same prices grouped)
//!     preview  <kind> <text> <affix tier|"">   (lines describing the first item of the
//!              listing above, for a hover preview: see trade::Preview)
//!     value    <filter index> <text, e.g. "158" or "81%">   (what the listing
//!              above has of an enabled filter; listings are only grouped
//!              when these match)
//!     total    <number of matches>
//!     url      <the search on pathofexile.com>
//!   request:  "TRADE\n" + comma-separated enabled filters, each "index",
//!             "index:minimum" or "index:minimum:maximum" (either may be
//!             empty: default minimum, no maximum) + "\n" +
//!             "base" or "category" + "\n" + any|no|yes + "\n" +
//!             normal|base (preset) + "\n" + instant|available|any (status)
//!             + "\n" + "6" (6-linked only) or "any" + "\n" + "unid"
//!             (unidentified only) or "any" + "\n" + item text: search with
//!             those filters and options.
//!   request:  "PREPARE\n" + normal|base + "\n" + item text: the trade
//!             filters for that preset, without searching ("base": the item
//!             as a crafting base, like APT's "Base item" preset).
//!
//!   request:  "ICON\n" + currency id, or a listed item's image URL (its
//!             "icon" preview line): the PNG, as "png <base64>"
//!
//!   request:  "OPEN\n" + url: open a pathofexile.com trade URL in the
//!             user's browser (the layer can't, from inside the game).
//!
//!   request:  "MAPCHECK\n" + item text: the map danger check
//!   response: status   map | not_map | bad_request
//!     title    <map name>
//!     subtitle <tier, class>
//!     stat     <label> <value>            (quantity, rarity, pack size, ...)
//!     mod      <key> <text> <marks>       (marks: one of d/w/g/- per profile)
//!     profile  <active profile, 1-3>
//!     price    <label> <chaos> <divine> <listings>   (poe.ninja, if listed;
//!              a rare map's is its base's, labelled "white map")
//!   request:  "MAPMARK\n" + profile + "\n" + d|w|g|- + "\n" + key
//!   request:  "MAPPROFILE\n" + profile        (both answer "status ok")
//!   request:  "MAPRESET\n" + profile: that profile's marks back to the
//!             defaults ("status ok")
//!
//!   request:  "SETTINGS\n": the settings (settings.rs), and what the
//!             settings page shows with them
//!   response: status ok
//!     setting       <key> <value>   (one per settings::KEYS)
//!     league_option <name>          (poe.ninja's leagues)
//!     league_current <auto setting> <name>  (what "auto" and
//!                                   "auto-hardcore" pick now)
//!     league_pinned <name>          (POE_LEAGUE overrides the setting)
//!     league        <name>          (prices are loaded for)
//!     map_profile   <1-3>           (the map check's active profile)
//!     version       <daemon version>
//!     log           <where our log is>
//!   request:  "SET\n" + lines of "<key>\t<value>" (a "reset" line first:
//!             from the defaults): saved all together, or none on an
//!             error (bad_request); replies like SETTINGS
//!
//! Normally started by systemd socket activation (poe-waytrade-daemon.socket):
//! systemd owns the socket, starts us on the first connection, and we exit
//! after a while without requests. Run by hand or by the `waytrade` launcher,
//! we bind the socket ourselves (POE_WAYTRADE_SOCKET names another one) and
//! stay up; if something already holds it (another daemon, or systemd's
//! socket), we exit successfully, since the layer has a daemon to talk to.
//! `--exit-with-parent` (the launcher) makes us exit when our parent does.
//! `--version` prints this build's version (see build.rs) and exits.

mod area;
mod art;
mod cdp;
mod item;
mod lookup;
mod mapcheck;
mod ninja;
mod ocr;
mod scan;
mod settings;
mod trade;

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// This build's version, from `git describe` (see build.rs).
const VERSION: &str = env!("WAYTRADE_VERSION");
const SOCKET_NAME: &str = "poe-waytrade";
/// SCANITEM's and SCANFIND's pixels are the biggest requests: a few MiB.
const MAX_FRAME: usize = 16 * 1024 * 1024;
const REFRESH: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(60);
/// When socket-activated, exit after this long without requests.
const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);

struct State {
    db: RwLock<Option<Arc<ninja::PriceDb>>>,
    last_error: Mutex<Option<String>>,
    last_request: Mutex<Instant>,
    trade: Mutex<trade::Trade>,
    map_marks: Mutex<mapcheck::Decisions>,
    settings: Mutex<settings::Settings>,
    /// The leagues poe.ninja lists, for the settings page's league picker.
    leagues: Mutex<Vec<String>>,
    /// Set (and notified) to refresh prices now: the league was changed.
    refresh_now: (Mutex<bool>, Condvar),
    /// Started by systemd (logs in the journal) rather than the launcher.
    socket_activated: bool,
    /// Item displays already priced: (what was read) -> (when, the reply),
    /// so a display seen again costs no lookup or trade search.
    scanned: Mutex<std::collections::HashMap<String, (Instant, String)>>,
    /// The names screen reads are matched against, and when they were got.
    vocabulary: Mutex<Option<(Instant, Arc<scan::Vocabulary>)>>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--version") {
        println!("poe-waytrade-daemon {VERSION}");
        return;
    }
    if args.get(1).map(String::as_str) == Some("scan") {
        std::process::exit(match scan_file(&args[2..]) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("scan failed: {e}");
                1
            }
        });
    }
    if args.get(1).map(String::as_str) == Some("query") {
        std::process::exit(match query(&args[2..]) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("query failed: {e}");
                1
            }
        });
    }

    if args.iter().any(|a| a == "--exit-with-parent") {
        exit_with_parent();
    }
    eprintln!("poe-waytrade-daemon {VERSION}");

    let systemd = systemd_listener();
    let state = Arc::new(State {
        db: RwLock::new(None),
        last_error: Mutex::new(None),
        last_request: Mutex::new(Instant::now()),
        trade: Mutex::new(trade::Trade::new()),
        map_marks: Mutex::new(mapcheck::Decisions::load()),
        settings: Mutex::new(settings::Settings::load()),
        leagues: Mutex::new(Vec::new()),
        refresh_now: (Mutex::new(false), Condvar::new()),
        socket_activated: systemd.is_some(),
        scanned: Mutex::new(std::collections::HashMap::new()),
        vocabulary: Mutex::new(None),
    });

    let listener = match systemd {
        Some(l) => {
            let idle = std::env::var("POE_WAYTRADE_IDLE_SECS")
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
            let name = std::env::var("POE_WAYTRADE_SOCKET").unwrap_or_else(|_| SOCKET_NAME.to_string());
            let addr = SocketAddr::from_abstract_name(name.as_bytes()).expect("socket name");
            match UnixListener::bind_addr(&addr) {
                Ok(l) => {
                    eprintln!("listening on @{name}");
                    l
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    eprintln!("@{name} is already served (another daemon or systemd's socket); exiting");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("can't listen on @{name}: {e}");
                    std::process::exit(1);
                }
            }
        }
    };

    let refresher = state.clone();
    thread::spawn(move || refresh_loop(&refresher));
    let trader = state.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(60));
            // A search in progress holds the lock; it's not idle then anyway.
            if let Ok(mut t) = trader.trade.try_lock() {
                t.close_if_idle();
            }
        }
    });

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

/// For the launcher: SIGTERM when the parent exits, even if it's killed.
fn exit_with_parent() {
    // SAFETY: plain syscalls with no pointers.
    unsafe {
        let parent = libc::getppid();
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        // The parent may have exited before prctl took effect.
        if libc::getppid() != parent {
            std::process::exit(0);
        }
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
        let wanted = state.settings.lock().unwrap().league.clone();
        let result = choose_league(state, &wanted).and_then(|league| ninja::fetch(&league));

        let wait = match result {
            Ok(db) => {
                eprintln!(
                    "loaded {} exchange prices and {} item names for {}",
                    db.exchange.len(),
                    db.items.len(),
                    db.league
                );
                let db = Arc::new(db);
                *state.db.write().unwrap() = Some(db.clone());
                *state.last_error.lock().unwrap() = None;
                prefetch_art(db);
                REFRESH
            }
            Err(e) => {
                eprintln!("price refresh failed: {e}");
                *state.last_error.lock().unwrap() = Some(e);
                RETRY
            }
        };
        // Until it's time, or the league is changed in the settings.
        let (flag, wake) = &state.refresh_now;
        let mut now = flag.lock().unwrap();
        if !*now {
            now = wake.wait_timeout(now, wait).unwrap().0;
        }
        *now = false;
    }
}

/// POE_LEAGUE wins; then the league picked in the settings, while poe.ninja
/// still has it; then the current challenge league ("auto", or its hardcore
/// twin for "auto-hardcore").
fn choose_league(state: &State, wanted: &str) -> Result<String, String> {
    let current = settings::AUTO_LEAGUES.contains(&wanted);
    if let Ok(league) = std::env::var("POE_LEAGUE") {
        return Ok(league);
    }
    let names = match ninja::leagues() {
        Ok(names) => names,
        // poe.ninja's league list failed; a named league may still load.
        Err(_) if !current => return Ok(wanted.to_string()),
        Err(e) => return Err(e),
    };
    *state.leagues.lock().unwrap() = names.clone();
    if !current {
        if names.iter().any(|n| n == wanted) {
            return Ok(wanted.to_string());
        }
        eprintln!("poe.ninja has no {wanted} league (any more); using the current one");
    }
    Ok(ninja::pick_league(&names, wanted == "auto-hardcore"))
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
        respond_default(state, text)
    } else if let Some(rest) = request.strip_prefix("PREPARE\n") {
        let (preset, text) = rest.split_once('\n').unwrap_or((rest, ""));
        respond(state, text, None, trade::Preset::parse(preset))
    } else if let Some(rest) = request.strip_prefix("TRADE\n") {
        let mut parts = rest.splitn(8, '\n');
        let (indices, base, corrupted, preset, status, links, identified, text) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
        );
        let search = TradeRequest {
            enabled: indices
                .split(',')
                .filter_map(|s| {
                    let mut f = s.trim().split(':');
                    let index = f.next()?.parse().ok()?;
                    let min = f.next().and_then(|m| m.parse().ok());
                    let max = f.next().and_then(|m| m.parse().ok());
                    Some(trade::Choice { index, min, max })
                })
                .collect(),
            options: trade::SearchOptions {
                base_only: base == "base",
                corrupted: trade::Corrupted::parse(corrupted),
                preset: trade::Preset::parse(preset),
                status: trade::Status::parse(status),
                six_link: links == "6",
                unidentified: identified == "unid",
            },
        };
        respond(state, text, Some(&search), search.options.preset)
    } else if let Some(rest) = request.strip_prefix("MATCHART\n") {
        match_art(state, rest)
    } else if let Some(rest) = request.strip_prefix("SCANFIND\n") {
        scan_find(rest)
    } else if let Some(rest) = request.strip_prefix("SCANITEM\n") {
        scan_item(state, rest)
    } else if let Some(hint) = request.strip_prefix("AREA\n") {
        area_reply(state, hint.trim())
    } else if let Some(id) = request.strip_prefix("ICON\n") {
        match state.trade.lock().unwrap().icon(id.trim()) {
            Ok(png) => format!("status\tok\npng\t{}\n", base64(&png)),
            Err(e) => format!("status\terror\nmessage\t{}\n", clean(&e)),
        }
    } else if let Some(url) = request.strip_prefix("OPEN\n") {
        open_trade_url(url.trim())
    } else if let Some(text) = request.strip_prefix("MAPCHECK\n") {
        map_check(state, text)
    } else if let Some(rest) = request.strip_prefix("MAPMARK\n") {
        let mut parts = rest.splitn(3, '\n');
        let (profile, decision, key) = (parts.next(), parts.next(), parts.next());
        let profile = profile.and_then(|p| p.parse().ok()).unwrap_or(0);
        let decision = decision.and_then(|d| d.chars().next()).unwrap_or('?');
        let mut marks = state.map_marks.lock().unwrap();
        if marks.set(profile, key.unwrap_or(""), decision) { save_marks(&marks) } else { bad_request("Bad mark") }
    } else if let Some(n) = request.strip_prefix("MAPPROFILE\n") {
        let mut marks = state.map_marks.lock().unwrap();
        match n.trim().parse::<usize>() {
            Ok(p) if (1..=mapcheck::PROFILES).contains(&p) => {
                marks.profile = p;
                save_marks(&marks)
            }
            _ => bad_request("Bad profile"),
        }
    } else if let Some(n) = request.strip_prefix("MAPRESET\n") {
        let mut marks = state.map_marks.lock().unwrap();
        if marks.reset(n.trim().parse().unwrap_or(0)) { save_marks(&marks) } else { bad_request("Bad profile") }
    } else if request == "SETTINGS\n" {
        settings_reply(state)
    } else if let Some(changes) = request.strip_prefix("SET\n") {
        change_settings(state, changes)
    } else if request == "PING\n" {
        let ready = state.db.read().unwrap().is_some();
        format!("status\t{}\n", if ready { "ready" } else { "loading" })
    } else {
        "status\tbad_request\nmessage\tUnknown request\n".to_string()
    };
    write_frame(&mut stream, &response)
}

/// `setting` lines, then what the settings page shows besides:
///   league_option <name>   (poe.ninja's leagues, 0+)
///   league_current <setting> <name>  (what each settings::AUTO_LEAGUES
///                          picks now, once poe.ninja's list is in)
///   league_pinned <name>   (POE_LEAGUE is set: the setting is ignored)
///   league        <name>   (the league prices are loaded for)
///   map_profile   <1-3>    (the map check's active profile)
///   version       <daemon version>
///   log           <where the daemon's log is>
fn settings_reply(state: &State) -> String {
    let mut out = "status\tok\n".to_string();
    out += &state.settings.lock().unwrap().lines();
    let leagues = state.leagues.lock().unwrap().clone();
    for name in &leagues {
        let _ = writeln!(out, "league_option\t{}", clean(name));
    }
    if !leagues.is_empty() {
        for auto in settings::AUTO_LEAGUES {
            let picked = ninja::pick_league(&leagues, *auto == "auto-hardcore");
            let _ = writeln!(out, "league_current\t{auto}\t{}", clean(&picked));
        }
    }
    if let Ok(league) = std::env::var("POE_LEAGUE") {
        let _ = writeln!(out, "league_pinned\t{}", clean(&league));
    }
    if let Some(db) = state.db.read().unwrap().as_ref() {
        let _ = writeln!(out, "league\t{}", clean(&db.league));
    }
    let _ = writeln!(out, "map_profile\t{}", state.map_marks.lock().unwrap().profile);
    let _ = writeln!(out, "version\t{VERSION}");
    let log = if state.socket_activated {
        "journalctl --user -u poe-waytrade-daemon".to_string()
    } else {
        let state_dir = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| "~/.local/state".to_string());
        format!("{state_dir}/poe-waytrade/daemon.log")
    };
    let _ = writeln!(out, "log\t{}", clean(&log));
    out
}

/// "SET\n" + lines of `<key>\t<value>`, all or none; a "reset" line first
/// starts from the defaults. Replies like SETTINGS, with the new values.
fn change_settings(state: &State, changes: &str) -> String {
    let mut settings = state.settings.lock().unwrap();
    let mut new = settings.clone();
    for line in changes.lines().filter(|l| !l.trim().is_empty()) {
        if line.trim() == "reset" {
            new = settings::Settings::default();
            continue;
        }
        let (key, value) = line.split_once('\t').unwrap_or((line, ""));
        if let Err(e) = new.set(key.trim(), value) {
            return bad_request(&clean(&e));
        }
    }
    if new == *settings {
        drop(settings);
        return settings_reply(state);
    }
    if let Err(e) = new.save() {
        return format!("status\terror\nmessage\tCouldn't save the settings: {}\n", clean(&e.to_string()));
    }
    new.apply();
    let league_changed = new.league != settings.league;
    *settings = new;
    drop(settings);
    if league_changed {
        let (flag, wake) = &state.refresh_now;
        *flag.lock().unwrap() = true;
        wake.notify_all();
    }
    settings_reply(state)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n =
            (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn bad_request(message: &str) -> String {
    format!("status\tbad_request\nmessage\t{message}\n")
}

fn save_marks(marks: &mapcheck::Decisions) -> String {
    match marks.save() {
        Ok(()) => "status\tok\n".to_string(),
        Err(e) => format!("status\terror\nmessage\tCouldn't save map marks: {}\n", clean(&e.to_string())),
    }
}

fn map_check(state: &State, text: &str) -> String {
    let Some(item) = item::parse(text) else { return bad_request("Not PoE item text") };
    if !mapcheck::is_map_like(&item) {
        return "status\tnot_map\nmessage\tNot a map\n".to_string();
    }
    let info = mapcheck::analyse(&item);
    let marks = state.map_marks.lock().unwrap();
    let mut out = format!("status\tmap\ntitle\t{}\nsubtitle\t{}\n", clean(&info.title), clean(&info.subtitle));
    for (label, value) in &info.props {
        let _ = writeln!(out, "stat\t{}\t{}", clean(label), clean(value));
    }
    for m in &info.mods {
        let _ = writeln!(out, "mod\t{}\t{}\t{}", clean(&m.key), clean(&m.text), marks.get(&m.key));
    }
    let _ = writeln!(out, "profile\t{}", marks.profile);
    drop(marks);
    // Its poe.ninja price, when it has one (by name, or by base: for a rare
    // or magic map that's only what it's worth unrolled, labelled so).
    let db = state.db.read().unwrap().clone();
    if let Some(db) = db
        && let lookup::Outcome::Found { mut prices, .. } = lookup::lookup(&db, &item)
    {
        prices.truncate(1);
        if item.rarity != "Normal" && item.name.as_ref().is_none_or(|n| !db.items.contains_key(n)) {
            for p in &mut prices {
                p.label = "white map".to_string();
            }
        }
        write_prices(&mut out, prices);
    }
    out
}

/// Only trade search URLs, so a local client can't make us open anything.
fn open_trade_url(url: &str) -> String {
    if !url.starts_with("https://www.pathofexile.com/trade/search/") || url.contains(char::is_whitespace) {
        return "status\tbad_request\nmessage\tNot a trade search URL\n".to_string();
    }
    match std::process::Command::new("xdg-open").arg(url).spawn() {
        Ok(_) => "status\tok\n".to_string(),
        Err(e) => format!("status\terror\nmessage\txdg-open: {}\n", clean(&e.to_string())),
    }
}

/// A trade search the layer asked for.
struct TradeRequest {
    enabled: Vec<trade::Choice>,
    options: trade::SearchOptions,
}

/// Filters and options for an item, and listings if `search` asks for a
/// search. Without one, no request goes to the trade site.
/// `db` is only needed to search (for the league); filters come without it.
fn trade_response(
    state: &State,
    item: &item::Item,
    search: Option<&TradeRequest>,
    preset: trade::Preset,
    db: Option<&ninja::PriceDb>,
) -> String {
    let mut out = String::new();
    let title = item.name.clone().unwrap_or_else(|| item.base.clone());
    let mut trade = state.trade.lock().unwrap();
    let fail = |e: String| {
        eprintln!("trade: {e}");
        format!("status\terror\ntitle\t{}\nmessage\t{}\n", clean(&title), clean(&e))
    };
    let prepared = match trade.prepare(item, preset) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let options = search.map_or(prepared.defaults, |s| s.options);

    let _ = writeln!(out, "status\ttrade\ntitle\t{}", clean(&title));
    for (i, f) in prepared.filters.iter().enumerate() {
        let chosen = search.and_then(|s| s.enabled.iter().find(|c| c.index == i));
        let on = search.map_or(f.default_on, |_| chosen.is_some());
        let min = chosen.and_then(|c| c.min).or(f.min).map(trade::trim_number).unwrap_or_default();
        // Once searched, an empty maximum is the user's: don't put it back.
        let max = match search {
            Some(_) => chosen.and_then(|c| c.max),
            None => f.max,
        };
        let max = max.map(trade::trim_number).unwrap_or_default();
        let (lo, hi) = f
            .slider
            .map_or((String::new(), String::new()), |(lo, hi)| (trade::trim_number(lo), trade::trim_number(hi)));
        let _ = writeln!(
            out,
            "filter\t{i}\t{}\t{}\t{min}\t{lo}\t{hi}\t{}\t{}\t{}\t{max}\t{}\t{}\t{}\t{}",
            on as u8,
            clean(&f.label),
            trade::trim_number(f.step),
            f.max_roll as u8,
            f.section.as_str(),
            f.tier.map(|t| t.to_string()).unwrap_or_default(),
            f.roll.map(trade::trim_number).unwrap_or_default(),
            clean(&trade::column_name(&f.label)),
            f.draw_slider as u8
        );
    }
    if let Some(label) = prepared.category_label {
        let _ = writeln!(out, "option\tbase\t{}\t{label}", clean(&item.base));
    }
    let _ = writeln!(out, "option\tbase_only\t{}", options.base_only as u8);
    let _ = writeln!(out, "option\tcorrupted\t{}", options.corrupted.as_str());
    let _ = writeln!(out, "option\titem_corrupted\t{}", item.corrupted as u8);
    let _ = writeln!(out, "option\tstatus\t{}", options.status.as_str());
    let _ = writeln!(out, "option\tbase_preset\t{}", prepared.base_preset as u8);
    let _ = writeln!(out, "option\tpreset\t{}", options.preset.as_str());
    if item.links >= 6 {
        let _ = writeln!(out, "option\tsix_link\t{}", options.six_link as u8);
    }
    if item.unidentified {
        let _ = writeln!(out, "option\tunidentified\t{}", options.unidentified as u8);
    }

    let Some(search) = search else {
        let _ = writeln!(out, "searched\t0");
        return out;
    };
    let Some(db) = db else { return fail("Still loading league data, try again in a moment".into()) };
    match trade.search(item, &search.enabled, options, &db.league, db) {
        Ok(r) => {
            let _ = writeln!(out, "searched\t1");
            if let Some(s) = &r.summary {
                let chaos = s.chaos.map(|c| format!("{c:.2}")).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "summary\t{}\t{}\t{}\t{chaos}\t{}\t{}",
                    trade::trim_number(s.amount),
                    clean(&s.currency),
                    clean(&s.currency_id),
                    clean(&s.range),
                    s.capped as u8
                );
            }
            for l in &r.listings {
                let chaos = l.chaos.map(|c| format!("{c:.2}")).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "listing\t{}\t{}\t{chaos}\t{}\t{}\t{}",
                    trade::trim_number(l.amount),
                    clean(&l.currency),
                    l.age,
                    l.count,
                    clean(&l.currency_id)
                );
                for (kind, text, tier) in &l.preview {
                    let tier = tier.map(|t| t.to_string()).unwrap_or_default();
                    let _ = writeln!(out, "preview\t{kind}\t{}\t{tier}", clean(text));
                }
                for (index, text) in &l.values {
                    let _ = writeln!(out, "value\t{index}\t{}", clean(text));
                }
            }
            let _ = writeln!(out, "total\t{}\nurl\t{}", r.total, clean(&r.url));
            out
        }
        Err(e) => fail(e),
    }
}

/// A price check: starts with the item's default preset (a white 6-link
/// is priced as a base).
fn respond_default(state: &State, text: &str) -> String {
    let preset = item::parse(text).map_or(trade::Preset::Normal, |item| trade::default_preset(&item));
    respond(state, text, None, preset)
}

fn respond(state: &State, text: &str, trade_search: Option<&TradeRequest>, preset: trade::Preset) -> String {
    let mut out = String::new();
    let Some(item) = item::parse(text) else {
        return "status\tbad_request\nmessage\tNot PoE item text\n".to_string();
    };
    let db = state.db.read().unwrap().clone();
    // Rares never have a poe.ninja price, and the base item preset doesn't
    // use one: show their filters right away, even while poe.ninja loads.
    if trade_search.is_none() && trade::searchable(&item) && (item.rarity == "Rare" || preset == trade::Preset::Base) {
        let mut out = trade_response(state, &item, None, preset, db.as_deref());
        if let Some(db) = &db {
            write_base_prices(&mut out, db, &item);
        }
        return out;
    }
    let Some(db) = db else {
        let message = match &*state.last_error.lock().unwrap() {
            Some(e) => format!("Couldn't load prices: {e}"),
            None => "Loading prices from poe.ninja...".to_string(),
        };
        let status = if message.starts_with("Couldn't") { "error" } else { "loading" };
        return format!("status\t{status}\nmessage\t{}\n", clean(&message));
    };

    if (item.rarity == "Unique" || item.rarity == "Gem" || item.class == "Viscera") && trade::searchable(&item) {
        // Identified uniques, gems and Viscera limbs: trade filters for
        // their rolls (a gem's level and quality), with the poe.ninja price
        // alongside.
        out = trade_response(state, &item, trade_search, preset, Some(&db));
        if out.starts_with("status\ttrade")
            && let lookup::Outcome::Found { prices, .. } = lookup::lookup(&db, &item)
        {
            write_prices(&mut out, prices);
        }
    } else if trade_search.is_some() {
        out = trade_response(state, &item, trade_search, preset, Some(&db));
        write_base_prices(&mut out, &db, &item);
    } else {
        match lookup::lookup(&db, &item) {
            lookup::Outcome::Unsupported(_) if trade::searchable(&item) => {
                out = trade_response(state, &item, None, preset, Some(&db));
                write_base_prices(&mut out, &db, &item);
            }
            outcome => write_lookup(&mut out, outcome),
        }
        if out.starts_with("status\tok") {
            offer_art_match(&mut out, &db, &item);
        }
    }
    let _ = writeln!(out, "league\t{}", clean(&db.league));
    let _ = writeln!(out, "age\t{}", db.fetched_at.elapsed().as_secs() / 60);
    out
}

/// Fetches the art of every unique an art match could need, one at a time,
/// so even a base's first match has it on disk. Downloaded once: the URLs
/// carry a hash of the image, so a cached one never goes stale.
fn prefetch_art(db: Arc<ninja::PriceDb>) {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        let missing: Vec<String> =
            lookup::matchable_arts(&db).into_iter().filter(|url| !trade::item_image_cached(url)).collect();
        drop(db);
        if !missing.is_empty() {
            let started = Instant::now();
            let failed = missing.iter().filter(|url| trade::item_image(url).is_err()).count();
            eprintln!(
                "art: fetched {} unique images in {}s ({failed} failed)",
                missing.len() - failed,
                started.elapsed().as_secs()
            );
        }
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// An unidentified unique that could be one of several arts: say so (and
/// how big the item is), so the layer sends its pixels with MATCHART, and
/// fetch the arts meanwhile.
fn offer_art_match(out: &mut String, db: &ninja::PriceDb, item: &item::Item) {
    let arts = lookup::unique_arts(db, item);
    if arts.is_empty() {
        return;
    }
    let cells: Option<Vec<(u32, u32)>> = arts.iter().map(|(url, _)| art::cells_from_url(url)).collect();
    // The biggest any unique's art is, or the biggest an item can be.
    let (w, h) = cells.and_then(|c| c.into_iter().reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))).unwrap_or((2, 4));
    let _ = writeln!(out, "art\t{w}\t{h}");
    // In case the prefetch hasn't got to them yet: MATCHART follows shortly.
    let urls: Vec<String> = arts.into_iter().map(|(url, _)| url).collect();
    thread::spawn(move || fetch_all(&urls));
}

/// Fetches these images at once (each into the cache), with their results.
fn fetch_all(urls: &[String]) -> Vec<Result<Vec<u8>, String>> {
    thread::scope(|s| {
        let fetches: Vec<_> = urls.iter().map(|url| s.spawn(move || trade::item_image(url))).collect();
        fetches.into_iter().map(|f| f.join().unwrap_or_else(|_| Err("art fetch panicked".into()))).collect()
    })
}

/// MATCHART: see the top of this file, and art.rs.
fn match_art(state: &State, request: &str) -> String {
    let no_match = |message: &str| format!("status\tno_match\nmessage\t{}\n", clean(message));
    let mut parts = request.splitn(3, '\n');
    let (header, pixels, text) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let numbers: Vec<f32> = header.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    let &[width, height, x, y, cell] = numbers.as_slice() else { return bad_request("Bad pixels header") };
    let (width, height) = (width as usize, height as usize);
    let Some(rgb) = art::base64_decode(pixels.trim()) else { return bad_request("Bad pixels") };
    if width == 0 || height == 0 || rgb.len() != width * height * 3 || cell <= 0.0 {
        return bad_request("Bad pixels");
    }
    let capture = art::Capture { width, height, rgb, cursor: (x, y), cell };
    let Some(item) = item::parse(text) else { return bad_request("Not PoE item text") };
    let Some(db) = state.db.read().unwrap().clone() else { return no_match("Prices aren't loaded yet") };
    let arts = lookup::unique_arts(&db, &item);
    if arts.is_empty() {
        return no_match("No uniques to tell apart");
    }

    let mut scores = Vec::new();
    let urls: Vec<String> = arts.iter().map(|(url, _)| url.clone()).collect();
    for ((url, names), png) in arts.iter().zip(fetch_all(&urls)) {
        match png.and_then(|png| art::Art::from_png(&png, art::cells_from_url(url))) {
            Ok(a) => scores.push(art::score(&capture, &a, item.sockets)),
            // Without every art, the best match could still be the wrong one.
            Err(e) => {
                eprintln!("art: {e}");
                return no_match(&format!("Couldn't get the art of {}", names.join(" / ")));
            }
        }
    }
    let scored: Vec<String> =
        arts.iter().zip(&scores).map(|((_, names), s)| format!("{} {s:.2}", names.join(" / "))).collect();
    eprintln!("art match for {}: {}", item.base, scored.join(", "));
    let Some(best) = art::decide(&scores) else { return no_match("No art matched clearly") };

    let names = &arts[best].1;
    let mut out = String::new();
    write_lookup(&mut out, lookup::unique_prices_named(&db, &item, names));
    if out.starts_with("status\tok") {
        let _ = writeln!(out, "matched\t{}\t{:.2}", clean(&names.join(" / ")), scores[best]);
    }
    let _ = writeln!(out, "league\t{}", clean(&db.league));
    let _ = writeln!(out, "age\t{}", db.fetched_at.elapsed().as_secs() / 60);
    out
}

/// A base's or cluster jewel's poe.ninja price next to its trade filters.
fn write_base_prices(out: &mut String, db: &ninja::PriceDb, item: &item::Item) {
    if out.starts_with("status\ttrade") {
        write_prices(out, lookup::base_prices(db, item));
    }
}

fn write_lookup(out: &mut String, outcome: lookup::Outcome) {
    match outcome {
        lookup::Outcome::Found { title, prices } => {
            let _ = writeln!(out, "status\tok\ntitle\t{}", clean(&title));
            write_prices(out, prices);
        }
        lookup::Outcome::NotFound(m) => {
            let _ = writeln!(out, "status\tnot_found\nmessage\t{}", clean(&m));
        }
        lookup::Outcome::Unsupported(m) => {
            let _ = writeln!(out, "status\tunsupported\nmessage\t{}", clean(&m));
        }
    }
}

fn write_prices(out: &mut String, prices: Vec<lookup::Price>) {
    for p in prices {
        let listings = p.listings.map(|n| n.to_string()).unwrap_or_default();
        let unique = p.unique.map(|u| format!("\t{}", clean(&u))).unwrap_or_default();
        let _ = writeln!(out, "price\t{}\t{:.2}\t{:.4}\t{listings}{unique}", clean(&p.label), p.chaos, p.divine);
    }
}

// ---- item displays (scan.rs) ------------------------------------------------

/// How long a display's price (or that it has none) is reused...
const SCAN_REUSE: Duration = Duration::from_secs(10 * 60);
/// ...and a failed lookup (a trade search over the rate limit, say): not
/// tried again each time the display comes back into view.
const SCAN_RETRY: Duration = Duration::from_secs(30);
/// How long the trade site's names are used before they're asked for again
/// (they're cached on disk for a day anyway).
const VOCABULARY_AGE: Duration = Duration::from_secs(60 * 60);

/// "<numbers...>\n<base64>" -> (numbers, decoded bytes).
fn pixels_request(request: &str, count: usize) -> Option<(Vec<f32>, Vec<u8>)> {
    let (header, pixels) = request.split_once('\n')?;
    let numbers: Vec<f32> = header.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    if numbers.len() != count {
        return None;
    }
    Some((numbers, art::base64_decode(pixels.trim())?))
}

/// SCANFIND: see the top of this file.
fn scan_find(request: &str) -> String {
    let Some((n, pixels)) = pixels_request(request, 4) else { return bad_request("Bad SCANFIND") };
    let (width, height) = (n[0] as usize, n[1] as usize);
    if width == 0 || height == 0 || pixels.len() != width * height || n[2] < n[0] || n[3] < n[1] {
        return bad_request("Bad SCANFIND pixels");
    }
    // A display check is starting: have the OCR models ready.
    ocr::prepare();
    let frame = scan::Gray { width, height, pixels };
    let mut out = "status\tok\n".to_string();
    for p in scan::find_plates(&frame, n[2] as u32, n[3] as u32) {
        let (cx, cy, cw, ch) = p.crop;
        let _ = writeln!(
            out,
            "plate\t{:.0}\t{:.0}\t{:.0}\t{:.0}\t{cx}\t{cy}\t{cw}\t{ch}\t{}",
            p.x,
            p.y,
            p.w,
            p.h,
            p.kind.as_str()
        );
    }
    out
}

/// The names to read items by: the trade site's (every item), or failing
/// that (no browser for the trade site), poe.ninja's (every priced one).
fn vocabulary(state: &State, db: Option<&ninja::PriceDb>) -> Option<Arc<scan::Vocabulary>> {
    let mut slot = state.vocabulary.lock().unwrap();
    if let Some((at, v)) = slot.as_ref()
        && at.elapsed() < VOCABULARY_AGE
    {
        return Some(v.clone());
    }
    let names = match state.trade.lock().unwrap().item_names() {
        Ok(names) if !names.is_empty() => names,
        result => {
            if let Err(e) = result {
                eprintln!("scan: the trade site's item names: {e}; using poe.ninja's");
            }
            Arc::new(ninja_names(db?))
        }
    };
    let v = scan::Vocabulary::shared(names);
    *slot = Some((Instant::now(), v.clone()));
    Some(v)
}

/// poe.ninja's item names, grouped like the trade site's.
fn ninja_names(db: &ninja::PriceDb) -> Vec<trade::NamedItem> {
    let mut names = Vec::new();
    for (name, lines) in &db.items {
        let Some(line) = lines.first() else { continue };
        let (name, base, group) = match (&line.base_type, line.gem_level) {
            (_, Some(_)) => (None, name.clone(), "gem"),
            (Some(base), None) => (Some(name.clone()), base.clone(), "unique"),
            (None, None) => (None, name.clone(), "other"),
        };
        names.push(trade::NamedItem { name, base, group: group.into() });
    }
    for name in db.exchange.keys() {
        names.push(trade::NamedItem { name: None, base: name.clone(), group: "currency".into() });
    }
    for base in db.bases.keys() {
        names.push(trade::NamedItem { name: None, base: base.clone(), group: "base".into() });
    }
    names
}

/// SCANITEM: see the top of this file.
fn scan_item(state: &State, request: &str) -> String {
    let Some((n, rgb)) = pixels_request(request, 6) else { return bad_request("Bad SCANITEM") };
    let (width, height) = (n[0] as usize, n[1] as usize);
    if width == 0 || height == 0 || rgb.len() != width * height * 3 {
        return bad_request("Bad SCANITEM pixels");
    }
    let (plate_x, plate_y, plate_w, plate_h) = (n[2], n[3], n[4], n[5]);
    let Some(db) = state.db.read().unwrap().clone() else {
        return "status\tloading\nmessage\tLoading prices from poe.ninja...\n".to_string();
    };
    let Some(vocabulary) = vocabulary(state, Some(&db)) else {
        return "status\tloading\nmessage\tLoading item names...\n".to_string();
    };
    let started = Instant::now();
    let lines = match ocr::read(&rgb, width, height) {
        Ok(lines) => lines,
        Err(e) => return format!("status\terror\nmessage\t{}\n", clean(&format!("OCR: {e}"))),
    };
    let lines = scan::tooltip_lines(&lines, plate_x + plate_w / 2.0, plate_y + plate_h, width as f32);
    let read = scan::read_item(&lines, &vocabulary, |text| state.trade.lock().unwrap().canonical_mod(text));
    let read = match read {
        Ok(read) => read,
        Err(e) => {
            eprintln!("scan: {e} ({}ms)", started.elapsed().as_millis());
            let mut out = "status\tno_item\nmessage\tNo item recognised\n".to_string();
            for l in &lines {
                let _ = writeln!(out, "line\t{}", clean(&l.text));
            }
            return out;
        }
    };
    let detail = scan_detail(&read);
    let title = read.item.name.clone().unwrap_or_else(|| read.item.base.clone());
    let cache_key = format!("{title}\n{}\n{detail}", read.item.base);
    let reusable = |(at, reply): &(Instant, String)| {
        at.elapsed() < if reply.starts_with("status\terror") { SCAN_RETRY } else { SCAN_REUSE }
    };
    let cached = state.scanned.lock().unwrap().get(&cache_key).filter(|c| reusable(c)).cloned();
    let mut out = match cached {
        Some((_, reply)) => reply,
        None => {
            let reply = price_screen_item(state, &db, &read, &title, &detail);
            if reply.starts_with("status\terror") {
                let message = reply.lines().find_map(|l| l.strip_prefix("message\t")).unwrap_or("");
                eprintln!("scan: {title}: {message}");
            }
            let mut scanned = state.scanned.lock().unwrap();
            scanned.retain(|_, c| reusable(c));
            scanned.insert(cache_key, (Instant::now(), reply.clone()));
            reply
        }
    };
    eprintln!("scan: {title} ({detail}) in {}ms", started.elapsed().as_millis());
    for l in &read.lines {
        let _ = writeln!(out, "line\t{}", clean(l));
    }
    out
}

/// What was read besides the name, as the label shows it.
fn scan_detail(read: &scan::ScreenItem) -> String {
    let item = &read.item;
    let mut parts = Vec::new();
    if let Some(level) = item.gem_level {
        parts.push(format!("level {level}"));
    }
    if item.quality > 0 {
        parts.push(format!("{}%", item.quality));
    }
    if let Some(n) = item.stack_size.filter(|&n| n > 1) {
        parts.push(format!("x{n}"));
    }
    if let Some(ilvl) = item.item_level {
        parts.push(format!("ilvl {ilvl}"));
    }
    if let Some(xp) = read.stored_experience {
        parts.push(format!("{}M xp", xp / 1_000_000));
    }
    if item.corrupted {
        parts.push("corrupted".into());
    }
    if item.rarity == "Rare" {
        parts.push(format!("{} mods", item.mods.len()));
    }
    parts.join(", ")
}

/// "3.2 div" for a divine or more, else "45c" ("0.5c" under one).
fn short_price(chaos: f64, divine: f64) -> String {
    if divine >= 1.0 {
        format!("{} div", trade::trim_number((divine * 10.0).round() / 10.0))
    } else if chaos >= 1.0 {
        format!("{chaos:.0}c")
    } else {
        format!("{}c", trade::trim_number((chaos * 10.0).round() / 10.0))
    }
}

fn price_screen_item(state: &State, db: &ninja::PriceDb, read: &scan::ScreenItem, title: &str, detail: &str) -> String {
    let item = &read.item;
    let head = format!("title\t{}\ndetail\t{}\n", clean(title), clean(detail));
    let from_trade = |result: Result<trade::TradeResult, String>| match result {
        Ok(r) => match r.summary.as_ref().and_then(|s| s.chaos) {
            Some(chaos) => {
                let divine = db.to_divine(chaos);
                format!(
                    "status\tok\n{head}value\t{chaos:.2}\t{divine:.4}\t{}\ttrade\nmessage\t{} listed\n",
                    clean(&short_price(chaos, divine)),
                    r.total
                )
            }
            None => format!("status\tnot_found\n{head}message\tNo priced listings on the trade site\n"),
        },
        Err(e) => format!("status\terror\n{head}message\t{}\n", clean(&e)),
    };

    if let Some(xp) = read.stored_experience {
        // A lens is worth its experience: listings with at least about as much.
        let result = state.trade.lock().unwrap().search_property(
            &item.base,
            ("misc_filters", "stored_experience"),
            (xp as f64 * 0.95).floor(),
            &db.league,
            db,
        );
        return from_trade(result);
    }
    let gear = matches!(item.rarity.as_str(), "Rare" | "Magic" | "Normal");
    let trinket = item.base.contains("Trinket");
    // An experimented base sells as its base, whatever its mods.
    if gear
        && !trinket
        && let Some(p) = lookup::experimented_price(db, item)
    {
        return format!(
            "status\tok\n{head}value\t{:.2}\t{:.4}\t{}\tninja\nmessage\texperimented base, {}\n",
            p.chaos,
            p.divine,
            clean(&short_price(p.chaos, p.divine)),
            clean(&p.label)
        );
    }
    // An enchanted one (Heist's Enchanted Armaments) by its type and
    // enchant: the Base item preset, whose enchant filters start ticked.
    let enchanted = gear && !trinket && item.mods.iter().any(|m| m.kind == item::ModKind::Enchant);
    if enchanted || (gear && item.rarity == "Rare") {
        // Else a rare (a trinket, say): its base with all its mods.
        let preset = if enchanted { trade::Preset::Base } else { trade::Preset::Normal };
        let mut trade = state.trade.lock().unwrap();
        let prepared = match trade.prepare(item, preset) {
            Ok(p) => p,
            Err(e) => return format!("status\terror\n{head}message\t{}\n", clean(&e)),
        };
        let enabled: Vec<trade::Choice> = prepared
            .filters
            .iter()
            .enumerate()
            .filter(|(_, f)| if enchanted { f.default_on } else { f.min.is_some() })
            .map(|(index, f)| trade::Choice { index, min: f.min, max: None })
            .collect();
        let options = trade::SearchOptions { base_only: true, ..prepared.defaults };
        return from_trade(trade.search(item, &enabled, options, &db.league, db));
    }

    let outcome = match lookup::lookup(db, item) {
        lookup::Outcome::Unsupported(_) if item.rarity == "Normal" => {
            let prices = lookup::base_prices(db, item);
            if prices.is_empty() {
                lookup::Outcome::NotFound(if item.item_level.is_none() {
                    "No item level read: bases are priced by it".into()
                } else {
                    format!("No poe.ninja price for {title}")
                })
            } else {
                lookup::Outcome::Found { title: title.to_string(), prices }
            }
        }
        outcome => outcome,
    };
    match outcome {
        lookup::Outcome::Found { prices, .. } => {
            // A stack's own price is its last line; anything else, the first
            // (the most listed).
            let price = if item.stack_size.is_some_and(|n| n > 1) { prices.last() } else { prices.first() };
            let Some(p) = price else { return format!("status\tnot_found\n{head}message\tNo price\n") };
            format!(
                "status\tok\n{head}value\t{:.2}\t{:.4}\t{}\tninja\n",
                p.chaos,
                p.divine,
                clean(&short_price(p.chaos, p.divine))
            )
        }
        lookup::Outcome::NotFound(m) | lookup::Outcome::Unsupported(m) => {
            format!("status\tnot_found\n{head}message\t{}\n", clean(&m))
        }
    }
}

/// AREA: see the top of this file.
fn area_reply(state: &State, hint: &str) -> String {
    let (setting, watch_areas, auto, heist) = {
        let s = state.settings.lock().unwrap();
        (s.client_log.clone(), s.auto_watch_areas.clone(), s.auto_watch, s.heist_watch)
    };
    let Some(path) = area::find_log(&setting, hint) else {
        return "status\tnone\nmessage\tNo Client.txt found\n".to_string();
    };
    let mut follower = area::FOLLOWER.lock().unwrap();
    let f = follower.get_or_insert_with(area::Follower::default);
    if let Err(e) = f.poll(&path) {
        return format!("status\tnone\nmessage\t{}\n", clean(&e));
    }
    let Some(area) = f.area.clone() else { return "status\tnone\nmessage\tNo area in Client.txt yet\n".to_string() };
    let area_id = f.area_id.clone().unwrap_or_default();
    let watch = if auto && watch_areas.contains(&area) {
        "1"
    } else if heist && area::is_heist(&area_id) {
        "idle"
    } else {
        "0"
    };
    format!(
        "status\tok\narea\t{}\nentered\t{}\nwatch\t{watch}\narea_id\t{}\n",
        clean(&area),
        f.entered,
        clean(&area_id)
    )
}

/// `poe-waytrade-daemon scan <frame.png>`: finds the item displays in a
/// screenshot and reads them, as the layer's requests would (no prices),
/// for testing without the game.
fn scan_file(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("usage: poe-waytrade-daemon scan <frame.png>")?;
    let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut reader = png::Decoder::new(io::BufReader::new(file)).read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("png too big")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other => return Err(format!("{other:?} PNG: RGB or RGBA please")),
    };
    let rgb: Vec<u8> = buf[..w * h * channels].chunks(channels).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let gray: Vec<u8> =
        rgb.chunks(3).map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8).collect();
    let (hw, hh) = (w / 2, h / 2);
    let mut half = vec![0u8; hw * hh];
    for y in 0..hh {
        for x in 0..hw {
            let at = |dx: usize, dy: usize| gray[(y * 2 + dy) * w + x * 2 + dx] as u32;
            half[y * hw + x] = ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1)) / 4) as u8;
        }
    }
    let started = Instant::now();
    let plates = scan::find_plates(&scan::Gray { width: hw, height: hh, pixels: half }, w as u32, h as u32);
    println!("{} plates in {}ms", plates.len(), started.elapsed().as_millis());
    let mut trade = trade::Trade::new();
    let names = trade.item_names()?;
    let vocabulary = scan::Vocabulary::shared(names);
    for p in plates {
        let (cx, cy, cw, ch) = p.crop;
        let (cx, cy, cw, ch) = (cx as usize, cy as usize, cw as usize, ch as usize);
        let mut crop = Vec::with_capacity(cw * ch * 3);
        for y in cy..cy + ch {
            crop.extend_from_slice(&rgb[(y * w + cx) * 3..(y * w + cx + cw) * 3]);
        }
        let started = Instant::now();
        let lines = ocr::read(&crop, cw, ch)?;
        let lines = scan::tooltip_lines(&lines, p.x - cx as f32 + p.w / 2.0, p.y - cy as f32 + p.h, cw as f32);
        let read = scan::read_item(&lines, &vocabulary, |t| trade.canonical_mod(t));
        println!("plate at {:.0},{:.0} ({}ms):", p.x, p.y, started.elapsed().as_millis());
        match read {
            Ok(read) => {
                println!("  {:?} {:?} {} ({})", read.item.rarity, read.item.name, read.item.base, scan_detail(&read))
            }
            Err(e) => println!("  {e}"),
        }
    }
    Ok(())
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

/// `poe-waytrade-daemon query [--map] [file]`: send item text (file or stdin)
/// to the running daemon as a price check (or map check) and print its
/// answer, for testing without the game. POE_WAYTRADE_SOCKET picks another
/// socket name, for a test daemon.
fn query(args: &[String]) -> io::Result<()> {
    let map = args.first().is_some_and(|a| a == "--map");
    let path = args.get(if map { 1 } else { 0 });
    let text = match path {
        Some(p) => std::fs::read_to_string(p)?,
        None => io::read_to_string(io::stdin())?,
    };
    let name = std::env::var("POE_WAYTRADE_SOCKET").unwrap_or_else(|_| "poe-waytrade".into());
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let mut stream = UnixStream::connect_addr(&addr)?;
    write_frame(&mut stream, &format!("{}\n{text}", if map { "MAPCHECK" } else { "PRICE" }))?;
    print!("{}", read_frame(&mut stream)?);
    Ok(())
}
