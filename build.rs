//! What this binary was built from, so a gate can tell which one it is asking.
//!
//! A number in the ledger is worth what the gate that produced it could prove
//! about the build it was talking to, and until now nothing could: the node
//! reported `"build_hash": "unknown"`, so a gate that reached a node left over
//! from another session -- a different binary on the same port -- counted its
//! answers and said nothing. The commit is compiled in here and reported by
//! `GET /`, and `--build-hash` prints the same string without starting a node,
//! so the two can be compared before anything is counted.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=VELOSEARCH_BUILD_HASH");
    // a build from a tarball rather than a checkout says so, the way it
    // already did, rather than pretending to a commit it does not know
    let hash =
        std::env::var("VELOSEARCH_BUILD_HASH").ok().filter(|h| !h.is_empty()).or_else(|| {
            let out = Command::new("git").args(["rev-parse", "HEAD"]).output().ok()?;
            if !out.status.success() {
                return None;
            }
            let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
            if sha.is_empty() {
                return None;
            }
            // a build with changes that are not in the commit is not that
            // commit, and a gate comparing the two should be told so
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .ok()
                .is_some_and(|o| !o.stdout.is_empty());
            Some(if dirty { format!("{sha}-dirty") } else { sha })
        });
    println!("cargo:rustc-env=VELOSEARCH_BUILD_HASH={}", hash.unwrap_or_else(|| "unknown".into()));
}
