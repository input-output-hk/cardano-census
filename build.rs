//! Stamp the binary with the git revision it was built from.
//!
//! The nix build passes `GIT_REV` from the flake's `self.rev`; a cargo build
//! in a checkout asks git and marks the tree dirty when it is.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=GIT_REV");

    let rev = match std::env::var("GIT_REV") {
        Ok(rev) if !rev.is_empty() => rev,
        _ => match git(&["rev-parse", "HEAD"]) {
            Some(rev) => {
                // Rebuild when the checked-out commit changes.
                if let Some(head) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
                    println!("cargo:rerun-if-changed={head}");
                }
                let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
                if dirty {
                    format!("{rev}-dirty")
                } else {
                    rev
                }
            }
            None => "unknown".to_string(),
        },
    };

    println!("cargo:rustc-env=CARDANO_CENSUS_GIT_REV={rev}");
}
