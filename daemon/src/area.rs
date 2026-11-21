//! Which area the player is in, from the game's log (Client.txt): it writes
//! "... : You have entered Zealot's Vault." on every area change. The layer
//! asks with AREA once a second, to price item displays by itself in the
//! areas that are only loot (settings `auto_watch_areas`), and to keep an eye
//! out for them in Heist.
//!
//! Just before that line the game logs the area's own id: `Generating level
//! 83 area "HeistMansion9" with seed ...`. Names can't tell a Heist's
//! "Laboratory" from the Atlas map's (`MapWorldsLaboratory`); the id can.
//!
//! The log is found from, in order: the `client_log` setting, the path the
//! layer sees from inside the game (its working directory is the game's),
//! and the Steam libraries' "Path of Exile" folders.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

const ENTERED: &str = "] : You have entered ";
const GENERATING: &str = "] Generating level ";
/// How far back to look for the current area when we start following.
const TAIL: u64 = 256 * 1024;
/// Checks the file at most this often.
const POLL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Follower {
    path: Option<PathBuf>,
    /// Where the next read starts.
    pos: u64,
    /// The file's identity, to notice it was replaced.
    inode: u64,
    /// The latest area, and how many area changes we've seen (so entering
    /// the same area again is a change too).
    pub area: Option<String>,
    /// That area's id ("HeistMansion9"), if the log named it.
    pub area_id: Option<String>,
    pub entered: u64,
    /// The latest "Generating level" id, for the next "You have entered".
    generating: Option<String>,
    last_poll: Option<Instant>,
    /// Bytes after the last newline, waiting for the rest of their line.
    partial: Vec<u8>,
}

impl Follower {
    /// Reads what the log gained since last time. `path`: the log to follow
    /// (another one than before starts over).
    pub fn poll(&mut self, path: &Path) -> Result<(), String> {
        if self.last_poll.is_some_and(|t| t.elapsed() < POLL) && self.path.as_deref() == Some(path) {
            return Ok(());
        }
        self.last_poll = Some(Instant::now());
        let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        let len = meta.len();
        let first_open = self.path.as_deref() != Some(path);
        if first_open || inode != self.inode || len < self.pos {
            // A file we haven't followed: the current area is the last one
            // near its end. One the game started over: read it all.
            self.path = Some(path.to_path_buf());
            self.inode = inode;
            self.pos = if first_open { len.saturating_sub(TAIL) } else { 0 };
            self.partial.clear();
            let entered = self.entered;
            self.read_from(&mut file, len)?;
            if first_open {
                // What was already there isn't a change of its own, but the
                // area is known now: one change, from not knowing it.
                self.entered = entered + u64::from(self.area.is_some());
            }
            return Ok(());
        }
        self.read_from(&mut file, len)
    }

    fn read_from(&mut self, file: &mut File, len: u64) -> Result<(), String> {
        if len <= self.pos {
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.pos)).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        file.take(len - self.pos).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        self.pos += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else { return Ok(()) };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        for line in String::from_utf8_lossy(&complete).lines() {
            if let Some(id) = generated_area(line) {
                self.generating = Some(id.to_string());
            } else if let Some(area) = entered_area(line) {
                self.area = Some(area.to_string());
                self.area_id = self.generating.take();
                self.entered += 1;
            }
        }
        Ok(())
    }
}

/// "2026/11/20 20:42:11 37588519 cffb06be [INFO Client 396] : You have
/// entered Karui Shores." -> "Karui Shores".
fn entered_area(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(ENTERED)?;
    Some(rest.trim_end().strip_suffix('.').unwrap_or(rest.trim_end()))
}

/// `... [DEBUG Client 396] Generating level 83 area "HeistMansion9" with
/// seed 1234` -> "HeistMansion9".
fn generated_area(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(GENERATING)?;
    let (_, rest) = rest.split_once(" area \"")?;
    Some(rest.split_once('"')?.0)
}

/// A Heist contract or Grand Heist area (not the Rogue Harbour).
pub fn is_heist(area_id: &str) -> bool {
    area_id.starts_with("Heist") && area_id != "HeistHub"
}

/// The log to follow: the setting, the layer's guess, or the most recently
/// written one in the Steam libraries.
pub fn find_log(setting: &str, hint: &str) -> Option<PathBuf> {
    if !setting.is_empty() {
        return Some(PathBuf::from(setting));
    }
    if !hint.is_empty() && Path::new(hint).is_file() {
        return Some(PathBuf::from(hint));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let mut libraries = Vec::new();
    for steam in [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"] {
        let root = home.join(steam);
        libraries.push(root.clone());
        if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libraries.extend(library_paths(&vdf));
        }
    }
    libraries
        .into_iter()
        .map(|l| l.join("steamapps/common").join(crate::game::current().steam_dir()).join("logs/Client.txt"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _): &(SystemTime, PathBuf)| *t)
        .map(|(_, p)| p)
}

/// The `"path"` values in Steam's libraryfolders.vdf.
fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("\"path\"")?.trim();
            Some(PathBuf::from(rest.strip_prefix('"')?.strip_suffix('"')?.replace("\\\\", "\\")))
        })
        .collect()
}

/// The daemon's one follower, shared by AREA requests.
pub static FOLLOWER: Mutex<Option<Follower>> = Mutex::new(None);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn area_lines() {
        assert_eq!(
            entered_area("2026/11/20 20:42:11 37588519 cffb06be [INFO Client 396] : You have entered Zealot's Vault."),
            Some("Zealot's Vault")
        );
        assert_eq!(entered_area("2026/11/20 20:42:11 37588519 cffb06be [INFO Client 396] : Connected"), None);
        // A player's chat can't fake it: chat lines have the speaker before ": ".
        assert_eq!(entered_area("... [INFO Client 396] @From Bob: You have entered Zealot's Vault."), None);
    }

    #[test]
    fn heist_by_area_id() {
        let log = "2026/09/20 21:02:11 1 c [DEBUG Client 396] Generating level 83 area \"HeistMansion9\" with seed 7\n\
                   2026/09/20 21:02:13 1 c [INFO Client 396] : You have entered Mansion.\n";
        let dir = std::env::temp_dir().join(format!("waytrade-heist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Client.txt");
        std::fs::write(&path, log).unwrap();
        let mut f = Follower::default();
        f.poll(&path).unwrap();
        assert_eq!((f.area.as_deref(), f.area_id.as_deref()), (Some("Mansion"), Some("HeistMansion9")));
        assert!(is_heist(f.area_id.as_deref().unwrap()));
        // The Atlas map of the same name, and the Rogue Harbour.
        let more = "2026/09/20 21:10:00 1 c [DEBUG Client 396] Generating level 81 area \"MapWorldsLaboratory\" with seed 9\n\
                    2026/09/20 21:10:02 1 c [INFO Client 396] : You have entered Laboratory.\n";
        std::fs::write(&path, format!("{log}{more}")).unwrap();
        f.last_poll = None;
        f.poll(&path).unwrap();
        assert_eq!(f.area_id.as_deref(), Some("MapWorldsLaboratory"));
        assert!(!is_heist("MapWorldsLaboratory"));
        assert!(!is_heist("HeistHub"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn library_folders() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/u/.local/share/Steam\"\n\
                   \t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/mnt/large/SteamLibrary\"\n\t\t\"label\"\t\t\"\"\n\t}\n}\n";
        assert_eq!(
            library_paths(vdf),
            [PathBuf::from("/home/u/.local/share/Steam"), PathBuf::from("/mnt/large/SteamLibrary")]
        );
    }

    #[test]
    fn follows_area_changes() {
        let dir = std::env::temp_dir().join(format!("waytrade-area-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Client.txt");
        let line = |area: &str| format!("2026/11/20 20:42:11 1 c [INFO Client 396] : You have entered {area}.\n");
        std::fs::write(&path, line("Karui Shores") + &line("Zealot's Vault")).unwrap();

        let mut f = Follower::default();
        f.poll(&path).unwrap();
        assert_eq!(f.area.as_deref(), Some("Zealot's Vault"), "the area we start in");
        let first = f.entered;
        assert!(first > 0);

        // A line written in two parts counts once, when it's whole.
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let next = line("Ritualist's Hideout");
        file.write_all(&next.as_bytes()[..20]).unwrap();
        f.last_poll = None;
        f.poll(&path).unwrap();
        assert_eq!(f.entered, first);
        file.write_all(&next.as_bytes()[20..]).unwrap();
        f.last_poll = None;
        f.poll(&path).unwrap();
        assert_eq!(f.area.as_deref(), Some("Ritualist's Hideout"));
        assert_eq!(f.entered, first + 1);

        // The game starting the file over.
        std::fs::write(&path, line("Zealot's Vault")).unwrap();
        f.last_poll = None;
        f.poll(&path).unwrap();
        assert_eq!(f.area.as_deref(), Some("Zealot's Vault"));
        assert!(f.entered > first + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
