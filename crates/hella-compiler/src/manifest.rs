//! Project manifest (`hella.toml`) and lockfile (`hella.lock`) model for the
//! Hella package manager (git-URLs-only, no registry).
//!
//! The manifest key of each dependency is its **short import name**: the name
//! used in `import mylib` / `import mylib::sub` statements. It maps to a git
//! URL plus a version request; the lockfile pins the exact resolved tag/SHA.
//!
//! Both the legacy bare form and the section form parse:
//!
//! ```toml
//! name = "myapp"
//! version = "0.1.0"
//! ```
//!
//! ```toml
//! [package]
//! name = "myapp"
//! version = "0.1.0"
//!
//! [dependencies]
//! mylib = { git = "github.com/repo/lib", version = "1.2.3" }
//! tool = { git = "github.com/repo/tool", version = "0.4.0", package = "tool::cli" }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use thiserror::Error;

/// A single library dependency: a git source plus a version request.
///
/// `git` is host + path without scheme (`github.com/owner/repo`); a leading
/// `https://` (or `http://`) is accepted and stripped. `version` is a
/// semver request (`1.2.3`, `^1.2`, `*`) or `latest`. `package` optionally
/// selects a sub-path inside the repo when the Hella module root is not the
/// repo root (`::`-separated, e.g. `tool::cli`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub git: String,
    pub version: String,
    pub package: Option<String>,
}

/// Parsed project manifest: `[package]` identity plus `[dependencies]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub dependencies: BTreeMap<String, Dependency>,
}

/// A pinned dependency in `hella.lock`: the exact tag (or `HEAD`) and
/// commit SHA the version request resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedDependency {
    pub name: String,
    pub git: String,
    pub version: String,
    pub rev: String,
    pub package: Option<String>,
}

/// Parsed lockfile: the exact pins for a project's dependency closure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lockfile {
    pub packages: Vec<LockedDependency>,
}

/// Manifest / lockfile errors (spans are not tracked: these files are small
/// and errors name the offending key).
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("{0}")]
    Invalid(String),
    #[error("failed to read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Short import names double as TOML keys and Hella module segments:
/// `[_a-zA-Z][_a-zA-Z0-9]*`. `std` is reserved for the standard library.
pub fn is_valid_dep_name(name: &str) -> bool {
    if name.is_empty() || name == "std" {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Normalize a `git` source: trim whitespace, strip one leading
/// `https://` / `http://` / `file://`, strip trailing slashes and an
/// optional `.git` suffix. Bare `owner/repo` gains a `github.com/` prefix.
/// Absolute local paths map under `local/` (`/tmp/foo` → `local/tmp/foo`)
/// so they stay rooted inside the `pkg/` cache. Empty results are rejected
/// by the caller.
pub fn normalize_git_source(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Already normalized (e.g. re-parsed from a manifest): never prefix twice.
    if s.starts_with("local/") {
        while s.ends_with('/') {
            s.pop();
        }
        return s;
    }
    for prefix in ["https://", "http://", "file://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    while s.ends_with('/') {
        s.pop();
    }
    if let Some(rest) = s.strip_suffix(".git") {
        s = rest.to_string();
    }
    if s.starts_with('/') {
        return format!("local{s}");
    }
    if !s.contains('/') {
        return s;
    }
    // `owner/repo` (no dot in the first segment) defaults to GitHub.
    let first = s.split('/').next().unwrap_or("");
    if !first.contains('.') && !first.contains(':') {
        return format!("github.com/{s}");
    }
    s
}

/// Clone URL for a normalized `git` source: `local/...` maps back to the
/// absolute path, everything else gains `https://`.
pub fn clone_url(git: &str) -> String {
    match git.strip_prefix("local/") {
        Some(path) => format!("/{path}"),
        None => format!("https://{git}"),
    }
}

/// A version request is `latest`, `*`, or anything `semver::VersionReq`
/// accepts (`1.2.3`, `^1.2`, `>=1.0, <2`, …).
pub fn is_valid_version_req(req: &str) -> bool {
    let req = req.trim();
    req == "latest" || req == "*" || semver::VersionReq::parse(req).is_ok()
}

/// An optional `package` sub-path is one or more `::`-separated short names.
pub fn is_valid_package_path(path: &str) -> bool {
    !path.is_empty()
        && path.split("::").all(|seg| is_valid_dep_name(seg))
}

fn value_to_string(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn parse_dependency(name: &str, v: &toml::Value) -> Result<Dependency, ManifestError> {
    if !is_valid_dep_name(name) {
        return Err(ManifestError::Invalid(format!(
            "invalid dependency name `{name}` (expected [_a-zA-Z][_a-zA-Z0-9]*, `std` is reserved)"
        )));
    }
    let table = match v {
        toml::Value::Table(t) => t,
        toml::Value::String(git) => {
            // Shorthand: `mylib = "github.com/repo/lib"` means latest.
            let git = normalize_git_source(git);
            if git.is_empty() {
                return Err(ManifestError::Invalid(format!(
                    "dependency `{name}` has an empty `git` source"
                )));
            }
            return Ok(Dependency {
                git,
                version: "latest".to_string(),
                package: None,
            });
        }
        _ => {
            return Err(ManifestError::Invalid(format!(
                "dependency `{name}` must be a table like `{{ git = \"...\", version = \"...\" }}`"
            )));
        }
    };
    let git_raw = table
        .get("git")
        .and_then(value_to_string)
        .ok_or_else(|| {
            ManifestError::Invalid(format!(
                "dependency `{name}` must define `git` (e.g. `github.com/owner/repo`)"
            ))
        })?;
    let git = normalize_git_source(&git_raw);
    if git.is_empty() {
        return Err(ManifestError::Invalid(format!(
            "dependency `{name}` has an empty `git` source"
        )));
    }
    let version = table
        .get("version")
        .and_then(value_to_string)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "latest".to_string());
    if !is_valid_version_req(&version) {
        return Err(ManifestError::Invalid(format!(
            "dependency `{name}` has invalid version `{version}` (expected semver like `1.2.3` or `latest`)"
        )));
    }
    let package = match table.get("package") {
        None => None,
        Some(v) => {
            let p = value_to_string(v).ok_or_else(|| {
                ManifestError::Invalid(format!(
                    "dependency `{name}` has a non-string `package` sub-path"
                ))
            })?;
            if !is_valid_package_path(&p) {
                return Err(ManifestError::Invalid(format!(
                    "dependency `{name}` has invalid `package` sub-path `{p}` (expected `mod` or `a::b`)"
                )));
            }
            Some(p)
        }
    };
    for key in table.keys() {
        if !matches!(key.as_str(), "git" | "version" | "package") {
            return Err(ManifestError::Invalid(format!(
                "dependency `{name}` has unknown key `{key}` (expected `git`, `version`, `package`)"
            )));
        }
    }
    Ok(Dependency {
        git,
        version,
        package,
    })
}

/// Parse manifest text. Accepts the legacy bare form (`name = …` at top
/// level) and the `[package]` section form; unknown top-level keys and
/// unknown `[package]` keys are ignored for forward compatibility.
pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError> {
    // NOTE (toml 1.x): `str::parse::<toml::Value>()` parses a TOML *value*,
    // not a document, so `name = "x"` fails with "expected nothing". A
    // document must be parsed as `toml::Table`.
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        ManifestError::Invalid(format!("malformed hella.toml: {e}"))
    })?;

    // Identity: `[package]` section wins, else legacy top-level keys.
    let (mut name, mut version) = (None, None);
    if let Some(toml::Value::Table(pkg)) = table.get("package") {
        for (key, v) in pkg {
            match key.as_str() {
                "name" => name = value_to_string(v),
                "version" => version = value_to_string(v),
                _ => {}
            }
        }
    }
    if name.is_none() {
        name = table.get("name").and_then(value_to_string);
    }
    if version.is_none() {
        version = table.get("version").and_then(value_to_string);
    }
    let (Some(name), Some(version)) = (name, version) else {
        return Err(ManifestError::Invalid(
            "hella.toml must define `name` and `version`".to_string(),
        ));
    };
    if name.trim().is_empty() {
        return Err(ManifestError::Invalid(
            "hella.toml `name` must not be empty".to_string(),
        ));
    }
    if version.trim().is_empty() {
        return Err(ManifestError::Invalid(
            "hella.toml `version` must not be empty".to_string(),
        ));
    }

    let mut dependencies = BTreeMap::new();
    if let Some(deps) = table.get("dependencies") {
        let deps_table = deps.as_table().ok_or_else(|| {
            ManifestError::Invalid(
                "hella.toml `[dependencies]` must be a table".to_string(),
            )
        })?;
        for (dep_name, dep_value) in deps_table {
            dependencies.insert(
                dep_name.clone(),
                parse_dependency(dep_name, dep_value)?,
            );
        }
    }
    Ok(Manifest {
        name,
        version,
        dependencies,
    })
}

/// Read and parse `root/hella.toml`. `Ok(None)` when the file is absent.
pub fn read_manifest_file(root: &Path) -> Result<Option<Manifest>, ManifestError> {
    let path = root.join("hella.toml");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| ManifestError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_manifest(&text).map(Some)
}

/// Serialize a manifest (section form) for writing back after `add`/`remove`.
/// Dependencies sort by name (`BTreeMap` order) for stable diffs.
pub fn serialize_manifest(manifest: &Manifest) -> String {
    let mut out = String::new();
    out.push_str("[package]\n");
    out.push_str(&format!("name = \"{}\"\n", manifest.name));
    out.push_str(&format!("version = \"{}\"\n", manifest.version));
    if !manifest.dependencies.is_empty() {
        out.push_str("\n[dependencies]\n");
        for (name, dep) in &manifest.dependencies {
            match &dep.package {
                Some(pkg) => out.push_str(&format!(
                    "{name} = {{ git = \"{}\", version = \"{}\", package = \"{pkg}\" }}\n",
                    dep.git, dep.version,
                )),
                None => out.push_str(&format!(
                    "{name} = {{ git = \"{}\", version = \"{}\" }}\n",
                    dep.git, dep.version,
                )),
            }
        }
    }
    out
}

/// Parse `hella.lock` text: a `[[package]]` list of exact pins.
pub fn parse_lockfile(text: &str) -> Result<Lockfile, ManifestError> {
    if text.trim().is_empty() {
        return Ok(Lockfile::default());
    }
    let value: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        ManifestError::Invalid(format!("malformed hella.lock: {e}"))
    })?;
    let table = &value;
    let mut packages = Vec::new();
    match table.get("package") {
        None => {}
        Some(toml::Value::Array(entries)) => {
            for entry in entries {
                let t = entry.as_table().ok_or_else(|| {
                    ManifestError::Invalid(
                        "malformed hella.lock: `package` entries must be tables".to_string(),
                    )
                })?;
                let get = |key: &str| {
                    t.get(key).and_then(value_to_string).ok_or_else(|| {
                        ManifestError::Invalid(format!(
                            "malformed hella.lock: package entry missing `{key}`"
                        ))
                    })
                };
                let name = get("name")?;
                if !is_valid_dep_name(&name) {
                    return Err(ManifestError::Invalid(format!(
                        "malformed hella.lock: invalid package name `{name}`"
                    )));
                }
                packages.push(LockedDependency {
                    name,
                    git: get("git")?,
                    version: get("version")?,
                    rev: get("rev")?,
                    package: t.get("package").and_then(value_to_string),
                });
            }
        }
        Some(_) => {
            return Err(ManifestError::Invalid(
                "malformed hella.lock: `package` must be a list".to_string(),
            ));
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Lockfile { packages })
}

/// Read and parse `root/hella.lock`. `Ok(None)` when the file is absent.
pub fn read_lockfile(root: &Path) -> Result<Option<Lockfile>, ManifestError> {
    let path = root.join("hella.lock");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| ManifestError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_lockfile(&text).map(Some)
}

/// Serialize a lockfile. Entries sort by name for stable diffs.
pub fn serialize_lockfile(lock: &Lockfile) -> String {
    let mut out = String::new();
    out.push_str("# Generated by `hella add` — do not edit by hand.\n");
    let mut packages = lock.packages.clone();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    for p in &packages {
        out.push_str("\n[[package]]\n");
        out.push_str(&format!("name = \"{}\"\n", p.name));
        out.push_str(&format!("git = \"{}\"\n", p.git));
        out.push_str(&format!("version = \"{}\"\n", p.version));
        out.push_str(&format!("rev = \"{}\"\n", p.rev));
        if let Some(pkg) = &p.package {
            out.push_str(&format!("package = \"{pkg}\"\n"));
        }
    }
    out
}

/// Derive the default short import name from a normalized `git` source:
/// the last path segment, lowercased, with non-alphanumerics mapped to
/// `_` (`github.com/Owner/My-Lib` → `my_lib`). A leading digit or the
/// reserved `std` gains a leading underscore.
pub fn derive_dep_name(git: &str) -> String {
    let last = git
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(git);
    let mut name: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Collapse runs and trim edges so `-foo-` / `foo.bar` read cleanly.
    let mut collapsed = String::with_capacity(name.len());
    let mut prev_underscore = false;
    for c in name.chars() {
        if c == '_' {
            if !prev_underscore {
                collapsed.push(c);
            }
            prev_underscore = true;
        } else {
            collapsed.push(c);
            prev_underscore = false;
        }
    }
    name = collapsed.trim_matches('_').to_string();
    if name.is_empty() {
        name = "dep".to_string();
    }
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) || name == "std" {
        name = format!("_{name}");
    }
    if !is_valid_dep_name(&name) {
        // Paranoia fallback: keep only valid chars (always succeeds —
        // worst case the `dep` default above).
        let filtered: String = name
            .chars()
            .filter(|c| *c == '_' || c.is_ascii_alphanumeric())
            .collect();
        name = if is_valid_dep_name(&filtered) {
            filtered
        } else {
            "dep".to_string()
        };
    }
    name
}

/// Marker file written into every fetched `pkg/` slot (`.hella-slot`):
/// the exact source the slot was populated from, so `remove`/`clean` can
/// garbage-collect unreferenced slots regardless of directory depth.
pub fn slot_marker_name() -> &'static str {
    ".hella-slot"
}

/// Serialize a slot marker.
pub fn serialize_slot_marker(git: &str, version: &str, rev: &str) -> String {
    format!("git = \"{git}\"\nversion = \"{version}\"\nrev = \"{rev}\"\n")
}

/// Read a slot marker. `Ok(None)` when the directory holds no marker
/// (not a managed slot); `Err` on malformed content.
pub fn read_slot_marker(slot: &Path) -> Result<Option<(String, String, String)>, ManifestError> {
    let path = slot.join(slot_marker_name());
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| ManifestError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let value: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        ManifestError::Invalid(format!(
            "malformed slot marker {}: {e}",
            path.display()
        ))
    })?;
    let table = &value;
    let get = |key: &str| {
        table
            .get(key)
            .and_then(value_to_string)
            .ok_or_else(|| {
                ManifestError::Invalid(format!(
                    "malformed slot marker {}: missing `{key}`",
                    path.display()
                ))
            })
    };
    Ok(Some((get("git")?, get("version")?, get("rev")?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_bare_manifest() {
        let m = parse_manifest("name = \"myapp\"\nversion = \"0.1.0\"\n").unwrap();
        assert_eq!(m.name, "myapp");
        assert_eq!(m.version, "0.1.0");
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn parses_section_manifest_with_deps() {
        let m = parse_manifest(
            "[package]\nname = \"myapp\"\nversion = \"0.1.0\"\n\
             \n[dependencies]\n\
             mylib = { git = \"https://github.com/repo/lib.git\", version = \"1.2.3\" }\n\
             tool = { git = \"github.com/repo/tool\", version = \"^0.4\", package = \"tool::cli\" }\n",
        )
        .unwrap();
        assert_eq!(m.name, "myapp");
        assert_eq!(
            m.dependencies["mylib"],
            Dependency {
                git: "github.com/repo/lib".to_string(),
                version: "1.2.3".to_string(),
                package: None,
            }
        );
        assert_eq!(
            m.dependencies["tool"],
            Dependency {
                git: "github.com/repo/tool".to_string(),
                version: "^0.4".to_string(),
                package: Some("tool::cli".to_string()),
            }
        );
    }

    #[test]
    fn parses_shorthand_string_dep_as_latest() {
        let m = parse_manifest(
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\
             [dependencies]\nmylib = \"github.com/repo/lib\"\n",
        )
        .unwrap();
        assert_eq!(m.dependencies["mylib"].version, "latest");
    }

    #[test]
    fn rejects_missing_identity_and_bad_deps() {
        assert!(parse_manifest("name = \"a\"\n").is_err());
        assert!(parse_manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n[dependencies]\nstd = { git = \"x\", version = \"1\" }\n"
        )
        .is_err());
        assert!(parse_manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n[dependencies]\n1bad = { git = \"x\", version = \"1\" }\n"
        )
        .is_err());
        assert!(parse_manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n[dependencies]\nok = { git = \"\", version = \"1\" }\n"
        )
        .is_err());
        assert!(parse_manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n[dependencies]\nok = { git = \"github.com/a/b\", version = \"nope\" }\n"
        )
        .is_err());
        assert!(parse_manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n[dependencies]\nok = { git = \"github.com/a/b\", version = \"1\", package = \"a::::b\" }\n"
        )
        .is_err());
        assert!(parse_manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n[dependencies]\nok = { git = \"github.com/a/b\", version = \"1\", extra = \"z\" }\n"
        )
        .is_err());
    }

    #[test]
    fn lockfile_round_trip() {
        let lock = Lockfile {
            packages: vec![LockedDependency {
                name: "mylib".to_string(),
                git: "github.com/repo/lib".to_string(),
                version: "1.2.3".to_string(),
                rev: "abc123".to_string(),
                package: None,
            }],
        };
        let text = serialize_lockfile(&lock);
        assert_eq!(parse_lockfile(&text).unwrap(), lock);
        assert!(parse_lockfile("").unwrap().packages.is_empty());
    }

    #[test]
    fn manifest_serialization_keeps_identity_and_sorts_deps() {
        let mut dependencies = BTreeMap::new();
        dependencies.insert(
            "zlib".to_string(),
            Dependency {
                git: "github.com/a/z".to_string(),
                version: "1.0.0".to_string(),
                package: None,
            },
        );
        dependencies.insert(
            "alib".to_string(),
            Dependency {
                git: "github.com/a/b".to_string(),
                version: "2.0.0".to_string(),
                package: Some("a::b".to_string()),
            },
        );
        let text = serialize_manifest(&Manifest {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            dependencies,
        });
        // Section form that re-parses to the same manifest.
        let reparsed = parse_manifest(&text).unwrap();
        assert_eq!(reparsed.name, "app");
        assert_eq!(reparsed.dependencies.len(), 2);
        assert!(text.find("alib").unwrap() < text.find("zlib").unwrap());
    }

    #[test]
    fn normalizes_git_sources() {
        assert_eq!(
            normalize_git_source("https://github.com/a/b.git"),
            "github.com/a/b"
        );
        assert_eq!(
            normalize_git_source("  github.com/a/b/ "),
            "github.com/a/b"
        );
        assert_eq!(normalize_git_source("owner/repo"), "github.com/owner/repo");
        assert_eq!(
            normalize_git_source("file:///tmp/foo"),
            "local/tmp/foo"
        );
        assert_eq!(normalize_git_source("/tmp/foo"), "local/tmp/foo");
        assert_eq!(
            normalize_git_source("local/tmp/foo"),
            "local/tmp/foo",
            "normalization must be idempotent"
        );
        assert_eq!(
            clone_url("github.com/a/b"),
            "https://github.com/a/b"
        );
        assert_eq!(clone_url("local/tmp/foo"), "/tmp/foo");
    }

    #[test]
    fn derives_dep_names() {
        assert_eq!(derive_dep_name("github.com/Owner/My-Lib"), "my_lib");
        assert_eq!(derive_dep_name("github.com/o/foo.bar"), "foo_bar");
        assert_eq!(derive_dep_name("github.com/o/2fast"), "_2fast");
        assert_eq!(derive_dep_name("github.com/o/std"), "_std");
        assert_eq!(derive_dep_name("local/tmp/my-lib"), "my_lib");
        for n in ["my_lib", "foo_bar", "_2fast", "_std"] {
            assert!(is_valid_dep_name(n), "{n}");
        }
    }
}
