//! Embeds the git commit as `CHOTU_GIT_SHA` for `/v1/health`. Falls back to
//! "unknown" when git or the repository is unavailable (e.g. a source tarball).

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let sha = git(&["rev-parse", "--short=7", "HEAD"]);
    if sha.is_some() {
        // Rebuild when HEAD moves: a checkout changes HEAD, a commit changes the
        // branch ref (loose, or in packed-refs after `git gc`).
        let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
        watched.extend(git(&["symbolic-ref", "-q", "HEAD"]));
        for path in watched {
            // A missing watched file would make cargo rerun this on every build.
            let path = git(&["rev-parse", "--git-path", &path]);
            if let Some(path) = path.filter(|path| Path::new(path).exists()) {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    println!(
        "cargo:rustc-env=CHOTU_GIT_SHA={}",
        sha.as_deref().unwrap_or("unknown")
    );
}
