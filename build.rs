//! Stamp the binary with the commit it was built from and the build date,
//! so two builds of the same version can be told apart:
//! `nanopi 0.17.2 (49ab2ec-dirty, built 2026-11-28)`.

use std::process::Command;

/// What a source change means here: uncommitted edits to these make the
/// build `-dirty`. Edits to anything else (notes, local files) do not.
const SOURCE: [&str; 6] = ["src", "assets", "Cargo.toml", "Cargo.lock", "VERSION", "build.rs"];

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-c", "safe.directory=*", "-C", dir])
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `YYYY-MM-DD` (UTC) for seconds since the epoch.
fn civil_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let commit = git(&dir, &["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let mut status = vec!["status", "--porcelain", "--untracked-files=no", "--"];
    status.extend(SOURCE);
    let dirty = git(&dir, &status).is_some_and(|s| !s.is_empty());
    // Reproducible builds pin the date; otherwise it is today.
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    println!(
        "cargo:rustc-env=NANOPI_COMMIT={commit}{}",
        if dirty { "-dirty" } else { "" }
    );
    println!("cargo:rustc-env=NANOPI_BUILD_DATE={}", civil_date(secs));
    // Rerun when the source or the checked-out commit changes.
    for path in SOURCE {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in [".git/HEAD", ".git/index", ".git/refs/heads"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
