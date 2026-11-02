//! Embed the git identity in the binary: `guillotine --version` prints the
//! output of `git describe --tags --always --dirty`, so any build (local or
//! a downloaded CI artifact) answers "which code is this?". The CI artifact
//! filenames are identical across runs, which has already caused a
//! stale-artifact mixup once.

use std::process::Command;

fn main() {
    // Rerun when the checked-out commit changes so the identity never goes
    // stale inside target/. Skipped (with no warning) for a source tree
    // without git history, where the path does not exist.
    if std::path::Path::new(".git/HEAD").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
    }
    println!("cargo:rerun-if-changed=build.rs");

    let described = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output();
    let version = match described {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                fallback()
            } else {
                s
            }
        }
        _ => fallback(),
    };
    println!("cargo:rustc-env=GUILLOTINE_GIT_VERSION={version}");
}

/// Source without git history (e.g. an exported tarball): fall back to the
/// crate version so --version still works and is visibly not a git build.
fn fallback() -> String {
    format!("{}-nogit", env!("CARGO_PKG_VERSION"))
}
