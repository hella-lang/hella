use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::builder::styling::{AnsiColor, Color as ClapColor, Style, Styles};
use clap::{Parser, Subcommand};
use console::{Color, style};
use indicatif::{ProgressBar, ProgressStyle};
use miette::Report;

use hella_compiler::lexer::lex;

// Standard-library sources embedded at build time by `crates/hella-cli/build.rs`
// (`stdlib/**/*.hll`), used by `hella setup` to populate the user library dir.
include!(concat!(env!("OUT_DIR"), "/stdlib_embedded.rs"));

mod pkg;

// Platform shim (`runtime/hella_rt.c`): POSIX names missing from the MSVC C
// runtime (`write`, `setenv`, `unsetenv`, `access`, `strdup`), defined only
// under `_WIN32`. Compiled and linked on Windows; nothing to do elsewhere.
#[cfg(windows)]
const HELLA_RT_C: &str = include_str!("../../../runtime/hella_rt.c");
// Structured-concurrency runtime (`runtime/hella_async.c`, Async-7).
// Compiled and linked ONLY when the program actually reaches async code
// (Async-8/Async-9): a synchronous program must not gain scheduler
// symbols or async runtime dependencies.
const HELLA_ASYNC_C: &str = include_str!("../../../runtime/hella_async.c");

/// Hella brand green #00A693 as an ANSI truecolor style.
fn brand_style() -> Style {
    Style::new()
        .fg_color(Some(ClapColor::Rgb(anstyle::RgbColor(0x00, 0xA6, 0x93))))
        .bold()
}

/// clap help/error styling in the Hella brand palette:
/// brand-green headers and literals, yellow errors, dim context.
fn cli_styles() -> Styles {
    let brand = brand_style();
    let dim =
        Style::new().fg_color(Some(ClapColor::Ansi(AnsiColor::BrightBlack)));
    let yellow =
        Style::new().fg_color(Some(ClapColor::Ansi(AnsiColor::Yellow)));
    Styles::styled()
        .header(brand)
        .usage(brand)
        .literal(brand)
        .placeholder(dim)
        .context(dim)
        .context_value(Style::new())
        .error(yellow.bold())
        .invalid(yellow.bold())
        .valid(brand)
}

/// Hella — main entry point (wraps `compiler` crate)
#[derive(Parser, Debug)]
#[command(
    name = "hella",
    version = "0.1.0",
    about = "The Hella Lang Toolchain",
    long_about = "Hella Toolchain — build, check and run .hll programs.",
    styles = cli_styles(),
    arg_required_else_help = true,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build the current project
    #[command(visible_alias = "b")]
    Build(BuildArgs),
    /// Build and run the current project
    #[command(visible_alias = "r")]
    Run(RunArgs),
    /// Type-check the current project
    #[command(visible_alias = "c")]
    Check(CheckArgs),
    /// Type-check, then lint the entry source file (advisory ownership checks)
    Lint(LintArgs),
    /// Run the Hella language server (LSP over stdio)
    Lsp,
    /// Install the embedded standard library to `~/.hella/lib`
    Setup(SetupArgs),
    /// Create a new Hella project (binary by default, `--lib` for a library)
    New(NewArgs),
    /// Format Hella source files (canonical style, in place)
    Fmt(FmtArgs),
    /// Add a third-party library (`<source>[@<rev>]`, git URL or local path)
    Add(AddArgs),
    /// Remove a third-party library dependency
    Remove(RemoveArgs),
    /// Install a Hella tool globally (release build into `~/.hella/bin`)
    Install(InstallArgs),
    /// Uninstall a globally installed Hella tool
    Uninstall(UninstallArgs),
    /// Download all locked dependencies into the cache (CI-friendly)
    Fetch,
    /// Update dependencies to the newest matching revisions
    Update(UpdateArgs),
    /// Print the dependency tree
    List,
    /// Clean orphaned slots (in a project) or the whole cache (`--cache`)
    Clean(CleanArgs),
}

#[derive(Parser, Debug)]
struct NewArgs {
    /// Project directory to create (also used as the project name)
    path: PathBuf,

    /// Create a library project (`src/lib.hll`) instead of a binary (`src/main.hll`)
    #[arg(long, default_value_t = false, conflicts_with = "bin")]
    lib: bool,

    /// Create a binary project (`src/main.hll`; this is the default)
    #[arg(long, default_value_t = false, conflicts_with = "lib")]
    bin: bool,

    /// Version control to initialize: `git` (default) or `none`
    #[arg(long, default_value = "git")]
    vcs: String,
}

#[derive(Parser, Debug)]
struct BuildArgs {
    /// Source file (.hll) to compile (default: project `src/main.hll`; use -f/--file for an explicit file)
    #[arg(short = 'f', long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Emit LLVM IR to stdout and exit (no link)
    #[arg(long, default_value_t = false)]
    emit_llvm: bool,

    /// When --emit-llvm, write IR to file instead of stdout
    #[arg(long)]
    emit_llvm_file: Option<PathBuf>,

    /// Keep object file (don't delete after linking)
    #[arg(long, default_value_t = false)]
    keep_obj: bool,

    /// Output executable path (default: input file with extension stripped)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Print AST for debugging
    #[arg(long, default_value_t = false)]
    print_ast: bool,

    /// Optimized release build (O3 IR passes + aggressive codegen)
    #[arg(long, default_value_t = false)]
    release: bool,

    /// Only print errors and program stdout (no status lines or progress bar)
    #[arg(long, default_value_t = false)]
    quiet: bool,

    /// Force coloured output even when stderr is not a terminal
    /// (also honours the `FORCE_COLOR` environment variable)
    #[arg(long, default_value_t = false)]
    color: bool,

    /// Show verbose progress (default: summary lines)
    #[arg(long, default_value_t = false)]
    verbose: bool,

    /// Force recompilation even if output is up to date
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Do not touch the network; error if any dependency is unfetched
    #[arg(long, default_value_t = false)]
    offline: bool,

    /// Error instead of resolving or updating hella.lock (CI reproducibility)
    #[arg(long, default_value_t = false)]
    frozen: bool,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Source file (.hll) to compile and run (default: project `src/main.hll`; use -f/--file for an explicit file)
    #[arg(short = 'f', long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Keep object file (don't delete after linking)
    #[arg(long, default_value_t = false)]
    keep_obj: bool,

    /// Print AST for debugging
    #[arg(long, default_value_t = false)]
    print_ast: bool,

    /// Optimized release build (O3 IR passes + aggressive codegen)
    #[arg(long, default_value_t = false)]
    release: bool,

    /// Only print errors and program stdout (no status lines or progress bar)
    #[arg(long, default_value_t = false)]
    quiet: bool,

    /// Force coloured output even when stderr is not a terminal
    /// (also honours the `FORCE_COLOR` environment variable)
    #[arg(long, default_value_t = false)]
    color: bool,

    /// Show verbose progress (default: summary lines)
    #[arg(long, default_value_t = false)]
    verbose: bool,

    /// Force recompilation even if output is up to date
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Do not touch the network; error if any dependency is unfetched
    #[arg(long, default_value_t = false)]
    offline: bool,

    /// Error instead of resolving or updating hella.lock (CI reproducibility)
    #[arg(long, default_value_t = false)]
    frozen: bool,

    /// Arguments forwarded to the program (use `--` to separate them)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    program_args: Vec<String>,
}

#[derive(Parser, Debug)]
struct SetupArgs {
    /// Overwrite files that already exist in `~/.hella/lib`
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Parser, Debug)]
struct FmtArgs {
    /// Files or directories to format (directories recurse for `.hll`
    /// files; defaults to the current directory)
    paths: Vec<PathBuf>,

    /// Report files that would change without modifying them
    #[arg(long, default_value_t = false)]
    check: bool,
}

#[derive(Parser, Debug)]
struct LintArgs {
    #[command(flatten)]
    check: CheckArgs,

    /// Exit unsuccessfully if any lint warnings are found
    #[arg(long)]
    deny_warnings: bool,
}

#[derive(Parser, Debug)]
struct CheckArgs {
    /// Source file (.hll) to check (default: project `src/main.hll` or `src/lib.hll`; use -f/--file for an explicit file)
    #[arg(short = 'f', long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Print AST for debugging
    #[arg(long, default_value_t = false)]
    print_ast: bool,

    /// Only print errors (no status lines or progress bar)
    #[arg(long, default_value_t = false)]
    quiet: bool,

    /// Force coloured output even when stderr is not a terminal
    /// (also honours the `FORCE_COLOR` environment variable)
    #[arg(long, default_value_t = false)]
    color: bool,

    /// Show verbose progress (default: summary lines)
    #[arg(long, default_value_t = false)]
    verbose: bool,

    /// Do not touch the network; error if any dependency is unfetched
    #[arg(long, default_value_t = false)]
    offline: bool,

    /// Error instead of resolving or updating hella.lock (CI reproducibility)
    #[arg(long, default_value_t = false)]
    frozen: bool,
}

#[derive(Parser, Debug)]
struct AddArgs {
    /// Package source with optional `@<rev>`: `github.com/owner/repo`,
    /// `owner/repo`, URL, or local path (e.g. `github.com/owner/repo@v1.2.0`)
    spec: String,

    /// Short import name to register (default: derived from the repo name)
    #[arg(long)]
    name: Option<String>,
}

#[derive(Parser, Debug)]
struct RemoveArgs {
    /// Short import name of the dependency to remove
    name: String,
}

#[derive(Parser, Debug)]
struct InstallArgs {
    /// Tool source with optional `@<rev>`: `github.com/owner/repo`,
    /// `owner/repo`, URL, or local path
    spec: String,

    /// Binary name to install as (default: the repo's package name)
    #[arg(long)]
    bin: Option<String>,
}

#[derive(Parser, Debug)]
struct UninstallArgs {
    /// Name of the installed tool to remove
    tool: String,
}

#[derive(Parser, Debug)]
struct UpdateArgs {
    /// Dependencies to update (default: all)
    names: Vec<String>,
}

#[derive(Parser, Debug)]
struct CleanArgs {
    /// Empty the whole package cache instead of just project orphans
    #[arg(long, default_value_t = false)]
    cache: bool,
}

/// Shared knobs for the compile pipeline (used by `build`, `run` and `check`).
struct CompileOptions<'a> {
    file: &'a Path,
    emit_llvm: bool,
    emit_llvm_file: Option<&'a Path>,
    keep_obj: bool,
    print_ast: bool,
    release: bool,
    quiet: bool,
    force_color: bool,
    verbose: bool,
    exe_path: Option<PathBuf>,
    /// Stop after sema (no codegen/link). Used by `check`.
    check_only: bool,
    /// Emit `missing `main` function` when no `main` is declared.
    /// Builds and runs always require an entry point; `check` only
    /// requires it for files actually named `main` (library modules
    /// are entry-less by design — same rule as the LSP).
    require_main: bool,
    force: bool,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build(args) => run_build(args),
        Commands::Run(args) => run_run(args),
        Commands::Check(args) => run_check(args),
        Commands::Lint(args) => run_lint(args),
        // The language server speaks LSP on stdio and terminates itself with
        // its own exit code after the client sends `exit`.
        Commands::Lsp => std::process::exit(hella_lsp::server::run()),
        Commands::Setup(args) => run_setup(args),
        Commands::New(args) => run_new(args),
        Commands::Fmt(args) => run_fmt(args),
        Commands::Add(args) => run_add(args),
        Commands::Remove(args) => run_remove(args),
        Commands::Install(args) => run_install(args),
        Commands::Uninstall(args) => run_uninstall(args),
        Commands::Fetch => run_fetch(),
        Commands::Update(args) => run_update(args),
        Commands::List => run_list(),
        Commands::Clean(args) => run_clean(args),
    }
}

/// A resolved entry file plus optional project context (when no explicit
/// file was given and the current directory is a Hella project).
struct ResolvedEntry {
    path: PathBuf,
    project: Option<Project>,
}

/// Project context: root (holds `hella.toml`), binary name, and the
/// build-output directory for the active profile.
struct Project {
    root: PathBuf,
    bin_name: String,
}

impl Project {
    /// `out/debug` or `out/release` under the project root (created on use).
    fn out_dir(&self, release: bool) -> PathBuf {
        self.root
            .join("out")
            .join(if release { "release" } else { "debug" })
    }

    /// Binary file name: `name` plus `.exe` on Windows, where the OS
    /// requires an extension to execute a program.
    fn bin_filename(&self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.bin_name)
        } else {
            self.bin_name.clone()
        }
    }
}

/// Resolve the entry file for build/run/check: an explicit file wins;
/// otherwise the project convention applies (`src/main.hll`, falling back
/// to `src/lib.hll` for `check`-able library sources).
/// Project mode (no explicit file) requires a valid `hella.toml` project;
/// `build`/`run` are canonically project commands.
fn resolve_entry(explicit: Option<PathBuf>) -> miette::Result<ResolvedEntry> {
    if let Some(f) = explicit {
        return Ok(ResolvedEntry {
            path: f,
            project: None,
        });
    }
    let cwd = std::env::current_dir().map_err(|e| {
        miette::miette!("failed to read current directory: {e}")
    })?;
    let Some(root) = find_project_root(&cwd) else {
        return Err(miette::miette!(
            "not in a Hella project (no hella.toml found in {} or parents); run `hella new <name>` or use -f/--file <FILE>",
            cwd.display()
        ));
    };
    // Validate hella.toml (read_manifest errors if malformed or missing name/version)
    let manifest = read_manifest(&root)?;
    if manifest.is_none() {
        return Err(miette::miette!(
            "invalid hella.toml in {} (must define `name` and `version`)",
            root.display()
        ));
    }
    for cand in ["src/main.hll", "src/lib.hll"] {
        let path = root.join(cand);
        if path.is_file() {
            let bin_name = manifest
                .as_ref()
                .map(|m| m.name.clone())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "main".to_string())
                });
            return Ok(ResolvedEntry {
                path,
                project: Some(Project { root, bin_name }),
            });
        }
    }
    Err(miette::miette!(
        "no entry file found in project {} (looked for src/main.hll and src/lib.hll)",
        root.display()
    ))
}

/// Walk up from `start` looking for a `hella.toml`; the directory holding it
/// is the project root. `None` when outside any project.
fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if dir.join("hella.toml").is_file() {
            return Some(dir);
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return None,
        }
    }
}

/// Minimal `hella.toml` reader (only `name`/`version` exist for now; unknown
/// keys are ignored so the manifest stays forward-compatible).
struct Manifest {
    name: String,
    version: String,
}

fn read_manifest(root: &Path) -> miette::Result<Option<Manifest>> {
    let path = root.join("hella.toml");
    if !path.is_file() {
        return Ok(None);
    }
    let parsed = hella_compiler::manifest::read_manifest_file(root)
        .map_err(|e| miette::miette!("invalid {}: {e}", path.display()))?;
    Ok(parsed.map(|m| Manifest {
        name: m.name,
        version: m.version,
    }))
}

/// Create a new Hella project: `hella.toml` manifest, VCS-ignored `out/`
/// build directory (via `.gitignore`), and `src/main.hll` (binary,
/// default) or `src/lib.hll` (`--lib`).
fn run_new(args: NewArgs) -> miette::Result<()> {
    match args.vcs.as_str() {
        "git" | "none" => {}
        other => {
            return Err(miette::miette!(
                "unsupported --vcs {other:?}: only `git` and `none` for now"
            ));
        }
    }
    let name = args
        .path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            miette::miette!("invalid project path: {}", args.path.display())
        })?;
    if args.path.exists() {
        if !args.path.is_dir() {
            return Err(miette::miette!(
                "{} exists and is not a directory",
                args.path.display()
            ));
        }
        let non_empty = fs::read_dir(&args.path)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(miette::miette!(
                "{} already exists and is not empty",
                args.path.display()
            ));
        }
    }

    let entry_name = if args.lib { "lib.hll" } else { "main.hll" };
    // `{NAME}` is substituted below; lowercase `{name}` in the library
    // template is genuine Hella string interpolation and is left alone.
    let entry_template = if args.lib {
        "// {NAME} — Hella library.\n\
         //\n\
         // Add modules next to this file and import them by file name:\n\
         // `import mymod` → `mymod.hll`, `import net::http` → `net/http.hll`.\n\
         \n\
         string greet(string name) do\n\
         \x20   return \"hello {name}\"\n\
         end\n"
    } else {
        "// {NAME} — Hella binary project.\n\
         \n\
         import std::io\n\
         \n\
         void main() do\n\
         \x20   println(\"Hello from {NAME}!\")\n\
         end\n"
    };
    let entry_src = entry_template.replace("{NAME}", &name);
    let files = [
        (
            "hella.toml".to_string(),
            format!(
                "name = \"{name}\"\nversion = \"0.1.0\"\n\
                 \n\
                 # Third-party libraries live here once added:\n\
                 # `hella add github.com/owner/repo[@rev]` appends\n\
                 # `[dependencies]` and pins the exact SHA in `hella.lock`.\n"
            ),
        ),
        (format!("src/{entry_name}"), entry_src),
        (".gitignore".to_string(), "/out/\n".to_string()),
    ];

    fs::create_dir_all(args.path.join("src")).map_err(|e| {
        miette::miette!("failed to create {}: {e}", args.path.display())
    })?;
    for (rel, contents) in &files {
        let dest = args.path.join(rel);
        if dest.exists() {
            return Err(miette::miette!(
                "refusing to overwrite existing {}",
                dest.display()
            ));
        }
        fs::write(&dest, contents).map_err(|e| {
            miette::miette!("failed to write {}: {e}", dest.display())
        })?;
    }

    if args.vcs == "git" {
        match Command::new("git").arg("init").arg(&args.path).status() {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!(
                "{:>11} `git init` exited with {s} — continuing without VCS",
                brand("Warning")
            ),
            Err(e) => eprintln!(
                "{:>11} could not run `git init` ({e}) — continuing without VCS",
                brand("Warning")
            ),
        }
    }

    eprintln!(
        "{:>11} {} ({})",
        brand("Created"),
        gpath(&args.path),
        if args.lib { "library" } else { "binary" }
    );
    for (rel, _) in &files {
        eprintln!("{:>11} {}", brand("Wrote"), args.path.join(rel).display());
    }
    eprintln!(
        "{:>11} cd {} && {}",
        brand("Next"),
        args.path.display(),
        if args.lib { "hella check" } else { "hella run" }
    );
    Ok(())
}

/// Format Hella source files in place (`hella fmt [paths...]`).
///
/// Accepts individual `.hll` files and directories (recursed for `.hll`
/// files). Formatting is deterministic and idempotent; with `--check` files
/// that would change are reported without modification and the process
/// exits non-zero when any file is unformatted.
fn run_fmt(args: FmtArgs) -> miette::Result<()> {
    let roots: Vec<PathBuf> = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths.clone()
    };
    for root in &roots {
        if !root.exists() {
            return Err(miette::miette!(
                "no such file or directory: {}",
                root.display()
            ));
        }
    }
    let files = hella_fmt::collect_sources(&roots);
    if files.is_empty() {
        // An explicit single file that is not `.hll` is almost certainly a
        // user mistake — say so instead of silently doing nothing.
        if roots.len() == 1 && roots[0].is_file() {
            return Err(miette::miette!(
                "not a Hella source file: {} (expected `.hll`)",
                roots[0].display()
            ));
        }
        eprintln!("{:>11} no .hll files found", brand("Fmt"));
        return Ok(());
    }
    let mut changed = 0usize;
    let mut failed = 0usize;
    for path in &files {
        let source = fs::read_to_string(path).map_err(|e| {
            miette::miette!("failed to read {}: {e}", path.display())
        })?;
        match hella_fmt::format_source(&source) {
            Ok(formatted) => {
                if formatted != source {
                    changed += 1;
                    if args.check {
                        println!("{}", path.display());
                    } else {
                        fs::write(path, formatted).map_err(|e| {
                            miette::miette!(
                                "failed to write {}: {e}",
                                path.display()
                            )
                        })?;
                        eprintln!(
                            "{:>11} {}",
                            brand("Formatted"),
                            path.display()
                        );
                    }
                }
            }
            Err(e) => {
                failed += 1;
                let diag = hella_compiler::error::SingleDiagnostic::new(
                    path.display().to_string(),
                    source.clone(),
                    e.span.unwrap_or(hella_compiler::token::Span::new(0, 0)),
                    e.message.clone(),
                );
                eprintln!("{:?}", Report::new(diag));
            }
        }
    }
    if args.check {
        if failed > 0 {
            return Err(miette::miette!("{failed} file(s) failed to parse"));
        }
        if changed > 0 {
            return Err(miette::miette!(
                "{changed} file(s) would be reformatted"
            ));
        }
        eprintln!(
            "{:>11} {} file(s) already formatted",
            brand("Checked"),
            files.len()
        );
        return Ok(());
    }
    if failed > 0 {
        return Err(miette::miette!("{failed} file(s) failed to format"));
    }
    eprintln!(
        "{:>11} {} file(s), {} reformatted",
        brand("Finished"),
        files.len(),
        changed,
    );
    Ok(())
}

/// Package-cache roots for `add`/`remove`: the `pkg/` slots plus a `cache/`
/// scratch area (falls back to the system temp dir when no home exists).
fn pkg_dirs() -> miette::Result<(PathBuf, PathBuf)> {
    let home = hella_compiler::modules::hella_home()
        .unwrap_or_else(|| std::env::temp_dir().join(".hella"));
    let pkg_root = home.join("pkg");
    let cache_root = home.join("cache");
    for dir in [&pkg_root, &cache_root] {
        fs::create_dir_all(dir).map_err(|e| {
            miette::miette!("failed to create {}: {e}", dir.display())
        })?;
    }
    Ok((pkg_root, cache_root))
}

/// Add a third-party library to the current project.
fn run_add(args: AddArgs) -> miette::Result<()> {
    let cwd = std::env::current_dir().map_err(|e| {
        miette::miette!("failed to read current directory: {e}")
    })?;
    let Some(root) = find_project_root(&cwd) else {
        return Err(miette::miette!(
            "not in a Hella project (no hella.toml found in {} or parents); run `hella new <name>` first",
            cwd.display()
        ));
    };
    let (pkg_root, cache_root) = pkg_dirs()?;
    pkg::run_add(
        &root,
        &pkg_root,
        &cache_root,
        &args.spec,
        args.name.as_deref(),
    )
}

/// Remove a third-party library from the current project.
fn run_remove(args: RemoveArgs) -> miette::Result<()> {
    let cwd = std::env::current_dir().map_err(|e| {
        miette::miette!("failed to read current directory: {e}")
    })?;
    let Some(root) = find_project_root(&cwd) else {
        return Err(miette::miette!(
            "not in a Hella project (no hella.toml found in {} or parents)",
            cwd.display()
        ));
    };
    let (pkg_root, _) = pkg_dirs()?;
    pkg::run_remove(&root, &pkg_root, &args.name)
}

/// Install a Hella tool globally: clone to a temp staging dir, resolve its
/// own dependencies, release-build, place the binary in `~/.hella/bin/`,
/// and clean up. Never touches the current project's `hella.toml`.
fn run_install(args: InstallArgs) -> miette::Result<()> {
    let Some(bin_dir) = hella_compiler::modules::hella_bin_dir() else {
        return Err(miette::miette!(
            "cannot locate a home directory for `hella install` (needs $HOME on Unix, %USERPROFILE% on Windows)"
        ));
    };
    fs::create_dir_all(&bin_dir).map_err(|e| {
        miette::miette!("failed to create {}: {e}", bin_dir.display())
    })?;
    let (pkg_root, cache_root) = pkg_dirs()?;
    let tool = pkg::prepare_tool(&cache_root, &args.spec, args.bin.as_deref())?;
    let dest = pkg::bin_path(&bin_dir, &tool.name);
    let result = (|| -> miette::Result<()> {
        // The tool's own third-party deps resolve into the shared cache.
        pkg::ensure_deps(
            &tool.dir,
            &pkg_root,
            &cache_root,
            &pkg::EnsureOptions {
                offline: false,
                frozen: false,
            },
        )?;
        let opts = CompileOptions {
            file: &tool.entry,
            emit_llvm: false,
            emit_llvm_file: None,
            keep_obj: false,
            print_ast: false,
            release: true,
            quiet: false,
            force_color: false,
            verbose: false,
            exe_path: Some(dest.clone()),
            check_only: false,
            require_main: true,
            force: true,
        };
        let _ = compile(opts)?;
        Ok(())
    })();
    pkg::cleanup_tool(&tool);
    result?;
    eprintln!("Installed {} → {}", tool.name, dest.display());
    if !bin_on_path(&bin_dir) {
        eprintln!(
            "Warning: {} is not on your $PATH — add it (e.g. `export PATH=\"$PATH:{}\"` in your shell profile)",
            bin_dir.display(),
            bin_dir.display(),
        );
    }
    Ok(())
}

/// True when `dir` is one of the `$PATH` entries (best effort: compares
/// canonicalized forms when both sides resolve).
fn bin_on_path(dir: &Path) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    let dir_canon = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    std::env::split_paths(&path_var).any(|p| {
        p == *dir || fs::canonicalize(&p).is_ok_and(|c| c == dir_canon)
    })
}

/// Uninstall a globally installed Hella tool.
fn run_uninstall(args: UninstallArgs) -> miette::Result<()> {
    let Some(bin_dir) = hella_compiler::modules::hella_bin_dir() else {
        return Err(miette::miette!(
            "cannot locate a home directory for `hella uninstall` (needs $HOME on Unix, %USERPROFILE% on Windows)"
        ));
    };
    pkg::run_uninstall(&bin_dir, &args.tool)
}

/// Resolve the current project root or error with the standard hint.
fn project_root_or_err(what: &str) -> miette::Result<PathBuf> {
    let cwd = std::env::current_dir().map_err(|e| {
        miette::miette!("failed to read current directory: {e}")
    })?;
    find_project_root(&cwd).ok_or_else(|| {
        miette::miette!(
            "not in a Hella project (no hella.toml found in {} or parents); run `hella new <name>` first (`{what}` needs a project)",
            cwd.display(),
        )
    })
}

/// Download all locked dependencies into the cache.
fn run_fetch() -> miette::Result<()> {
    let root = project_root_or_err("hella fetch")?;
    let (pkg_root, cache_root) = pkg_dirs()?;
    pkg::run_fetch(&root, &pkg_root, &cache_root)
}

/// Update dependencies to the newest matching revisions.
fn run_update(args: UpdateArgs) -> miette::Result<()> {
    let root = project_root_or_err("hella update")?;
    let (pkg_root, cache_root) = pkg_dirs()?;
    pkg::run_update(&root, &pkg_root, &cache_root, &args.names)
}

/// Print the dependency tree.
fn run_list() -> miette::Result<()> {
    let root = project_root_or_err("hella list")?;
    let (pkg_root, _) = pkg_dirs()?;
    pkg::run_list(&root, &pkg_root)
}

/// Clean orphaned slots (in a project) or the whole cache (`--cache`).
fn run_clean(args: CleanArgs) -> miette::Result<()> {
    let (pkg_root, cache_root) = pkg_dirs()?;
    if args.cache {
        pkg::run_clean_cache(&pkg_root, &cache_root)?;
        return Ok(());
    }
    let root = project_root_or_err("hella clean")?;
    pkg::run_clean_project(&root, &pkg_root)?;
    Ok(())
}

/// Install the embedded standard library (`stdlib/**/*.hll` baked in by
/// `crates/hella-cli/build.rs`) to `~/.hella/lib`, the directory the compiler searches
/// for `import`ed libraries. Existing files are kept unless `--force`.
fn run_setup(args: SetupArgs) -> miette::Result<()> {
    let Some(dest_root) = hella_compiler::modules::hella_lib_dir() else {
        return Err(miette::miette!(
            "cannot locate a home directory for `hella setup` (needs $HOME on Unix, %USERPROFILE% on Windows)"
        ));
    };
    let mut installed = 0usize;
    let mut skipped = 0usize;
    for (rel, contents) in STDLIB_FILES {
        let dest = dest_root.join(rel);
        if dest.is_file() && !args.force {
            skipped += 1;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                miette::miette!("failed to create {}: {e}", parent.display())
            })?;
        }
        fs::write(&dest, contents).map_err(|e| {
            miette::miette!("failed to write {}: {e}", dest.display())
        })?;
        installed += 1;
    }
    eprintln!(
        "{:>11} stdlib → {} ({installed} installed, {skipped} kept)",
        brand("Setup"),
        gpath(&dest_root),
    );
    Ok(())
}

/// Toolchain terminal output: a single progress bar covering the whole
/// compile, plus one static status line per completed phase on stderr with a
/// right-aligned brand-green prefix (`{:>11}`, `Compiled in 0.12s`). `--quiet`
/// silences everything except errors and program stdout.
fn init_colors(force: bool) {
    if force
        || std::env::var("FORCE_COLOR")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        console::set_colors_enabled(true);
    } else if !std::io::stderr().is_terminal() {
        console::set_colors_enabled(false);
    }
}

/// Brand color: Hella green #00A693 (RGB 0, 166, 147).
fn brand(prefix: &str) -> String {
    style(prefix)
        .fg(Color::TrueColor(0x00, 0xA6, 0x93))
        .bold()
        .to_string()
}

/// Brand-green path for the summary line.
fn gpath(p: &Path) -> String {
    style(p.display().to_string())
        .fg(Color::TrueColor(0x00, 0xA6, 0x93))
        .to_string()
}

/// Yellow duration, matching compiler-error yellow accents.
fn gduration(d: Duration) -> String {
    style(seconds(d)).fg(Color::Yellow).to_string()
}

/// Formats durations as `0.12s`.
fn seconds(d: Duration) -> String {
    format!("{:.2}s", d.as_millis() as f32 / 1000.)
}

/// Single status line on stderr: brand-green right-aligned prefix + message.
/// Suppressed under `--quiet`. Suspends the progress bar so the bar redraws
/// cleanly instead of being corrupted by raw `eprintln!` output.
fn status(pb: &ProgressBar, quiet: bool, prefix: &str, msg: &str) {
    if !quiet {
        pb.suspend(|| eprintln!("{:>11} {}", brand(prefix), msg));
    }
}

/// Single progress bar for the whole compile (read → lex → parse → resolve →
/// check → codegen → link). Created once per `build`/`run` and advanced with
/// `set_message` + `inc(1)`; hidden under `--quiet`. Bar and spinner render
/// in the brand green.
fn new_progress_bar(quiet: bool, total_steps: u64) -> ProgressBar {
    let pb = if quiet {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(total_steps)
    };
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{bar:30.green}] {pos}/{len} {msg}",
        )
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        .progress_chars("#>-"),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Central error sink: all diagnostics render to stderr, then the process
/// exits non-zero.
fn fail(report: Report) -> ! {
    eprintln!("{report:?}");
    std::process::exit(1);
}

/// Fetch the owning project's locked dependencies before compile (no-op
/// outside projects and for dependency-free manifests).
fn ensure_entry_deps(
    entry: &Path,
    offline: bool,
    frozen: bool,
) -> miette::Result<()> {
    let root = hella_compiler::modules::project_root(entry);
    if !root.join("hella.toml").is_file() {
        return Ok(());
    }
    let (pkg_root, cache_root) = pkg_dirs()?;
    pkg::ensure_deps(
        &root,
        &pkg_root,
        &cache_root,
        &pkg::EnsureOptions { offline, frozen },
    )
}

fn run_build(args: BuildArgs) -> miette::Result<()> {
    let entry = resolve_entry(args.file.clone())?;
    ensure_entry_deps(&entry.path, args.offline, args.frozen)?;
    // Project mode redirects output to `out/debug|release/<name>` unless
    // `-o` is given; file mode keeps the legacy next-to-source default.
    let exe_path = args.output.clone().or_else(|| {
        entry
            .project
            .as_ref()
            .map(|p| p.out_dir(args.release).join(p.bin_filename()))
    });
    let opts = CompileOptions {
        file: &entry.path,
        emit_llvm: args.emit_llvm,
        emit_llvm_file: args.emit_llvm_file.as_deref(),
        keep_obj: args.keep_obj,
        print_ast: args.print_ast,
        release: args.release,
        quiet: args.quiet,
        force_color: args.color,
        verbose: args.verbose,
        exe_path,
        check_only: false,
        require_main: true,
        force: args.force,
    };
    let _ = compile(opts)?;
    Ok(())
}

fn run_check(args: CheckArgs) -> miette::Result<()> {
    let entry = resolve_entry(args.file.clone())?;
    ensure_entry_deps(&entry.path, args.offline, args.frozen)?;
    let require_main = entry.path.file_stem().is_some_and(|s| s == "main");
    let opts = CompileOptions {
        file: &entry.path,
        emit_llvm: false,
        emit_llvm_file: None,
        keep_obj: true,
        print_ast: args.print_ast,
        release: false,
        quiet: args.quiet,
        force_color: args.color,
        verbose: args.verbose,
        exe_path: None,
        check_only: true,
        require_main,
        force: false,
    };
    let _ = compile(opts)?;
    Ok(())
}

fn run_lint(args: LintArgs) -> miette::Result<()> {
    let entry = resolve_entry(args.check.file.clone())?;
    // Keep dependency resolution, entry-point policy and semantic errors exactly
    // as in `check`. Never emit advisory warnings in place of type checking.
    run_check(args.check)?;

    // Reparse the entry, NOT the import-expanded program: imported items carry
    // offsets from other files. Linting is deliberately entry-file-only.
    let source = fs::read_to_string(&entry.path)
        .map_err(|e| miette::miette!("failed to read {}: {e}", entry.path.display()))?;
    let lexed = lex(&source);
    if !lexed.errors.is_empty() {
        return Err(miette::miette!("source changed during lint; rerun the command"));
    }
    let program = hella_compiler::parse::parse(lexed.tokens, source.clone())
        .map_err(|e| miette::miette!("{}: {}", entry.path.display(), e.message))?;
    let warnings = hella_compiler::lint::check(&program);
    for warning in &warnings {
        let before = &source[..warning.span.start];
        let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        eprintln!("{}:{line}:{column}: warning[{}]: {}",
            entry.path.display(), warning.code, warning.message);
    }
    if args.deny_warnings && !warnings.is_empty() {
        return Err(miette::miette!("{} lint warning(s) (--deny-warnings)", warnings.len()));
    }
    Ok(())
}

fn run_run(args: RunArgs) -> miette::Result<()> {
    let entry = resolve_entry(args.file.clone())?;
    ensure_entry_deps(&entry.path, args.offline, args.frozen)?;
    // The binary persists: project mode uses `out/debug|release/<name>`
    // (ignored by VCS), file mode the legacy next-to-source default.
    // Rebuilds are skipped while sources are unchanged (see freshness in
    // `compile`), so repeated `run` is cheap.
    let exe_path = match &entry.project {
        Some(p) => p.out_dir(args.release).join(p.bin_filename()),
        None => default_exe_path(&entry.path, None),
    };

    let opts = CompileOptions {
        file: &entry.path,
        emit_llvm: false,
        emit_llvm_file: None,
        keep_obj: args.keep_obj,
        print_ast: args.print_ast,
        release: args.release,
        quiet: args.quiet,
        force_color: args.color,
        verbose: args.verbose,
        exe_path: Some(exe_path),
        check_only: false,
        require_main: true,
        force: args.force,
    };
    let built = compile(opts)?;
    let exe = match built {
        Some(p) => p,
        None => return Ok(()), // empty program: nothing to run
    };

    // ── Run ──────────────────────────────────────────────────────────
    if !args.quiet {
        eprintln!("{:>11} {}", brand("Running"), exe.display());
    }
    let status = Command::new(&exe)
        .args(&args.program_args)
        .status()
        .map_err(|e| {
            miette::miette!("failed to execute {}: {e}", exe.display())
        })?;

    // The binary is kept (rebuilt only when sources change).

    // Propagate the program's exit code so `hella run` behaves like the binary.
    match status.code() {
        Some(0) | None => {
            if !status.success() {
                // Killed by signal (Unix): surface as failure.
                std::process::exit(1);
            }
            Ok(())
        }
        Some(code) => std::process::exit(code),
    }
}

/// Full pipeline: read → lex → parse → resolve → check → codegen/link.
///
/// A single progress bar covers the whole process, plus static status lines on
/// stderr. Returns the executable path when a binary was produced, or `None`
/// for `--emit-llvm` / empty programs.
fn compile(opts: CompileOptions<'_>) -> miette::Result<Option<PathBuf>> {
    let file = opts.file;
    init_colors(opts.force_color);
    let quiet = opts.quiet;
    let start_all = Instant::now();
    // read, lex, parse, resolve, check, [codegen/emit, link]
    let total_steps: u64 = if opts.check_only {
        5
    } else if opts.emit_llvm {
        6
    } else {
        7
    };
    let pb = new_progress_bar(quiet, total_steps);

    status(&pb, quiet, "Compiling", &gpath(file).to_string());

    // ── Read ─────────────────────────────────────────────────────────
    pb.set_message("Reading");
    let source = fs::read_to_string(file).map_err(|e| {
        pb.abandon();
        miette::miette!("failed to read {}: {e}", gpath(file))
    })?;
    let filename = file.display().to_string();
    pb.inc(1);
    if opts.verbose {
        status(
            &pb,
            quiet,
            "Reading",
            &format!("{} ({} bytes)", gpath(file), source.len()),
        );
    }

    // ── Lex ──────────────────────────────────────────────────────────
    pb.set_message("Lexing");
    let t_lex = Instant::now();
    let out = lex(&source);
    pb.inc(1);
    if opts.verbose {
        status(
            &pb,
            quiet,
            "Lexed",
            &format!(
                "{} tokens in {}",
                out.tokens.len(),
                gduration(t_lex.elapsed())
            ),
        );
    }
    if !out.errors.is_empty() {
        let parts: Vec<(hella_compiler::token::Span, String)> = out
            .errors
            .into_iter()
            .map(|e| (e.span, format!("unexpected token `{}`", e.slice)))
            .collect();
        let multi = hella_compiler::error::MultiDiagnostic::from_errors(
            filename.clone(),
            source.clone(),
            parts,
            "lexing failed".into(),
        );
        pb.abandon();
        fail(Report::new(multi));
    }
    let is_empty_program = out.tokens.iter().all(|st| {
        matches!(
            st.token,
            hella_compiler::token::Token::Newline
                | hella_compiler::token::Token::Semicolon
        )
    });
    if is_empty_program && out.tokens.is_empty() {
        pb.finish_with_message("Finished (empty file)");
        println!("ok: {} — 0 tokens (empty file)", filename);
        return Ok(None);
    }

    // ── Parse ────────────────────────────────────────────────────────
    pb.set_message("Parsing");
    let t_parse = Instant::now();
    let program = match hella_compiler::parse::parse(
        out.tokens.clone(),
        source.clone(),
    ) {
        Ok(p) => {
            pb.inc(1);
            if opts.verbose {
                status(
                    &pb,
                    quiet,
                    "Parsed",
                    &format!(
                        "{} items in {}",
                        p.items.len(),
                        gduration(t_parse.elapsed())
                    ),
                );
            }
            p
        }
        Err(e) => {
            let diag = hella_compiler::error::SingleDiagnostic::new(
                filename.clone(),
                source.clone(),
                e.span,
                e.message,
            );
            pb.abandon();
            fail(Report::new(diag));
        }
    };

    // ── Import expansion ─────────────────────────────────────────────
    pb.set_message("Resolving imports");
    let t_import = Instant::now();
    // `@cfg(debug)` is true only for a debug *link* profile. `check` (no
    // link at all) deliberately treats it as false so type analysis never
    // depends on which profile would have been built.
    let cfg_debug = !opts.release && !opts.check_only;
    let expanded =
        hella_compiler::modules::expand_imports_with_cfg(program, file, cfg_debug);
    if let Some(first) = expanded.errors.into_iter().next() {
        let diag = hella_compiler::error::SingleDiagnostic::new(
            filename.clone(),
            source.clone(),
            first.span,
            first.message,
        );
        pb.abandon();
        fail(Report::new(diag));
    }
    // Complete source set the build depends on (entry + resolved imports):
    // a rebuild is skipped when the binary is newer than all of these.
    let source_files = expanded.files;
    let program = expanded.program;
    let program = {
        pb.inc(1);
        if opts.verbose {
            status(
                &pb,
                quiet,
                "Resolved",
                &format!(
                    "{} items in {}",
                    program.items.len(),
                    gduration(t_import.elapsed())
                ),
            );
        }
        program
    };
    if opts.print_ast {
        println!("{:#?}", program);
    }

    // ── Sema ─────────────────────────────────────────────────────────
    pb.set_message("Checking");
    status(&pb, quiet, "Checking", &filename);
    let t_check = Instant::now();
    let sema_errors = hella_compiler::sema::check_with_options(
        &program,
        hella_compiler::sema::CheckOptions {
            require_main: opts.require_main,
        },
    );
    pb.inc(1);
    if !sema_errors.is_empty() {
        let parts: Vec<(hella_compiler::token::Span, String)> = sema_errors
            .into_iter()
            .map(|e| (e.span, e.message))
            .collect();
        let multi = hella_compiler::error::MultiDiagnostic::from_errors(
            filename.clone(),
            source.clone(),
            parts,
            "semantic error".into(),
        );
        pb.abandon();
        fail(Report::new(multi));
    }
    if opts.check_only {
        pb.finish_with_message("Finished");
        status(
            &pb,
            quiet,
            "Checked",
            &format!("{} in {}", gpath(file), gduration(start_all.elapsed())),
        );
        return Ok(None);
    }
    status(
        &pb,
        quiet,
        "Checked",
        &format!("in {}", seconds(t_check.elapsed())),
    );

    let opt = if opts.release {
        hella_compiler::codegen::OptLevel::Release
    } else {
        hella_compiler::codegen::OptLevel::Debug
    };

    // ── Codegen ──────────────────────────────────────────────────────
    if opts.emit_llvm {
        pb.set_message("Generating LLVM IR");
        status(&pb, quiet, "Generating", "LLVM IR");
        let t_ir = Instant::now();
        let ir = match generate_ir_string(&program, opt) {
            Ok(ir) => ir,
            Err(e) => {
                pb.abandon();
                return Err(e);
            }
        };
        pb.inc(1);
        status(
            &pb,
            quiet,
            "Generated",
            &format!("in {}", gduration(t_ir.elapsed())),
        );
        if let Some(path) = opts.emit_llvm_file {
            fs::write(path, &ir)
                .map_err(|e| miette::miette!("failed to write IR: {e}"))?;
            status(&pb, quiet, "Exported", &gpath(path).to_string());
        } else {
            println!("{ir}");
        }
        pb.finish_with_message("Finished");
        status(
            &pb,
            quiet,
            "Compiled",
            &format!("in {}", gduration(start_all.elapsed())),
        );
        return Ok(None);
    }

    let exe_path = default_exe_path(file, opts.exe_path.clone());
    let obj_path = obj_path_for_exe(&exe_path);
    // Project mode (`out/debug|release/`) and explicit `-o` paths may point
    // into directories that don't exist yet — neither LLVM nor clang creates
    // parent directories.
    for p in [&obj_path, &exe_path] {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    miette::miette!(
                        "failed to create {}: {e}",
                        parent.display()
                    )
                })?;
            }
        }
    }

    // ── Async runtime requirement (Async-8/Async-9) ──────────────────
    // Reachability-based: importing/declaring unused async code does not
    // pull the runtime in. Decided before the freshness check because the
    // runtime set is part of what identifies the binary.
    let needs_async = hella_compiler::async_req::uses_async_runtime(&program);

    // ── Freshness ────────────────────────────────────────────────────
    // Skip codegen+link when the binary is newer than every source file
    // (entry + resolved imports) and the build stamp still matches this
    // profile and toolchain version. `run` relies on this: its binary
    // persists between invocations and only rebuilds on change.
    if !opts.force && is_fresh(&exe_path, &source_files, opts.release, needs_async) {
        pb.finish_with_message("Finished");
        status(
            &pb,
            quiet,
            "Fresh",
            &format!("{} (up to date)", gpath(&exe_path),),
        );
        return Ok(Some(exe_path));
    }

    pb.set_message(format!("Compiling {}", obj_path.display()));
    let build_kind = if opts.release { "release" } else { "debug" };
    status(
        &pb,
        quiet,
        "Compiling",
        &format!("{} ({build_kind})", gpath(&obj_path)),
    );
    let t_cg = Instant::now();
    if let Err(e) =
        codegen_to_object(&program, &obj_path, &filename, &source, opt)
    {
        pb.abandon();
        return Err(e);
    }
    pb.inc(1);
    if opts.verbose {
        status(
            &pb,
            quiet,
            "Compiled",
            &format!("{} in {}", gpath(&obj_path), gduration(t_cg.elapsed())),
        );
    }

    // ── Link ─────────────────────────────────────────────────────────
    pb.set_message(format!("Linking {}", exe_path.display()));
    let t_link = Instant::now();
    let linker = find_linker().map_err(|e| {
        pb.abandon();
        e
    })?;
    let mut link = Command::new(&linker);
    // Windows: the MSVC C runtime lacks a few POSIX names the stdlib
    // declares (`write`, `setenv`, `unsetenv`, `access`, `strdup`), so
    // compile the embedded `runtime/hella_rt.c` shim and link it along.
    // The shim is a no-op TU everywhere else and is skipped there.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut extra_paths: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        let rt_src = obj_path.with_file_name("hella_rt_gen.c");
        let rt_obj = obj_path.with_file_name("hella_rt_gen.o");
        if let Err(e) = fs::write(&rt_src, HELLA_RT_C) {
            pb.abandon();
            return Err(miette::miette!(
                "failed to write {}: {e}",
                rt_src.display()
            ));
        }
        extra_paths.push(rt_src.clone());
        extra_paths.push(rt_obj.clone());
        let cc_status = Command::new(&linker)
            .arg("-c")
            .arg(&rt_src)
            .arg("-o")
            .arg(&rt_obj)
            .status()
            .map_err(|e| {
                pb.abandon();
                miette::miette!(
                    "failed to invoke {linker} for runtime shim: {e}"
                )
            })?;
        if !cc_status.success() {
            pb.abandon();
            return Err(miette::miette!(
                "compiling the Windows runtime shim failed with {linker}"
            ));
        }
        link.arg(&rt_obj);
    }
    // Async-9: compile + link the structured-concurrency runtime only when
    // the program reaches async code. `-pthread` is required on Linux (and
    // harmless on macOS, where pthread lives in libc); Windows needs no
    // extra library (Win32 threads come from kernel32).
    if needs_async {
        let a_src = obj_path.with_file_name("hella_async_gen.c");
        let a_obj = obj_path.with_file_name("hella_async_gen.o");
        if let Err(e) = fs::write(&a_src, HELLA_ASYNC_C) {
            pb.abandon();
            return Err(miette::miette!("failed to write {}: {e}", a_src.display()));
        }
        extra_paths.push(a_src.clone());
        extra_paths.push(a_obj.clone());
        let mut cc = Command::new(&linker);
        cc.arg("-c").arg(&a_src).arg("-o").arg(&a_obj);
        if !cfg!(windows) {
            cc.arg("-pthread");
        }
        let cc_status = cc.status().map_err(|e| {
            pb.abandon();
            miette::miette!("failed to invoke {linker} for the async runtime: {e}")
        })?;
        if !cc_status.success() {
            pb.abandon();
            return Err(miette::miette!(
                "compiling the async runtime failed with {linker}"
            ));
        }
        link.arg(&a_obj);
        if !cfg!(windows) {
            link.arg("-pthread");
        }
    }
    link.arg(&obj_path).arg("-o").arg(&exe_path);
    // Linux does not fold libm into libc: `-lm` is required wherever
    // `std::math` (or any `from "libm"` extern) may appear. The system
    // linker drops it when unused (`--as-needed`), so passing it
    // unconditionally is harmless.
    //
    // `-no-pie`: codegen emits absolute relocations (non-PIC objects),
    // which the default PIE link on Linux rejects
    // (`relocation R_X86_64_32 ... can not be used when making a PIE
    // object`). Long term this belongs in codegen (PIC emission);
    // the link flag is the minimal correct fix here.
    if cfg!(target_os = "linux") {
        link.arg("-lm");
        link.arg("-no-pie");
    }
    let link_status = link.status().map_err(|e| {
        pb.abandon();
        miette::miette!("failed to invoke {linker}: {e} — {}", link_hint())
    })?;
    if !link_status.success() {
        pb.abandon();
        return Err(miette::miette!("linking failed with {linker}"));
    }
    pb.inc(1);
    if opts.verbose {
        status(
            &pb,
            quiet,
            "Linked",
            &format!("{} in {}", gpath(&exe_path), gduration(t_link.elapsed())),
        );
    }

    if !opts.keep_obj {
        let _ = fs::remove_file(&obj_path);
        for extra in &extra_paths {
            let _ = fs::remove_file(extra);
        }
    }
    // Record what produced this binary so a later invocation can prove
    // freshness without recompiling (profile + toolchain version; sources
    // are compared by mtime against the binary itself).
    write_build_stamp(&exe_path, opts.release, needs_async);

    pb.finish_with_message("Finished");
    status(
        &pb,
        quiet,
        "Compiled",
        &format!(
            "{} → {} in {}",
            gpath(file),
            gpath(&exe_path),
            gduration(start_all.elapsed())
        ),
    );
    Ok(Some(exe_path))
}

/// C linker used for the final link (and the Windows runtime shim):
/// `$HELLA_LINKER` wins, else the first available of `clang`, `cc`.
/// `cc` covers Linux boxes without clang (typically gcc) while `clang`
/// is the only practical driver on Windows (MSVC `link.exe` backend)
/// and macOS (Xcode CLT).
fn find_linker() -> miette::Result<String> {
    if let Ok(l) = std::env::var("HELLA_LINKER") {
        if !l.trim().is_empty() {
            return Ok(l);
        }
    }
    for cand in ["clang", "cc"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Ok(cand.to_string());
        }
    }
    Err(miette::miette!(
        "no C linker found (tried `clang`, `cc`) — {}",
        link_hint()
    ))
}

/// Platform-appropriate hint for installing a C linker.
fn link_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "install the Xcode command line tools (`xcode-select --install`)"
    } else if cfg!(target_os = "windows") {
        "install LLVM (https://releases.llvm.org/download.html) or set HELLA_LINKER"
    } else {
        "install clang or gcc (e.g. `apt install clang`) or set HELLA_LINKER"
    }
}

/// Sidecar recording the profile + toolchain that produced a binary.
/// `<exe>.hellastamp`, e.g. `out/debug/demo.hellastamp`.
fn stamp_path(exe: &Path) -> PathBuf {
    let mut p = exe.as_os_str().to_owned();
    p.push(".hellastamp");
    PathBuf::from(p)
}

fn stamp_contents(release: bool, needs_async: bool) -> String {
    format!(
        "profile={}\ntoolchain=hella {}\nasync={}\n",
        if release { "release" } else { "debug" },
        env!("CARGO_PKG_VERSION"),
        // Async-9: the linked runtime set is part of what produced the
        // binary, so a sync<->async transition must rebuild even when no
        // source mtime changed.
        if needs_async { "runtime" } else { "none" },
    )
}

fn write_build_stamp(exe: &Path, release: bool, needs_async: bool) {
    let _ = fs::write(stamp_path(exe), stamp_contents(release, needs_async));
}

/// True when `exe` exists, is newer than every source file, and its stamp
/// matches this profile + toolchain version. Anything else (missing binary
/// or stamp, profile/version switch, touched source) means rebuild.
fn is_fresh(exe: &Path, sources: &[PathBuf], release: bool, needs_async: bool) -> bool {
    let exe_mtime = match fs::metadata(exe).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    match fs::read_to_string(stamp_path(exe)) {
        Ok(contents) if contents == stamp_contents(release, needs_async) => {}
        _ => return false,
    }
    sources.iter().all(|s| {
        fs::metadata(s)
            .and_then(|m| m.modified())
            .map(|t| exe_mtime >= t)
            .unwrap_or(false)
    })
}

fn generate_ir_string(
    program: &hella_compiler::ast::Program,
    opt: hella_compiler::codegen::OptLevel,
) -> miette::Result<String> {
    use inkwell::context::Context;
    let ctx = Context::create();
    let mut cg = hella_compiler::codegen::Codegen::new(&ctx, "hella");
    cg.release = opt == hella_compiler::codegen::OptLevel::Release;
    cg.compile_program(program).map_err(|e| {
        miette::miette!(
            "codegen error: {} at {}..{}",
            e.message,
            e.span.start,
            e.span.end
        )
    })?;
    if opt == hella_compiler::codegen::OptLevel::Release {
        let machine =
            hella_compiler::codegen::target_machine(opt).map_err(|e| {
                miette::miette!("failed to create target machine: {e}")
            })?;
        cg.optimize_for_release(&machine)
            .map_err(|e| miette::miette!("release passes failed: {e}"))?;
    }
    Ok(cg.get_module_ir())
}

fn codegen_to_object(
    program: &hella_compiler::ast::Program,
    obj_path: &Path,
    filename: &str,
    source: &str,
    opt: hella_compiler::codegen::OptLevel,
) -> miette::Result<()> {
    if let Err(msg) =
        hella_compiler::codegen::compile_to_object(program, obj_path, opt)
    {
        let diag = hella_compiler::error::SingleDiagnostic::new(
            filename.to_string(),
            source.to_string(),
            hella_compiler::token::Span::new(0, source.len()),
            msg,
        );
        // Render to stderr via the single diagnostic sink, then exit non-zero.
        fail(Report::new(diag));
    }
    Ok(())
}

/// Default executable path: the input file with its extension stripped
/// (`examples/hello.hll` → `examples/hello`), i.e. a proper binary with no
/// `.out` suffix, plus `.exe` on Windows where the OS needs an extension
/// to execute a program. An explicit `-o/--output` is used verbatim.
fn default_exe_path(input: &Path, override_: Option<PathBuf>) -> PathBuf {
    if let Some(o) = override_ {
        return o;
    }
    let mut p = input.to_path_buf();
    // Strip only a real extension (e.g. `.hll`); extensionless input stays as-is.
    if input.extension().is_some() {
        p.set_extension("");
    }
    if cfg!(windows) && p.extension().is_none() {
        p.set_extension("exe");
    }
    p
}

/// Object path derived from the executable path, guaranteed not to collide
/// with the exe itself (e.g. `-o foo.o` → `foo.obj.o` instead of `foo.o`).
fn obj_path_for_exe(exe: &Path) -> PathBuf {
    if exe.extension().is_some_and(|e| e == "o") {
        let mut p = exe.to_path_buf();
        p.set_extension("obj.o");
        p
    } else {
        let mut p = exe.to_path_buf();
        p.set_extension("o");
        p
    }
}
