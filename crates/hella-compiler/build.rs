//! Sets `HELLA_HOST_TRIPLE` (the compiling toolchain's host triple) so
//! `@cfg(target = "...")` / `@cfg(os = "...")` can evaluate against it.
//!
//! `rustc -vV` prints `host: <triple>`; when rustc is unreachable (should
//! not happen — Cargo runs this with rustc on PATH), the fallback is a
//! conservative `unknown-unknown-unknown` that makes `target = "..."`
//! conditions false rather than guessing.

fn main() {
    let triple = std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-vV")
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8(out.stdout).ok().and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("host: "))
                    .map(|t| t.trim().to_string())
            })
        })
        .unwrap_or_else(|| "unknown-unknown-unknown".to_string());
    println!("cargo:rustc-env=HELLA_HOST_TRIPLE={triple}");
}
