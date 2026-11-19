//! Sets WAYTRADE_VERSION to the build's version from `git describe`:
//! "0.21.0" on a release tag, "0.21.0-5-gcf90910" five commits after it, with
//! "-dirty" for uncommitted changes. Without a tag it's Cargo's version plus
//! the commit ("0.21.0-gcf90910"); without git (a source tarball) it's Cargo's
//! version, unless WAYTRADE_VERSION is set in the environment.
//! cmake/version.cmake does the same for the layer; keep them in step.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=WAYTRADE_VERSION");
    let version = match std::env::var("WAYTRADE_VERSION") {
        Ok(v) if !v.is_empty() => v,
        _ => describe().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
    };
    println!("cargo:rustc-env=WAYTRADE_VERSION={version}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(env!("CARGO_MANIFEST_DIR")).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn describe() -> Option<String> {
    let out = git(&["describe", "--tags", "--match", "v[0-9]*", "--dirty", "--always"])?;
    rerun_on_git_changes();
    Some(match out.strip_prefix('v') {
        Some(tagged) => tagged.to_string(),
        // --always gave just the commit: no release tag in the history yet.
        None => format!("{}-g{out}", env!("CARGO_PKG_VERSION")),
    })
}

/// Rerun when HEAD moves (commit, checkout) or tags change. Edits that make
/// the tree dirty are picked up only when something else rebuilds us.
fn rerun_on_git_changes() {
    let (Some(git_dir), Some(common_dir)) =
        (git(&["rev-parse", "--absolute-git-dir"]), git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]))
    else {
        return;
    };
    let watch = |path: &Path| {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    };
    watch(&Path::new(&git_dir).join("HEAD"));
    let common = Path::new(&common_dir);
    watch(&common.join("packed-refs"));
    watch(&common.join("refs/tags"));
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch(&common.join(branch));
    }
}
