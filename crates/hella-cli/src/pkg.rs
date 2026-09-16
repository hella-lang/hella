//! Third-party package management: `hella add` / `hella remove`
//! (git-URLs-only, no registry).
//!
//! All entry points take explicit `pkg_root` / `cache_root` paths (never
//! ambient `$HOME`) so they are unit-testable with fixture directories.
//! The CLI wires the real `~/.hella` locations in `main.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use hella_compiler::manifest::{
    self, Dependency, LockedDependency, Lockfile, Manifest,
};
use hella_compiler::modules;

/// A parsed `hella add` argument: normalized git source, optional
/// requested revision, and the short import name to register.
pub struct AddRequest {
    pub git: String,
    pub rev: Option<String>,
    pub name: String,
}

/// Parse `hella add <spec>`: `<source>[@<rev>]` where `<source>` is
/// `github.com/owner/repo`, `owner/repo` (GitHub default),
/// `https://…[.git]`, `file://…`, or an absolute local path.
///
/// An existing local path wins verbatim (so directories containing `@`
/// work); otherwise the spec splits at the last `@`.
pub fn parse_add_spec(spec: &str, name_override: Option<&str>) -> miette::Result<AddRequest> {
    let raw = spec.trim();
    if raw.is_empty() {
        return Err(miette::miette!("empty package spec (expected `<source>[@<rev>]`)"));
    }
    let (source, rev) = if Path::new(raw).exists() {
        (raw.to_string(), None)
    } else if let Some(at) = raw.rfind('@') {
        let (s, r) = raw.split_at(at);
        let r = r[1..].trim();
        if r.is_empty() {
            return Err(miette::miette!(
                "empty revision in `{spec}` (expected `<source>[@<rev>]`)"
            ));
        }
        if r.chars().any(|c| c.is_whitespace()) {
            return Err(miette::miette!(
                "invalid revision `{r}` in `{spec}` (must not contain whitespace)"
            ));
        }
        (s.trim().to_string(), Some(r.to_string()))
    } else {
        (raw.to_string(), None)
    };
    let git = manifest::normalize_git_source(&source);
    if git.is_empty() {
        return Err(miette::miette!("empty git source in `{spec}`"));
    }
    let name = match name_override {
        Some(n) => n.trim().to_string(),
        None => manifest::derive_dep_name(&git),
    };
    if !manifest::is_valid_dep_name(&name) {
        return Err(miette::miette!(
            "invalid dependency name `{name}` (expected [_a-zA-Z][_a-zA-Z0-9]*, `std` is reserved) — pass `--name <name>`"
        ));
    }
    Ok(AddRequest { git, rev, name })
}

/// Serialize one `[dependencies]` entry (short inline-table form).
fn serialize_dep_value(dep: &Dependency) -> String {
    match &dep.package {
        Some(pkg) => format!(
            "{{ git = \"{}\", version = \"{}\", package = \"{pkg}\" }}",
            dep.git, dep.version,
        ),
        None => format!("{{ git = \"{}\", version = \"{}\" }}", dep.git, dep.version),
    }
}

/// Surgically insert (or replace) a dependency line in manifest text,
/// preserving comments and formatting. Falls back to a full rewrite when
/// the edited text no longer parses.
pub fn insert_dep_line(text: &str, name: &str, dep: &Dependency) -> String {
    let entry = format!("{name} = {}", serialize_dep_value(dep));
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    // Locate the `[dependencies]` header (exact section match).
    let mut header: Option<usize> = None;
    let mut in_deps = false;
    let mut existing: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            if in_deps && header.is_none() {
                header = Some(i);
            }
            continue;
        }
        if in_deps && dep_line_name(t) == Some(name) {
            existing = Some(i);
        }
    }
    if let Some(i) = existing {
        lines[i] = entry;
    } else if let Some(h) = header {
        lines.insert(h + 1, entry);
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("[dependencies]".to_string());
        lines.push(entry);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    // Safety net: a mangled edit must never corrupt the manifest.
    if manifest::parse_manifest(&out).is_err() {
        let mut m = manifest::parse_manifest(text).unwrap_or(Manifest {
            name: String::new(),
            version: String::new(),
            dependencies: BTreeMap::new(),
        });
        m.dependencies.insert(name.to_string(), dep.clone());
        out = manifest::serialize_manifest(&m);
    }
    out
}

/// The dependency name of a `name = …` line inside `[dependencies]`,
/// or `None` for any other line.
fn dep_line_name(trimmed: &str) -> Option<&str> {
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    let key = key.trim().trim_matches(['"', '\'']);
    if manifest::is_valid_dep_name(key) {
        Some(key)
    } else {
        None
    }
}

/// Surgically remove a dependency line, preserving the rest of the file.
pub fn remove_dep_line(text: &str, name: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_deps = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            lines.push(line.to_string());
            continue;
        }
        if in_deps && dep_line_name(t) == Some(name) {
            continue;
        }
        lines.push(line.to_string());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// git plumbing (via the `git` CLI: no libgit2 dependency for v1)
// ---------------------------------------------------------------------------

/// The `git` CLI must exist; Hella never vendors git.
fn ensure_git() -> miette::Result<()> {
    let ok = Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(miette::miette!(
            "the `git` CLI is required for `hella add` (install git and retry)"
        ))
    }
}

/// Run `git` with ambient repo config neutralized (tests must not depend
/// on the user's global gitconfig).
fn git(args: &[&str], cwd: Option<&Path>) -> miette::Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "user.email=hella@test", "-c", "user.name=hella"])
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args);
    cmd.output().map_err(|e| {
        miette::miette!("failed to run `git {}`: {e}", args.join(" "))
    })
}

fn git_text(args: &[&str], cwd: Option<&Path>, what: &str) -> miette::Result<String> {
    let out = git(args, cwd)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(miette::miette!(
            "{what} failed (`git {}`){}: {err}",
            args.join(" "),
            if err.is_empty() { "" } else { ": " },
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// A tag name and the commit it points at (peeled `^{}` SHA preferred).
struct TagRef {
    name: String,
    sha: String,
}

struct RemoteRefs {
    tags: Vec<TagRef>,
    head: Option<String>,
}

/// All refs of a remote without cloning (`git ls-remote`).
fn ls_remote(url: &str) -> miette::Result<RemoteRefs> {
    let out = git_text(
        &["ls-remote", url],
        None,
        &format!("cannot reach `{url}`"),
    )
    .map_err(|e| {
        miette::miette!(
            "{e} (check the URL and your network / credentials)"
        )
    })?;
    // tag name -> (sha, peeled sha): annotated tags list both.
    let mut tags: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    let mut head = None;
    for line in out.lines() {
        let (sha, reference) = match line.split_once(char::is_whitespace) {
            Some((s, r)) => (s.trim().to_string(), r.trim().to_string()),
            None => continue,
        };
        if reference == "HEAD" {
            head = Some(sha);
        } else if let Some(tag) = reference.strip_prefix("refs/tags/") {
            if let Some(name) = tag.strip_suffix("^{}") {
                // Assignment RHS evaluates before the place expression,
                // so bind the entry first (else `sha` moves too early).
                let entry = tags
                    .entry(name.to_string())
                    .or_insert((sha.clone(), None));
                entry.1 = Some(sha);
            } else {
                tags.entry(tag.to_string()).or_insert((sha, None));
            }
        }
    }
    Ok(RemoteRefs {
        tags: tags
            .into_iter()
            .map(|(name, (sha, peeled))| TagRef {
                name,
                sha: peeled.unwrap_or(sha),
            })
            .collect(),
        head,
    })
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
}

/// `(semver, tag, sha)` for every tag that parses as semver.
fn semver_tags(tags: &[TagRef]) -> Vec<(semver::Version, &TagRef)> {
    let mut out: Vec<(semver::Version, &TagRef)> = tags
        .iter()
        .filter_map(|t| {
            semver::Version::parse(strip_v(&t.name))
                .ok()
                .map(|v| (v, t))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A resolved revision: exact `version` label plus full commit `rev`.
/// `tag` is the tag to shallow-clone when the resolution came from one.
struct Resolved {
    version: String,
    rev: String,
    tag: Option<String>,
    /// True when `rev` is a branch name needing `rev-parse` after clone.
    needs_rev_parse: bool,
}

/// Resolve a version request against a remote: exact tag, semver range
/// over tags, branch name, or full commit SHA — else the newest tag, else
/// HEAD. Never touches the network beyond `ls-remote`.
fn resolve_rev(url: &str, req: Option<&str>) -> miette::Result<Resolved> {
    let refs = ls_remote(url)?;
    let req = req.map(str::trim).filter(|r| !r.is_empty());
    let tagged = semver_tags(&refs.tags);
    // Pool: stable releases first; prereleases only when nothing stable exists.
    let stable: Vec<(semver::Version, &TagRef)> = tagged
        .iter()
        .filter(|(v, _)| v.pre.is_empty())
        .cloned()
        .collect();
    let pool = if stable.is_empty() { tagged } else { stable };

    // No request (or `latest`/`*`): newest tag, else HEAD.
    if req.is_none_or(|r| r == "latest" || r == "*") {
        if let Some((v, t)) = pool.last() {
            return Ok(Resolved {
                version: v.to_string(),
                rev: t.sha.clone(),
                tag: Some(t.name.clone()),
                needs_rev_parse: false,
            });
        }
        let Some(head) = refs.head else {
            return Err(miette::miette!(
                "`{url}` has no tags and no HEAD (is the repository empty?)"
            ));
        };
        return Ok(Resolved {
            version: format!("HEAD-{}", &head[..head.len().min(12)]),
            rev: head,
            tag: None,
            needs_rev_parse: false,
        });
    }
    let req = req.unwrap_or("latest");

    // Exact tag (`1.2.3` also matches `v1.2.3`).
    if let Some(t) = refs.tags.iter().find(|t| {
        t.name == req || strip_v(&t.name) == strip_v(req)
    }) {
        let version = semver::Version::parse(strip_v(&t.name))
            .map(|v| v.to_string())
            .unwrap_or_else(|_| t.name.clone());
        return Ok(Resolved {
            version,
            rev: t.sha.clone(),
            tag: Some(t.name.clone()),
            needs_rev_parse: false,
        });
    }
    // Semver range over tags.
    if let Ok(range) = semver::VersionReq::parse(strip_v(req)) {
        if let Some((v, t)) = pool.iter().rev().find(|(v, _)| range.matches(v)) {
            return Ok(Resolved {
                version: v.to_string(),
                rev: t.sha.clone(),
                tag: Some(t.name.clone()),
                needs_rev_parse: false,
            });
        }
        return Err(miette::miette!(
            "no tag of `{url}` satisfies `{req}`"
        ));
    }
    // Branch name or commit SHA: verify after clone.
    Ok(Resolved {
        version: format!("rev-{}", &req[..req.len().min(12)]),
        rev: req.to_string(),
        tag: None,
        needs_rev_parse: true,
    })
}

/// Clone a resolved revision into `dest` (must not exist yet).
fn clone_resolved(url: &str, resolved: &Resolved, dest: &Path) -> miette::Result<String> {
    if let Some(tag) = &resolved.tag {
        git_text(
            &[
                "clone",
                "--depth",
                "1",
                "--branch",
                tag,
                "--",
                url,
                &dest.display().to_string(),
            ],
            None,
            &format!("cannot clone `{url}`"),
        )?;
    } else {
        git_text(
            &["clone", "--", url, &dest.display().to_string()],
            None,
            &format!("cannot clone `{url}`"),
        )?;
        git_text(
            &["checkout", "--detach", "--quiet", &resolved.rev],
            Some(dest),
            &format!("cannot check out `{}`", resolved.rev),
        )?;
    }
    let sha = git_text(
        &["rev-parse", "HEAD"],
        Some(dest),
        "cannot read cloned HEAD",
    )?;
    let sha = sha.trim().to_string();
    if !resolved.needs_rev_parse && sha != resolved.rev {
        return Err(miette::miette!(
            "clone of `{url}` gave {sha} but `{}` was expected",
            resolved.rev,
        ));
    }
    Ok(sha)
}

fn copy_dir_without_git(src: &Path, dst: &Path) -> miette::Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        miette::miette!("failed to create {}: {e}", dst.display())
    })?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| miette::miette!("failed to read {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| {
            miette::miette!("failed to read {}: {e}", src.display())
        })?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let ty = entry.file_type().map_err(|e| {
            miette::miette!("failed to inspect {}: {e}", from.display())
        })?;
        if ty.is_dir() {
            copy_dir_without_git(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                miette::miette!(
                    "failed to copy {} → {}: {e}",
                    from.display(),
                    to.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Fetch one dependency into its immutable `pkg/` slot (no-op when the
/// slot already carries a matching marker). Returns the slot path.
fn fetch_slot(
    pkg_root: &Path,
    cache_root: &Path,
    git: &str,
    version: &str,
    resolved: &Resolved,
) -> miette::Result<PathBuf> {
    let slot = modules::pkg_slot_dir(pkg_root, git, version);
    if let Some((mg, _, mrev)) = manifest::read_slot_marker(&slot)
        .map_err(|e| miette::miette!("{e}"))?
    {
        if mg == git && mrev == resolved.rev {
            return Ok(slot);
        }
        std::fs::remove_dir_all(&slot).map_err(|e| {
            miette::miette!("failed to clear stale slot {}: {e}", slot.display())
        })?;
    } else if slot.exists() {
        // Unmanaged files where a slot should be: never merge, start clean.
        std::fs::remove_dir_all(&slot).map_err(|e| {
            miette::miette!("failed to clear {}: {e}", slot.display())
        })?;
    }
    let scratch = cache_root.join(format!(
        "hella-fetch-{}-{}",
        std::process::id(),
        scratch_nonce()
    ));
    let result = (|| -> miette::Result<String> {
        let url = manifest::clone_url(git);
        let sha = clone_resolved(&url, resolved, &scratch)?;
        copy_dir_without_git(&scratch, &slot)?;
        std::fs::write(
            slot.join(manifest::slot_marker_name()),
            manifest::serialize_slot_marker(git, version, &sha),
        )
        .map_err(|e| {
            miette::miette!("failed to write slot marker: {e}")
        })?;
        Ok(sha)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result?;
    Ok(slot)
}

fn scratch_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    // Nanos + counter: unique across parallel `add`s in one process.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos.wrapping_add(N.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// dependency closure
// ---------------------------------------------------------------------------

struct WorkItem {
    name: String,
    git: String,
    req: String,
    package: Option<String>,
}

/// Resolve + fetch a dependency closure (direct deps first, then
/// transitive). First pin wins per name; a conflicting `git` source or an
/// incompatible version for the same name is a loud error, never a silent
/// downgrade. Cycles cut by recording each pin before recursing.
fn resolve_and_fetch_closure(
    pkg_root: &Path,
    cache_root: &Path,
    direct: &BTreeMap<String, Dependency>,
    pre_pinned: &[LockedDependency],
) -> miette::Result<Vec<LockedDependency>> {
    let mut pinned: BTreeMap<String, LockedDependency> = BTreeMap::new();
    for p in pre_pinned {
        pinned.insert(p.name.clone(), p.clone());
    }
    let mut queue: Vec<WorkItem> = direct
        .iter()
        .map(|(name, d)| WorkItem {
            name: name.clone(),
            git: d.git.clone(),
            req: d.version.clone(),
            package: d.package.clone(),
        })
        .collect();

    while let Some(item) = queue.pop() {
        if let Some(existing) = pinned.get(&item.name) {
            if existing.git != item.git {
                return Err(miette::miette!(
                    "dependency name conflict: `{}` comes from both `{}` and `{}` \
                     (rename one with `hella add <url> --name <name>`)",
                    item.name,
                    existing.git,
                    item.git,
                ));
            }
            // Same source: the existing pin must satisfy the new request
            // when both sides are comparable semver.
            if let (Ok(range), Ok(ver)) = (
                semver::VersionReq::parse(&item.req),
                semver::Version::parse(&existing.version),
            ) {
                if !range.matches(&ver) {
                    return Err(miette::miette!(
                        "conflicting requirements for `{}`: `{}` does not satisfy `{}` \
                         (pin an explicit version in hella.toml)",
                        item.name,
                        existing.version,
                        item.req,
                    ));
                }
            }
            continue;
        }
        let url = manifest::clone_url(&item.git);
        let resolved = resolve_rev(&url, Some(&item.req))?;
        fetch_slot(pkg_root, cache_root, &item.git, &resolved.version, &resolved)?;
        let pin = LockedDependency {
            name: item.name.clone(),
            git: item.git.clone(),
            version: resolved.version.clone(),
            rev: resolved.rev.clone(),
            package: item.package.clone(),
        };
        // Record before recursing: cuts A ↔ B cycles.
        pinned.insert(item.name.clone(), pin.clone());
        let slot = modules::pkg_slot_dir(pkg_root, &item.git, &resolved.version);
        if let Ok(Some(child_manifest)) = manifest::read_manifest_file(&slot) {
            for (name, dep) in &child_manifest.dependencies {
                queue.push(WorkItem {
                    name: name.clone(),
                    git: dep.git.clone(),
                    req: dep.version.clone(),
                    package: dep.package.clone(),
                });
            }
        }
    }
    Ok(pinned.into_values().collect())
}

// ---------------------------------------------------------------------------
// add / remove / gc
// ---------------------------------------------------------------------------

fn read_manifest_text(root: &Path) -> miette::Result<String> {
    std::fs::read_to_string(root.join("hella.toml")).map_err(|e| {
        miette::miette!("failed to read hella.toml: {e}")
    })
}

/// `hella add <spec> [--name <name>]`: fetch the revision, record it in
/// `hella.toml`, and pin the closure in `hella.lock`.
pub fn run_add(
    root: &Path,
    pkg_root: &Path,
    cache_root: &Path,
    spec: &str,
    name_override: Option<&str>,
) -> miette::Result<()> {
    ensure_git();
    let req = parse_add_spec(spec, name_override)?;
    let text = read_manifest_text(root)?;
    let manifest = manifest::parse_manifest(&text)
        .map_err(|e| miette::miette!("invalid hella.toml: {e}"))?;
    if manifest.dependencies.contains_key(&req.name) {
        return Err(miette::miette!(
            "`{}` is already a dependency (run `hella remove {}` first to replace it)",
            req.name,
            req.name,
        ));
    }
    let old_lock = manifest::read_lockfile(root)
        .map_err(|e| miette::miette!("invalid hella.lock: {e}"))?
        .unwrap_or_default();

    let mut direct = BTreeMap::new();
    direct.insert(
        req.name.clone(),
        Dependency {
            git: req.git.clone(),
            version: req.rev.clone().unwrap_or_else(|| "latest".to_string()),
            package: None,
        },
    );
    let pins = resolve_and_fetch_closure(pkg_root, cache_root, &direct, &old_lock.packages)?;
    let Some(pin) = pins.iter().find(|p| p.name == req.name).cloned() else {
        return Err(miette::miette!("failed to resolve `{}`", req.name));
    };

    // Manifest keeps the *requested* revision; the lock holds the pin.
    let dep = Dependency {
        git: req.git.clone(),
        version: req.rev.clone().unwrap_or_else(|| pin.version.clone()),
        package: None,
    };
    std::fs::write(
        root.join("hella.toml"),
        insert_dep_line(&text, &req.name, &dep),
    )
    .map_err(|e| miette::miette!("failed to write hella.toml: {e}"))?;

    let mut merged: BTreeMap<String, LockedDependency> = BTreeMap::new();
    for p in old_lock.packages {
        merged.insert(p.name.clone(), p);
    }
    for p in pins {
        merged.insert(p.name.clone(), p);
    }
    std::fs::write(
        root.join("hella.lock"),
        manifest::serialize_lockfile(&Lockfile {
            packages: merged.into_values().collect(),
        }),
    )
    .map_err(|e| miette::miette!("failed to write hella.lock: {e}"))?;

    eprintln!(
        "Added {} {} ({}@{})",
        req.name, dep.version, req.git, pin.rev,
    );
    Ok(())
}

/// Reachable lock pins from `roots`, traversing fetched slot manifests.
/// Slots that are missing or unreadable keep their pins conservatively
/// (plus everything already kept) instead of risking a bad prune.
fn reachable_pins(
    roots: &BTreeMap<String, Dependency>,
    lock: &Lockfile,
    pkg_root: &Path,
) -> Vec<LockedDependency> {
    let by_name: BTreeMap<&str, &LockedDependency> = lock
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    let mut keep: BTreeMap<String, LockedDependency> = BTreeMap::new();
    let mut stack: Vec<String> = roots.keys().cloned().collect();
    while let Some(name) = stack.pop() {
        let Some(pin) = by_name.get(name.as_str()) else {
            continue;
        };
        if keep.contains_key(&pin.name) {
            continue;
        }
        keep.insert(pin.name.clone(), (*pin).clone());
        let slot = modules::pkg_slot_dir(pkg_root, &pin.git, &pin.version);
        match manifest::read_manifest_file(&slot) {
            Ok(Some(child)) => {
                stack.extend(child.dependencies.keys().cloned());
            }
            // No slot / no manifest / malformed: the subtree is unknown,
            // so keep every remaining pin rather than prune blindly.
            _ => {
                for p in &lock.packages {
                    keep.insert(p.name.clone(), p.clone());
                }
                break;
            }
        }
    }
    keep.into_values().collect()
}

/// Delete every `pkg/` slot whose marker is not referenced by `lock`.
/// Returns the number of slots removed.
pub fn gc_unreferenced_slots(pkg_root: &Path, lock: &Lockfile) -> miette::Result<usize> {
    let mut slots = Vec::new();
    collect_slots(pkg_root, &mut slots);
    let mut removed = 0usize;
    for slot in slots {
        let marker = manifest::read_slot_marker(&slot)
            .map_err(|e| miette::miette!("{e}"))?;
        let Some((git, version, _)) = marker else {
            continue;
        };
        let referenced = lock
            .packages
            .iter()
            .any(|p| p.git == git && p.version == version);
        if !referenced {
            std::fs::remove_dir_all(&slot).map_err(|e| {
                miette::miette!("failed to remove {}: {e}", slot.display())
            })?;
            prune_empty_parents(&slot, pkg_root);
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove empty parent directories up to (not including) `stop`.
/// `remove_dir` only succeeds on empty dirs, so this stops naturally.
fn prune_empty_parents(slot: &Path, stop: &Path) {
    let mut dir = slot.parent();
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn collect_slots(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if path.join(manifest::slot_marker_name()).is_file() {
            out.push(path);
        } else {
            collect_slots(&path, out);
        }
    }
}

/// `hella remove <name>`: drop the dependency from `hella.toml`, prune
/// now-unreachable pins from `hella.lock`, and delete orphaned slots.
pub fn run_remove(root: &Path, pkg_root: &Path, name: &str) -> miette::Result<()> {
    let text = read_manifest_text(root)?;
    let mut manifest = manifest::parse_manifest(&text)
        .map_err(|e| miette::miette!("invalid hella.toml: {e}"))?;
    if manifest.dependencies.remove(name).is_none() {
        return Err(miette::miette!(
            "no dependency named `{name}` (see `[dependencies]` in hella.toml)"
        ));
    }
    std::fs::write(root.join("hella.toml"), remove_dep_line(&text, name))
        .map_err(|e| miette::miette!("failed to write hella.toml: {e}"))?;

    let old_lock = manifest::read_lockfile(root)
        .map_err(|e| miette::miette!("invalid hella.lock: {e}"))?
        .unwrap_or_default();
    let pruned = reachable_pins(&manifest.dependencies, &old_lock, pkg_root);
    let pruned_lock = Lockfile {
        packages: pruned,
    };
    std::fs::write(
        root.join("hella.lock"),
        manifest::serialize_lockfile(&pruned_lock),
    )
    .map_err(|e| miette::miette!("failed to write hella.lock: {e}"))?;
    let removed = gc_unreferenced_slots(pkg_root, &pruned_lock)?;

    eprintln!("Removed {name} ({removed} cached slot(s) cleaned)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hella-pkg-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = git(args, Some(dir)).expect("git failed to run");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// A local git repo with `files` committed; `tags` are applied in order
    /// (each on its own commit after the first when there are several).
    fn make_repo(base: &Path, name: &str, files: &[(&str, &str)], tags: &[&str]) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, contents) in files {
            write_file(&dir.join(rel), contents);
        }
        git_ok(&dir, &["init"]);
        git_ok(&dir, &["add", "-A"]);
        git_ok(&dir, &["commit", "-m", "init", "--quiet"]);
        for (i, tag) in tags.iter().enumerate() {
            if i > 0 {
                write_file(&dir.join(format!("bump{i}.txt")), tag);
                git_ok(&dir, &["add", "-A"]);
                git_ok(&dir, &["commit", "-m", &format!("bump{i}"), "--quiet"]);
            }
            git_ok(&dir, &["tag", tag]);
        }
        dir
    }

    fn lib_manifest(deps: &[(&str, &str)]) -> String {
        let mut out = "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n".to_string();
        if !deps.is_empty() {
            out.push_str("\n[dependencies]\n");
            for (name, git) in deps {
                out.push_str(&format!("{name} = \"{git}\"\n"));
            }
        }
        out
    }

    struct Env {
        _root: PathBuf,
        project: PathBuf,
        pkg: PathBuf,
        cache: PathBuf,
    }

    fn env(tag: &str) -> Env {
        let root = scratch(tag);
        let project = root.join("proj");
        write_file(
            &project.join("hella.toml"),
            "# my project\nname = \"app\"\nversion = \"0.1.0\"\n",
        );
        write_file(&project.join("src/main.hll"), "void main() do\nend\n");
        let pkg = root.join("pkg");
        let cache = root.join("cache");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        Env {
            _root: root,
            project,
            pkg,
            cache,
        }
    }

    #[test]
    fn parses_specs() {
        let r = parse_add_spec("github.com/o/r", None).unwrap();
        assert_eq!((r.git.as_str(), r.rev, r.name.as_str()), ("github.com/o/r", None, "r"));
        let r = parse_add_spec("github.com/o/r@v1.2.0", None).unwrap();
        assert_eq!(r.rev.as_deref(), Some("v1.2.0"));
        let r = parse_add_spec("owner/repo@^1.2", None).unwrap();
        assert_eq!((r.git.as_str(), r.name.as_str()), ("github.com/owner/repo", "repo"));
        let r = parse_add_spec("https://github.com/o/My-Lib.git", None).unwrap();
        assert_eq!((r.git.as_str(), r.name.as_str()), ("github.com/o/My-Lib", "my_lib"));
        let r = parse_add_spec("github.com/o/r", Some("custom")).unwrap();
        assert_eq!(r.name, "custom");
        assert!(parse_add_spec("", None).is_err());
        assert!(parse_add_spec("github.com/o/r@", None).is_err());
        assert!(parse_add_spec("github.com/o/r", Some("std")).is_err());
        assert!(parse_add_spec("github.com/o/r", Some("9bad")).is_err());
    }

    #[test]
    fn local_path_with_at_sign_is_verbatim() {
        let root = scratch("atsign");
        let weird = root.join("my@dir");
        std::fs::create_dir_all(&weird).unwrap();
        let r = parse_add_spec(weird.to_str().unwrap(), None).unwrap();
        assert_eq!(r.rev, None);
        assert_eq!(r.git, format!("local{}", weird.display()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn surgical_edits_preserve_comments() {
        let text = "# leading comment\nname = \"app\"\nversion = \"0.1.0\"\n";
        let dep = Dependency {
            git: "github.com/o/r".to_string(),
            version: "1.0.0".to_string(),
            package: None,
        };
        let out = insert_dep_line(text, "mylib", &dep);
        assert!(out.contains("# leading comment"));
        assert!(out.contains("name = \"app\""));
        assert!(out.contains("mylib = { git = \"github.com/o/r\", version = \"1.0.0\" }"));
        let m = manifest::parse_manifest(&out).unwrap();
        assert_eq!(m.name, "app");
        assert!(m.dependencies.contains_key("mylib"));
        // Replace is idempotent; removal restores the original shape.
        let again = insert_dep_line(&out, "mylib", &dep);
        assert_eq!(again.matches("mylib =").count(), 1);
        let removed = remove_dep_line(&again, "mylib");
        assert!(!removed.contains("mylib"));
        assert!(removed.contains("# leading comment"));
    }

    #[test]
    fn addFetches_resolves_and_removes() {
        let e = env("cycle");
        let repos = scratch("cycle-repos");
        // Transitive dep first: `inner` has no deps of its own.
        let inner = make_repo(
            &repos,
            "inner",
            &[
                ("hella.toml", &lib_manifest(&[])),
                ("inner.hll", "int answer() do\n    return 42\nend\n"),
            ],
            &["v0.2.0"],
        );
        // `outer` depends on `inner` by local path.
        let outer = make_repo(
            &repos,
            "outer",
            &[
                ("hella.toml", &lib_manifest(&[("inner", inner.to_str().unwrap())])),
                ("outer.hll", "int twice() do\n    return 84\nend\n"),
            ],
            &["v1.0.0"],
        );

        run_add(
            &e.project,
            &e.pkg,
            &e.cache,
            outer.to_str().unwrap(),
            None,
        )
        .unwrap();

        // Manifest gained exactly one direct dep; the file header survived.
        let text = std::fs::read_to_string(e.project.join("hella.toml")).unwrap();
        assert!(text.contains("# my project"));
        let m = manifest::parse_manifest(&text).unwrap();
        assert_eq!(m.dependencies.len(), 1);
        assert!(m.dependencies.contains_key("outer"));
        // Lock pins the full closure.
        let lock = manifest::read_lockfile(&e.project).unwrap().unwrap();
        assert_eq!(lock.packages.len(), 2);
        let names: Vec<&str> = lock.packages.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"outer") && names.contains(&"inner"));
        // Slots carry sources + markers, and the resolver sees them.
        let bases = modules::dep_bases_for_project(&e.project, &e.pkg);
        assert_eq!(bases.len(), 2);
        assert!(bases.iter().all(|b| b.is_dir()));
        assert!(
            modules::resolve_import(&["outer".to_string()], &bases).is_some()
        );
        assert!(
            modules::resolve_import(&["inner".to_string()], &bases).is_some()
        );
        // Adding twice is an error, not a duplicate.
        assert!(
            run_add(&e.project, &e.pkg, &e.cache, outer.to_str().unwrap(), None).is_err()
        );
        // A stray unmanaged slot is GC'd on remove; fetch state is pruned.
        let stray = e.pkg.join("github.com/o/stray/1.0.0");
        write_file(
            &stray.join(manifest::slot_marker_name()),
            &manifest::serialize_slot_marker("github.com/o/stray", "1.0.0", "x"),
        );
        run_remove(&e.project, &e.pkg, "outer").unwrap();
        let text = std::fs::read_to_string(e.project.join("hella.toml")).unwrap();
        assert!(!text.contains("outer"));
        assert!(text.contains("# my project"));
        let lock = manifest::read_lockfile(&e.project).unwrap().unwrap();
        assert!(lock.packages.is_empty());
        assert!(!stray.exists(), "orphan slots must be collected");
        assert!(
            modules::dep_bases_for_project(&e.project, &e.pkg).is_empty()
        );
    }

    #[test]
    fn latest_prefers_stable_and_ranges_match() {
        let repos = scratch("versions");
        let repo = make_repo(
            &repos,
            "vlib",
            &[("vlib.hll", "// v\n")],
            &["v1.0.0", "v2.0.0", "v3.0.0-beta"],
        );
        let url = repo.to_str().unwrap();
        let r = resolve_rev(url, None).unwrap();
        assert_eq!((r.version.as_str(), r.tag.as_deref()), ("2.0.0", Some("v2.0.0")));
        let r = resolve_rev(url, Some("^1.0")).unwrap();
        assert_eq!(r.version, "1.0.0");
        assert!(resolve_rev(url, Some(">=9.0")).is_err());
    }

    #[test]
    fn conflicting_names_are_loud() {
        let e = env("conflict");
        let repos = scratch("conflict-repos");
        let x = make_repo(&repos, "x", &[("x.hll", "// x\n")], &["v1.0.0"]);
        let y = make_repo(&repos, "y", &[("y.hll", "// y\n")], &["v1.0.0"]);
        // `mid` depends on `dup` (-> X).
        let mid = make_repo(
            &repos,
            "mid",
            &[("hella.toml", &lib_manifest(&[("dup", x.to_str().unwrap())]))],
            &["v1.0.0"],
        );
        run_add(&e.project, &e.pkg, &e.cache, mid.to_str().unwrap(), None).unwrap();
        // Top-level `dup` (-> Y) conflicts with the transitive pin.
        let err = run_add(&e.project, &e.pkg, &e.cache, y.to_str().unwrap(), Some("dup"))
            .unwrap_err();
        assert!(format!("{err:?}").contains("name conflict"), "{err:?}");
        // Incompatible version ranges conflict too.
        let err = resolve_and_fetch_closure(
            &e.pkg,
            &e.cache,
            &BTreeMap::from([(
                "x".to_string(),
                Dependency {
                    git: manifest::normalize_git_source(x.to_str().unwrap()),
                    version: ">=9.0".to_string(),
                    package: None,
                },
            )]),
            &[],
        )
        .unwrap_err();
        assert!(format!("{err:?}").contains("no tag"), "{err:?}");
    }

    #[test]
    fn remove_unknown_name_errors() {
        let e = env("rmunknown");
        assert!(run_remove(&e.project, &e.pkg, "nope").is_err());
    }
}
