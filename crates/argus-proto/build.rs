//! Stamps `ARGUS_BUILD`: the crate version plus, when built from a git
//! checkout, the commit it was built at (e.g. `0.0.1+458efa2c1d`). Used to
//! tell a manager or holder left running from an older install.

use std::path::Path;
use std::process::Command;

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let build = match git(&["rev-parse", "--short=10", "HEAD"]) {
        Some(commit) => format!("{version}+{commit}"),
        None => version,
    };
    println!("cargo:rustc-env=ARGUS_BUILD={build}");

    // Re-stamp when HEAD moves: a checkout changes HEAD, a commit changes the
    // branch it points at. Only existing paths, or cargo reruns every build.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let dir = Path::new(&dir);
        let mut watched = vec![dir.join("HEAD"), dir.join("packed-refs")];
        if let Ok(head) = std::fs::read_to_string(dir.join("HEAD"))
            && let Some(branch) = head.trim().strip_prefix("ref: ")
        {
            watched.push(dir.join(branch));
        }
        for path in watched.iter().filter(|p| p.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(env!("CARGO_MANIFEST_DIR")).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !text.trim().is_empty()).then(|| text.trim().to_string())
}
