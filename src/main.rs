use log::{Level, LevelFilter, Log, Metadata, Record, debug, error, warn};
use owo_colors::{OwoColorize, Style};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::{
    borrow::Cow,
    ffi::OsStr,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use supports_color::{self, Stream as ColorStream};
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

mod readme;

fn terminal_supports_color(stream: ColorStream) -> bool {
    supports_color::on_cached(stream).is_some()
}

fn maybe_strip_bytes<'a>(data: &'a [u8], stream: ColorStream) -> Cow<'a, [u8]> {
    if terminal_supports_color(stream) {
        Cow::Borrowed(data)
    } else {
        Cow::Owned(strip_ansi_escapes::strip(data))
    }
}

fn maybe_strip_str<'a>(line: &'a str, stream: ColorStream) -> Cow<'a, str> {
    if terminal_supports_color(stream) {
        Cow::Borrowed(line)
    } else {
        Cow::Owned(strip_ansi_escapes::strip_str(line))
    }
}

fn apply_color_env(cmd: &mut Command) {
    cmd.env("FORCE_COLOR", "1");
    cmd.env("CARGO_TERM_COLOR", "always");
}

fn command_with_color<S: AsRef<OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    apply_color_env(&mut cmd);
    cmd
}

/// Returns true if the given path is gitignored.
fn is_gitignored(path: &Path) -> bool {
    Command::new("git")
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct Job {
    path: PathBuf,
    old_content: Option<Vec<u8>>,
    new_content: Vec<u8>,
    #[cfg(unix)]
    executable: bool,
}

impl Job {
    fn is_noop(&self) -> bool {
        match &self.old_content {
            Some(old) => {
                if &self.new_content != old {
                    return false;
                }
                #[cfg(unix)]
                {
                    // Check if executable bit would change
                    let current_executable = self
                        .path
                        .metadata()
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false);
                    current_executable == self.executable
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
            None => {
                #[cfg(unix)]
                {
                    self.new_content.is_empty() && !self.executable
                }
                #[cfg(not(unix))]
                {
                    self.new_content.is_empty()
                }
            }
        }
    }

    /// Applies the job by writing out the new_content to path and staging the file.
    fn apply(&self) -> std::io::Result<()> {
        use std::fs;

        // Create parent directories if they don't exist
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.path, &self.new_content)?;

        // Set executable bit if needed
        #[cfg(unix)]
        if self.executable {
            let mut perms = fs::metadata(&self.path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&self.path, perms)?;
        }

        // Now stage it, best effort
        let _ = command_with_color("git")
            .arg("add")
            .arg(&self.path)
            .status();
        Ok(())
    }
}

fn ensure_table(item: &mut Item) -> &mut Table {
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("item to be a table")
}

fn rewrite_cargo_toml<F>(cargo_toml_path: &Path, mut transform: F) -> Option<Job>
where
    F: FnMut(&mut DocumentMut) -> bool,
{
    let content = fs::read_to_string(cargo_toml_path).ok()?;
    let mut document: DocumentMut = match content.parse() {
        Ok(doc) => doc,
        Err(e) => {
            error!(
                "Failed to parse {} as TOML: {}",
                cargo_toml_path.display(),
                e
            );
            return None;
        }
    };

    if !transform(&mut document) {
        return None;
    }

    let new_content = document.to_string();
    if new_content == content {
        return None;
    }

    Some(Job {
        path: cargo_toml_path.to_path_buf(),
        old_content: Some(content.into_bytes()),
        new_content: new_content.into_bytes(),
        #[cfg(unix)]
        executable: false,
    })
}

fn array_matches(array: &Array, expected: &[&str]) -> bool {
    if array.len() != expected.len() {
        return false;
    }

    array
        .iter()
        .zip(expected.iter())
        .all(|(value, expected_value)| value.as_str() == Some(*expected_value))
}

fn ensure_docsrs_metadata(document: &mut DocumentMut) -> bool {
    let package_table = match document.get_mut("package").and_then(Item::as_table_mut) {
        Some(table) => table,
        None => return false,
    };

    let metadata_table = ensure_table(
        package_table
            .entry("metadata")
            .or_insert(Item::Table(Table::new())),
    );
    let docs_table = ensure_table(
        metadata_table
            .entry("docs.rs")
            .or_insert(Item::Table(Table::new())),
    );

    let desired = ["--html-in-header", "arborium-header.html"];
    let already_correct = match docs_table.get("rustdoc-args") {
        Some(item) => item
            .as_array()
            .map(|array| array_matches(array, &desired))
            .unwrap_or(false),
        None => false,
    };

    if already_correct {
        return false;
    }

    let mut args_array = Array::new();
    for arg in desired {
        args_array.push(Value::from(arg));
    }
    docs_table.insert("rustdoc-args", Item::Value(Value::Array(args_array)));
    true
}

fn ensure_rust_version(document: &mut DocumentMut) -> bool {
    let package_table = match document.get_mut("package").and_then(Item::as_table_mut) {
        Some(table) => table,
        None => return false,
    };

    if package_table.get("rust-version").and_then(Item::as_str) == Some("1.87") {
        return false;
    }

    package_table.insert("rust-version", value("1.87"));
    true
}

/// Check that all workspace crates use edition 2024. Bails with an error if not.
fn check_edition_2024() {
    use std::collections::HashSet;

    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            debug!("Failed to load workspace metadata for edition check: {}", e);
            return;
        }
    };

    let mut errors: Vec<String> = Vec::new();

    // Check workspace.package.edition in root Cargo.toml (if it exists)
    let workspace_root = &metadata.workspace_root;
    let root_cargo_toml = workspace_root.join("Cargo.toml");
    if root_cargo_toml.as_std_path().exists() {
        if let Ok(content) = fs::read_to_string(root_cargo_toml.as_std_path()) {
            if let Ok(doc) = content.parse::<DocumentMut>() {
                if let Some(workspace) = doc.get("workspace").and_then(Item::as_table) {
                    if let Some(package) = workspace.get("package").and_then(Item::as_table) {
                        if let Some(edition) = package.get("edition").and_then(Item::as_str) {
                            if edition != "2024" {
                                errors.push(format!(
                                    "{}: [workspace.package].edition = {:?} (expected \"2024\")",
                                    root_cargo_toml, edition
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // Get workspace members
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    // Check each workspace crate's edition
    for package in &metadata.packages {
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        let edition = &package.edition;
        if edition.as_str() != "2024" {
            errors.push(format!(
                "{}: edition = \"{}\" (expected \"2024\")",
                package.manifest_path,
                edition.as_str()
            ));
        }
    }

    if !errors.is_empty() {
        error!(
            "{}",
            "You have been deemed OUTDATED - edition 2024 now or bust".red()
        );
        error!("");
        for err in &errors {
            error!("  {} {}", "fix:".yellow(), err);
        }
        error!("");
        error!("Set edition = \"2024\" in the above location(s) to proceed.");
        std::process::exit(1);
    }
}

/// Get the git tree hash for a directory
fn get_git_tree_hash(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", &format!("HEAD:{}", path.display())])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Cache file location: .git/facet-dev-cache.json
fn get_cache_path() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if output.status.success() {
        let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(PathBuf::from(git_dir).join("facet-dev-cache.json"))
    } else {
        None
    }
}

/// Load the check cache (crate_name -> tree_hash that passed)
fn load_check_cache() -> std::collections::HashMap<String, String> {
    let Some(cache_path) = get_cache_path() else {
        return std::collections::HashMap::new();
    };
    let Ok(content) = fs::read_to_string(&cache_path) else {
        return std::collections::HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Save the check cache
#[allow(dead_code)]
fn save_check_cache(cache: &std::collections::HashMap<String, String>) {
    let Some(cache_path) = get_cache_path() else {
        return;
    };
    if let Ok(content) = serde_json::to_string_pretty(cache) {
        let _ = fs::write(&cache_path, content);
    }
}

/// Configuration read from `[workspace.metadata.facet-dev]` in Cargo.toml
#[derive(Debug)]
struct FacetDevConfig {
    // Pre-commit jobs
    generate_readmes: bool,
    rustfmt: bool,
    cargo_lock: bool,
    arborium: bool,
    rust_version: bool,
    edition_2024: bool,

    // Pre-push checks
    clippy: bool,
    nextest: bool,
    doc_tests: bool,
    docs: bool,
    cargo_shear: bool,
}

fn load_facet_dev_config() -> FacetDevConfig {
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            debug!("Failed to load workspace metadata for config: {e}");
            return FacetDevConfig::default();
        }
    };

    // Try [package.metadata.facet-dev] first (more specific), then fall back to
    // [workspace.metadata.facet-dev]
    let facet_dev = metadata
        .root_package()
        .and_then(|p| p.metadata.get("facet-dev"))
        .or_else(|| metadata.workspace_metadata.get("facet-dev"));

    let facet_dev = match facet_dev {
        Some(v) => v,
        None => return FacetDevConfig::default(),
    };

    let get_bool = |key: &str| -> Option<bool> { facet_dev.get(key).and_then(|v| v.as_bool()) };

    FacetDevConfig {
        // Pre-commit jobs
        generate_readmes: get_bool("generate-readmes").unwrap_or(true),
        rustfmt: get_bool("rustfmt").unwrap_or(true),
        cargo_lock: get_bool("cargo-lock").unwrap_or(true),
        arborium: get_bool("arborium").unwrap_or(true),
        rust_version: get_bool("rust-version").unwrap_or(true),
        edition_2024: get_bool("edition-2024").unwrap_or(true),

        // Pre-push checks
        clippy: get_bool("clippy").unwrap_or(true),
        nextest: get_bool("nextest").unwrap_or(true),
        doc_tests: get_bool("doc-tests").unwrap_or(true),
        docs: get_bool("docs").unwrap_or(true),
        cargo_shear: get_bool("cargo-shear").unwrap_or(true),
    }
}

impl Default for FacetDevConfig {
    fn default() -> Self {
        Self {
            // Pre-commit jobs
            generate_readmes: true,
            rustfmt: true,
            cargo_lock: true,
            arborium: true,
            rust_version: true,
            edition_2024: true,

            // Pre-push checks
            clippy: true,
            nextest: true,
            doc_tests: true,
            docs: true,
            cargo_shear: true,
        }
    }
}

fn enqueue_readme_jobs(
    sender: std::sync::mpsc::Sender<Job>,
    template_dir: Option<&Path>,
    staged_files: &StagedFiles,
) {
    let workspace_dir = std::env::current_dir().unwrap();
    let entries = match fs_err::read_dir(&workspace_dir) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read workspace directory ({e})");
            return;
        }
    };

    let template_name = "README.md.in";

    // Load custom header and footer from .facet-dev-templates/ if available
    let custom_templates_dir = workspace_dir.join(".facet-dev-templates");
    let custom_header = if custom_templates_dir.exists() {
        let header_path = custom_templates_dir.join("readme-header.md");
        fs::read_to_string(&header_path).ok()
    } else {
        None
    };
    let custom_footer = if custom_templates_dir.exists() {
        let footer_path = custom_templates_dir.join("readme-footer.md");
        fs::read_to_string(&footer_path).ok()
    } else {
        None
    };

    // Helper function to process a README template
    let process_readme_template = |template_path: &Path, output_dir: &Path, crate_name: &str| {
        if !template_path.exists() {
            error!(
                "🚫 {} Please add a README.md.in template here that describes what this crate is for:\n   {}",
                "Missing template!".red().bold(),
                template_path.display().yellow()
            );
            return;
        }

        // Read the template file
        let template_input = match fs::read_to_string(template_path) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to read template {}: {e}", template_path.display());
                return;
            }
        };

        let readme_content = readme::generate(readme::GenerateReadmeOpts {
            crate_name: crate_name.to_string(),
            input: template_input,
            header: custom_header.clone(),
            footer: custom_footer.clone(),
        });

        let readme_path = output_dir.join("README.md");

        // Check if this README is staged and would be modified
        if staged_files.clean.contains(&readme_path) {
            // Get the staged content
            let staged_content = command_with_color("git")
                .args(["show", &format!(":{}", readme_path.display())])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| o.stdout);

            if let Some(staged) = staged_content {
                let new_content_bytes = readme_content.as_bytes();
                if staged != new_content_bytes {
                    // The staged version differs from what we would generate!
                    error!("");
                    error!("{}", "❌ GENERATED FILE CONFLICT DETECTED".red().bold());
                    error!("");
                    error!(
                        "You modified {} directly, but this file is auto-generated.",
                        readme_path.display().yellow()
                    );
                    error!("This pre-commit hook would overwrite your changes.");
                    error!("");
                    error!(
                        "{} Edit {} instead (the template source)",
                        "→".cyan(),
                        template_path.display().yellow()
                    );
                    error!("");
                    error!("{}", "To fix this:".cyan().bold());
                    error!("  1. Undo changes to the generated file:");
                    error!("     git restore --staged {}", readme_path.display());
                    error!("     git restore {}", readme_path.display());
                    error!("");
                    error!("  2. OR edit the template and regenerate:");
                    error!("     # Edit {}", template_path.display());
                    error!("     cargo run --release  # regenerate");
                    error!(
                        "     git add {}  # stage the generated file",
                        readme_path.display()
                    );
                    error!("");
                    error!("Refusing to commit until this conflict is resolved.");
                    std::process::exit(1);
                }
            }
        }

        let old_content = fs::read(&readme_path).ok();

        let job = Job {
            path: readme_path,
            old_content,
            new_content: readme_content.into_bytes(),
            #[cfg(unix)]
            executable: false,
        };

        if let Err(e) = sender.send(job) {
            error!("Failed to send job: {e}");
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warn!("Skipping entry: {e}");
                continue;
            }
        };
        let crate_path = entry.path();

        if !crate_path.is_dir()
            || crate_path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with('.') || name.starts_with('_')
            })
        {
            continue;
        }

        let dir_name = crate_path.file_name().unwrap().to_string_lossy();

        // Skip common non-publishable directories
        if matches!(
            dir_name.as_ref(),
            "target" | "xtask" | "examples" | "benches" | "tests" | "fuzz"
        ) {
            continue;
        }

        let cargo_toml_path = crate_path.join("Cargo.toml");
        if !cargo_toml_path.exists() {
            continue;
        }

        // Check if this crate has generate-readmes = false in its package metadata
        if crate_has_readme_disabled(&cargo_toml_path) {
            continue;
        }

        let crate_name = dir_name.to_string();

        // Check for custom template path (from --template-dir or config)
        let template_path = if let Some(custom_dir) = template_dir {
            let custom_path = custom_dir.join(&crate_name).with_extension("md.in");
            if custom_path.exists() {
                custom_path
            } else {
                // Fall back to crate's own template
                crate_path.join(template_name)
            }
        } else if crate_name == "facet" {
            Path::new(template_name).to_path_buf()
        } else {
            crate_path.join(template_name)
        };

        process_readme_template(&template_path, &crate_path, &crate_name);
    }

    // Also handle the workspace/top-level README, if there's a Cargo.toml
    let workspace_cargo_toml = workspace_dir.join("Cargo.toml");
    if !workspace_cargo_toml.exists() {
        // No top-level Cargo.toml, skip workspace README
        return;
    }

    let workspace_template_path = workspace_dir.join(template_name);

    // Get workspace name from cargo metadata so we can use the declared default member
    let workspace_name = match workspace_name_from_metadata(&workspace_dir) {
        Ok(name) => name,
        Err(err) => {
            let fallback = workspace_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("facet")
                .to_string();
            warn!(
                "Failed to determine workspace name via cargo metadata: {err}, falling back to '{fallback}'"
            );
            fallback
        }
    };

    process_readme_template(&workspace_template_path, &workspace_dir, &workspace_name);
}

fn workspace_name_from_metadata(workspace_dir: &Path) -> Result<String, String> {
    let manifest_path = workspace_dir.join("Cargo.toml");
    if !manifest_path.exists() {
        return Err("Workspace manifest Cargo.toml not found".to_string());
    }

    let output = command_with_color("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--no-deps")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run cargo metadata: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo metadata exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse cargo metadata output: {e}"))?;

    if let Some(root_id) = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("root"))
        .and_then(|root| root.as_str())
    {
        if let Some(name) = package_name_by_id(&metadata, root_id) {
            return Ok(name.to_string());
        }
    }

    if let Some(default_members) = metadata
        .get("workspace_default_members")
        .and_then(|members| members.as_array())
    {
        for member in default_members {
            if let Some(member_id) = member.as_str() {
                if let Some(name) = package_name_by_id(&metadata, member_id) {
                    return Ok(name.to_string());
                }
            }
        }
    }

    let canonical_manifest = fs::canonicalize(&manifest_path)
        .map_err(|e| format!("Failed to canonicalize workspace manifest: {e}"))?;

    if let Some(packages) = metadata
        .get("packages")
        .and_then(|packages| packages.as_array())
    {
        for pkg in packages {
            if let (Some(name), Some(manifest_path_str)) = (
                pkg.get("name").and_then(|n| n.as_str()),
                pkg.get("manifest_path").and_then(|path| path.as_str()),
            ) {
                if let Ok(pkg_manifest_path) = fs::canonicalize(manifest_path_str) {
                    if pkg_manifest_path == canonical_manifest {
                        return Ok(name.to_string());
                    }
                }
            }
        }
    }

    Err("Unable to match workspace manifest to any package".to_string())
}

fn package_name_by_id<'a>(metadata: &'a serde_json::Value, package_id: &str) -> Option<&'a str> {
    let packages = metadata.get("packages")?.as_array()?;
    for pkg in packages {
        let id = pkg.get("id")?.as_str()?;
        if id == package_id {
            return pkg.get("name")?.as_str();
        }
    }
    None
}

/// Check if a crate has `generate-readmes = false` in its `[package.metadata.facet-dev]`
fn crate_has_readme_disabled(cargo_toml_path: &Path) -> bool {
    let content = match fs::read_to_string(cargo_toml_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let doc = match content.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return false,
    };
    doc.get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("facet-dev"))
        .and_then(|f| f.get("generate-readmes"))
        .and_then(|v| v.as_bool())
        == Some(false)
}

fn enqueue_rustfmt_jobs(sender: std::sync::mpsc::Sender<Job>, staged_files: &StagedFiles) {
    use log::trace;
    use std::time::Instant;

    for path in &staged_files.clean {
        // Only process .rs files
        if let Some(ext) = path.extension() {
            if ext != "rs" {
                continue;
            }
        } else {
            continue;
        }

        trace!("rustfmt: formatting {}", path.display());

        let original = match fs::read(path) {
            Ok(val) => val,
            Err(e) => {
                error!(
                    "{} {}: {}",
                    "❌".red(),
                    path.display().to_string().blue(),
                    format_args!("Failed to read: {e}").dimmed()
                );
                continue;
            }
        };

        let size_mb = (original.len() as f64) / (1024.0 * 1024.0);

        // Format the content via rustfmt (edition 2024)
        let start = Instant::now();
        let cmd = command_with_color("rustfmt")
            .arg("--edition")
            .arg("2024")
            .arg("--emit")
            .arg("stdout")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut cmd = match cmd {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to spawn rustfmt for {}: {}", path.display(), e);
                continue;
            }
        };

        // Write source to rustfmt's stdin
        {
            let mut stdin = cmd.stdin.take().expect("Failed to take rustfmt stdin");
            if stdin.write_all(&original).is_err() {
                error!(
                    "{} {}: {}",
                    "❌".red(),
                    path.display().to_string().blue(),
                    "Failed to write src to rustfmt".dimmed()
                );
                continue;
            }
        }

        let output = match cmd.wait_with_output() {
            Ok(out) => out,
            Err(e) => {
                error!("Failed to get rustfmt output for {}: {}", path.display(), e);
                continue;
            }
        };

        let duration = start.elapsed();
        let secs = duration.as_secs_f64();
        let mbps = if secs > 0.0 { size_mb / secs } else { 0.0 };
        debug!(
            "rustfmt: {} formatted {:.2} MiB in {:.2} s ({:.2} MiB/s)",
            path.display(),
            size_mb,
            secs,
            mbps.magenta()
        );

        if !output.status.success() {
            let stderr_clean = maybe_strip_bytes(&output.stderr, ColorStream::Stderr);
            let stdout_clean = maybe_strip_bytes(&output.stdout, ColorStream::Stdout);
            error!(
                "{} {}: rustfmt failed\n{}\n{}",
                "❌".red(),
                path.display().to_string().blue(),
                String::from_utf8_lossy(&stderr_clean).dimmed(),
                String::from_utf8_lossy(&stdout_clean).dimmed()
            );
            continue;
        }

        let formatted = output.stdout;
        let job = Job {
            path: path.clone(),
            old_content: Some(original),
            new_content: formatted,
            #[cfg(unix)]
            executable: false,
        };
        if let Err(e) = sender.send(job) {
            error!("Failed to send rustfmt job for {}: {}", path.display(), e);
        }
    }
}

fn enqueue_cargo_lock_jobs(sender: std::sync::mpsc::Sender<Job>) {
    let lock_path = Path::new("Cargo.lock");

    // Check if Cargo.lock has unstaged changes
    let status_output = command_with_color("git")
        .args(["status", "--porcelain", "Cargo.lock"])
        .output();

    if let Ok(output) = status_output {
        let status = String::from_utf8_lossy(&output.stdout);

        // If there are unstaged changes (starts with space in second column, meaning modified in working tree)
        if status.contains(" M ") {
            // Stage the Cargo.lock changes
            if let Ok(content) = fs::read(lock_path) {
                let old_content = command_with_color("git")
                    .args(["show", "HEAD:Cargo.lock"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| o.stdout);

                let job = Job {
                    path: lock_path.to_path_buf(),
                    old_content,
                    new_content: content,
                    #[cfg(unix)]
                    executable: false,
                };

                if let Err(e) = sender.send(job) {
                    error!("Failed to send Cargo.lock job: {e}");
                }
            }
        }
    }
}

fn enqueue_arborium_jobs_sync() -> Vec<Job> {
    use std::collections::HashSet;

    let mut jobs = Vec::new();

    // Load workspace metadata to get all publishable crates
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            debug!(
                "Failed to load workspace metadata for arborium setup: {}",
                e
            );
            return jobs;
        }
    };

    // Get workspace members
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    // Filter to get publishable workspace crates (excluding demos and test crates)
    let arborium_header = br#"<!-- Rustdoc doesn't highlight some languages natively -- let's do it ourselves: https://github.com/bearcove/arborium -->
<script defer src="https://cdn.jsdelivr.net/npm/@arborium/arborium@1/dist/arborium.iife.js"></script>"#;

    for package in &metadata.packages {
        // Only process workspace members
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        // Skip test/example crates based on common patterns
        if package.name.contains("test") || package.name.contains("example") {
            continue;
        }

        if let Some(manifest_dir) = package.manifest_path.parent() {
            let crate_dir: PathBuf = manifest_dir.into();
            let header_path = crate_dir.join("arborium-header.html");

            // Check if the file already exists with correct content
            let old_content = fs::read(&header_path).ok();
            let new_content = arborium_header.to_vec();

            // Only create a job if the file doesn't exist or content differs
            if old_content.as_ref() != Some(&new_content) {
                let job = Job {
                    path: header_path,
                    old_content,
                    new_content,
                    #[cfg(unix)]
                    executable: false,
                };
                jobs.push(job);
            }

            // Also update Cargo.toml to add docsrs metadata if not present
            let cargo_toml_path = crate_dir.join("Cargo.toml");
            if cargo_toml_path.exists() {
                if let Some(job) = rewrite_cargo_toml(&cargo_toml_path, ensure_docsrs_metadata) {
                    jobs.push(job);
                }
            }
        }
    }

    jobs
}

fn enforce_rust_version_sync() -> Vec<Job> {
    use std::collections::HashSet;

    let mut jobs = Vec::new();

    // Load workspace metadata to get all publishable crates
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            debug!(
                "Failed to load workspace metadata for rust-version check: {}",
                e
            );
            return jobs;
        }
    };

    // Get workspace members
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    // Check each workspace crate for rust-version
    for package in &metadata.packages {
        // Only process workspace members
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        // Skip non-library crates that we don't need to track
        if package.name.contains("test") || package.name.contains("example") {
            continue;
        }

        if let Some(manifest_dir) = package.manifest_path.parent() {
            let cargo_toml_path: PathBuf = manifest_dir.into();
            if cargo_toml_path.exists() {
                if let Some(job) = rewrite_cargo_toml(&cargo_toml_path, ensure_rust_version) {
                    jobs.push(job);
                }
            }
        }
    }

    jobs
}

fn shell_escape(part: &str) -> String {
    if part
        .chars()
        .all(|c| !c.is_whitespace() && c != '"' && c != '\'' && c != '\\')
    {
        part.to_string()
    } else {
        format!("{:?}", part)
    }
}

fn format_command_line(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn cargo_subcommand_missing_message(stderr: &str, subcommand: &str) -> bool {
    let stderr_lower = stderr.to_lowercase();
    let patterns = [
        format!("no such command: `{}`", subcommand),
        format!("no such command: '{}'", subcommand),
        format!("no such subcommand: `{}`", subcommand),
        format!("no such subcommand: '{}'", subcommand),
    ];
    patterns
        .iter()
        .any(|pattern| stderr_lower.contains(&pattern.to_lowercase()))
}

fn indicates_missing_cargo_subcommand(output: &std::process::Output, subcommand: &str) -> bool {
    cargo_subcommand_missing_message(&String::from_utf8_lossy(&output.stderr), subcommand)
}

fn print_clippy_fix_hint(command: &[String]) {
    let mut fix_command = Vec::with_capacity(command.len() + 2);
    let mut inserted = false;

    for part in command {
        if !inserted && part == "--" {
            fix_command.push("--allow-dirty".to_string());
            fix_command.push("--fix".to_string());
            inserted = true;
        }
        fix_command.push(part.clone());
    }

    if !inserted {
        fix_command.push("--allow-dirty".to_string());
        fix_command.push("--fix".to_string());
    }

    println!(
        "    {} Try auto-fixing with:\n        {}\n        git commit --amend --no-edit",
        "💡".cyan(),
        format_command_line(&fix_command)
    );
}

fn print_shear_fix_hint() {
    println!(
        "    {} Try cleaning unused dependencies with:\n        cargo shear --fix",
        "💡".cyan()
    );
}

fn install_cargo_shear() {
    println!(
        "    {} Installing cargo-shear via cargo-binstall...",
        "⬇️".cyan()
    );
    let binstall_command = vec![
        "cargo".to_string(),
        "binstall".to_string(),
        "-y".to_string(),
        "cargo-shear".to_string(),
    ];
    let binstall_output = match run_command_with_streaming(&binstall_command, &[]) {
        Ok(output) => output,
        Err(e) => {
            println!("{}", "    cargo-binstall invocation failed".red());
            exit_with_command_error(&binstall_command, &[], e, None);
        }
    };

    if binstall_output.status.success() {
        println!("{}", "    cargo-shear installed".green());
        return;
    }

    println!("{}", "    cargo-binstall failed".red());
    if indicates_missing_cargo_subcommand(&binstall_output, "binstall") {
        println!(
            "    {} Install cargo-binstall first: https://github.com/cargo-bins/cargo-binstall#installation",
            "⚠️".yellow()
        );
    }
    exit_with_command_failure(&binstall_command, &[], binstall_output, None);
}

fn print_stream(label: &str, data: &[u8], stream: ColorStream) {
    if data.is_empty() {
        println!("    {}: <empty>", label);
    } else {
        let cleaned = maybe_strip_bytes(data, stream);
        let text = String::from_utf8_lossy(&cleaned);
        println!("    {}:\n{}", label, text.trim_end());
    }
}

fn print_env_vars(envs: &[(&str, &str)]) {
    for (key, value) in envs {
        println!("    env: {}={}", key, value);
    }
}

fn exit_with_command_failure(
    command: &[String],
    envs: &[(&str, &str)],
    output: std::process::Output,
    hint: Option<Box<dyn FnOnce()>>,
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    match output.status.code() {
        Some(code) => println!("    exit code: {}", code),
        None => println!("    exit code: terminated by signal"),
    }
    print_stream("stdout", &output.stdout, ColorStream::Stdout);
    print_stream("stderr", &output.stderr, ColorStream::Stderr);
    if let Some(action) = hint {
        action();
    }
    std::process::exit(1);
}

fn exit_with_command_error(
    command: &[String],
    envs: &[(&str, &str)],
    error: std::io::Error,
    hint: Option<Box<dyn FnOnce()>>,
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    println!("    error: {}", error);
    if let Some(action) = hint {
        action();
    }
    std::process::exit(1);
}

fn should_skip_doc_tests(output: &std::process::Output) -> bool {
    if output.status.code() != Some(101) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("there is nothing to test")
        || stderr.contains("found no library targets to test")
        || stderr.contains("found no binaries to test")
        || stderr.contains("no library targets found")
}

/// Runs a command with smart streaming behavior:
/// - Shows elapsed time from the start
/// - Buffers output for the first 5 seconds
/// - If command completes within 5s, returns without printing
/// - If it takes >5s, starts streaming output in real-time
/// - Updates elapsed time every second during execution
fn run_command_with_streaming(
    command: &[String],
    envs: &[(&str, &str)],
) -> Result<std::process::Output, std::io::Error> {
    let mut cmd = command_with_color(&command[0]);
    for arg in &command[1..] {
        cmd.arg(arg);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    let start_time = Instant::now();
    let threshold = Duration::from_secs(5);

    // Channels to collect output
    let (stdout_tx, stdout_rx) = mpsc::channel::<String>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();

    // Spawn threads to read stdout and stderr
    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let _ = stdout_tx.send(line);
        }
    });

    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            let _ = stderr_tx.send(line);
        }
    });

    // Collect output and decide when to start streaming
    let mut stdout_buffer = Vec::new();
    let mut stderr_buffer = Vec::new();
    let mut streaming = false;

    loop {
        // Check if process has exited
        match child.try_wait()? {
            Some(status) => {
                // Process has finished, collect remaining output
                while let Ok(line) = stdout_rx.try_recv() {
                    if streaming {
                        println!("{}", maybe_strip_str(&line, ColorStream::Stdout));
                    }
                    stdout_buffer.push(line);
                }
                while let Ok(line) = stderr_rx.try_recv() {
                    if streaming {
                        eprintln!("{}", maybe_strip_str(&line, ColorStream::Stderr));
                    }
                    stderr_buffer.push(line);
                }

                // Wait for reader threads to finish
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();

                // If we were streaming, print a newline for clean output
                if streaming {
                    println!();
                }

                // Reconstruct output
                let stdout_bytes = stdout_buffer.join("\n").into_bytes();
                let stderr_bytes = stderr_buffer.join("\n").into_bytes();

                return Ok(std::process::Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                });
            }
            None => {
                // Process is still running
                let elapsed = start_time.elapsed();

                // Check if we should start streaming
                if !streaming && elapsed >= threshold {
                    streaming = true;
                    // Print the elapsed time indicator
                    println!(
                        "  {} Taking longer than expected, streaming output...",
                        "⏱️".yellow()
                    );
                    // Flush buffered output
                    for line in &stdout_buffer {
                        println!("{}", maybe_strip_str(line, ColorStream::Stdout));
                    }
                    for line in &stderr_buffer {
                        eprintln!("{}", maybe_strip_str(line, ColorStream::Stderr));
                    }
                }

                // Collect new output
                let mut got_output = false;
                while let Ok(line) = stdout_rx.try_recv() {
                    if streaming {
                        println!("{}", maybe_strip_str(&line, ColorStream::Stdout));
                    }
                    stdout_buffer.push(line);
                    got_output = true;
                }
                while let Ok(line) = stderr_rx.try_recv() {
                    if streaming {
                        eprintln!("{}", maybe_strip_str(&line, ColorStream::Stderr));
                    }
                    stderr_buffer.push(line);
                    got_output = true;
                }

                // Sleep briefly to avoid busy-waiting
                if !got_output {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn debug_packages() {
    use std::collections::HashSet;

    println!("{}", "Loading workspace metadata...".cyan().bold());

    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                println!("{}", "No Cargo.toml found, nothing to do".yellow());
                std::process::exit(0);
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                println!(
                    "{}",
                    "No workspace members found (empty virtual workspace)".yellow()
                );
                std::process::exit(0);
            }
            error!("Failed to get workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // If this is a virtual workspace with no members, show that info
    if metadata.workspace_members.is_empty() {
        println!(
            "{}",
            "No workspace members found (empty virtual workspace)".yellow()
        );
        std::process::exit(0);
    }

    println!("{}", "\n📦 Workspace Members:".cyan().bold());
    for member_id in &metadata.workspace_members {
        if let Some(package) = metadata.packages.iter().find(|p| &p.id == member_id) {
            println!(
                "  ✓ {} ({})",
                package.name,
                package.manifest_path.parent().unwrap()
            );
        }
    }

    // Get the set of excluded crate names (those that are packages but not workspace members)
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    let excluded: Vec<_> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .collect();

    if !excluded.is_empty() {
        println!("{}", "\n🚫 Excluded Packages:".yellow().bold());
        for package in excluded {
            println!(
                "  ✗ {} ({})",
                package.name,
                package.manifest_path.parent().unwrap()
            );
        }
    } else {
        println!("{}", "\n🚫 Excluded Packages: None".yellow().bold());
    }

    println!("\n✅ Total packages: {}", metadata.packages.len());
}

fn run_pre_push() {
    use std::collections::{BTreeSet, HashSet};

    let mut config = load_facet_dev_config();

    // HAVE_MERCY levels:
    // 1 (or just set) = skip slow checks (tests, doc tests, docs)
    // 2 = also skip clippy (just cargo-shear)
    // 3 = skip everything, just check formatting basically
    if let Ok(mercy) = std::env::var("HAVE_MERCY") {
        let level: u8 = mercy.parse().unwrap_or(1);
        let mut skipped = Vec::new();

        if level >= 1 {
            config.nextest = false;
            config.doc_tests = false;
            config.docs = false;
            skipped.extend(["nextest", "doc-tests", "docs"]);
        }
        if level >= 2 {
            config.clippy = false;
            skipped.push("clippy");
        }
        if level >= 3 {
            config.cargo_shear = false;
            skipped.push("cargo-shear");
        }

        println!(
            "{}",
            format!("🙏 HAVE_MERCY={}: skipping {}", level, skipped.join(", "))
                .yellow()
                .bold()
        );
    }

    // Show what's disabled via config (if anything)
    let mut config_disabled = Vec::new();
    if !config.clippy {
        config_disabled.push("clippy");
    }
    if !config.nextest {
        config_disabled.push("nextest");
    }
    if !config.doc_tests {
        config_disabled.push("doc-tests");
    }
    if !config.docs {
        config_disabled.push("docs");
    }
    if !config.cargo_shear {
        config_disabled.push("cargo-shear");
    }
    if !config_disabled.is_empty() && std::env::var("HAVE_MERCY").is_err() {
        println!(
            "{}",
            format!("⏭️  Disabled via config: {}", config_disabled.join(", ")).dimmed()
        );
    }

    println!("{}", "Running pre-push checks...".cyan().bold());

    // Fetch to ensure origin/main is up to date
    println!("  {} Fetching latest from origin...", "⬇️".cyan());
    let fetch_output = command_with_color("git")
        .args(["fetch", "origin", "main"])
        .output();

    match fetch_output {
        Ok(output) if !output.status.success() => {
            error!(
                "Failed to fetch from origin: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::process::exit(1);
        }
        Err(e) => {
            error!("Failed to run git fetch: {}", e);
            std::process::exit(1);
        }
        _ => {}
    }

    // Check if current branch is fast-forward to origin/main
    let merge_base_output = command_with_color("git")
        .args(["merge-base", "HEAD", "origin/main"])
        .output();

    let merge_base = match merge_base_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            error!("Failed to find merge base with origin/main");
            std::process::exit(1);
        }
    };

    // Get current HEAD
    let head_output = command_with_color("git")
        .args(["rev-parse", "HEAD"])
        .output();

    let head = match head_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            error!("Failed to get HEAD");
            std::process::exit(1);
        }
    };

    // If merge-base != origin/main, we have non-fast-forward changes
    if merge_base != head {
        // Check if origin/main is ahead of merge_base
        let origin_main = "origin/main";
        let ahead_check = command_with_color("git")
            .args(["rev-parse", origin_main])
            .output();

        if let Ok(output) = ahead_check {
            if output.status.success() {
                let origin_main_rev = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if origin_main_rev != merge_base {
                    error!("Your branch has diverged from origin/main");
                    error!("Please rebase your changes:");
                    error!("  git rebase origin/main");
                    std::process::exit(1);
                }
            }
        }
    }

    // Load workspace metadata
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                println!(
                    "{}",
                    "No Cargo.toml found, skipping pre-push checks".yellow()
                );
                std::process::exit(0);
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                println!(
                    "{}",
                    "No workspace members found, skipping pre-push checks".yellow()
                );
                std::process::exit(0);
            }
            error!("Failed to get workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // If this is a virtual workspace with no members, skip checks
    if metadata.workspace_members.is_empty() {
        println!(
            "{}",
            "No workspace members found, skipping pre-push checks".yellow()
        );
        std::process::exit(0);
    }

    let workspace_root = metadata.workspace_root.clone().into_std_path_buf();

    // Get the set of workspace member crate IDs
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.clone())
        .collect();

    // Get the set of excluded crate names (those that are packages but not workspace members)
    let excluded_crates: HashSet<String> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .map(|pkg| pkg.name.to_string())
        .collect();

    // Get the list of changed files between origin/main and HEAD
    let mut changed_files: std::collections::BTreeSet<String> = BTreeSet::new();

    let diff_output = command_with_color("git")
        .args(["diff", "--name-only", "origin/main", "HEAD"])
        .output();

    match diff_output {
        Ok(output) if output.status.success() => {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                changed_files.insert(line.to_string());
            }
        }
        Err(e) => {
            error!("Failed to get changed files: {}", e);
            std::process::exit(1);
        }
        Ok(output) => {
            error!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::process::exit(1);
        }
    };

    let changed_files: Vec<_> = changed_files.into_iter().collect();

    if changed_files.is_empty() {
        println!("{}", "No changes detected".green().bold());
        std::process::exit(0);
    }

    // Build a map from directory to crate name using workspace packages
    let mut dir_to_crate: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for package in &metadata.packages {
        if let Some(parent) = package.manifest_path.parent() {
            dir_to_crate.insert(parent.to_string(), package.name.to_string());
        }
    }

    // Find which crates are affected
    let mut affected_crates = HashSet::new();

    for file in &changed_files {
        let initial_path = Path::new(file);
        let mut current_path = if initial_path.is_absolute() {
            PathBuf::from(initial_path)
        } else {
            workspace_root.join(initial_path)
        };

        // Find the crate directory by walking up the path
        loop {
            let current_str = current_path.to_string_lossy().to_string();
            if let Some(crate_name) = dir_to_crate.get(&current_str) {
                affected_crates.insert(crate_name.clone());
                break;
            }

            if !current_path.pop() {
                break;
            }
        }
    }

    if affected_crates.is_empty() {
        println!("{}", "No crates affected by changes".yellow());
        std::process::exit(0);
    }

    // Filter affected crates to exclude those in the excluded list
    affected_crates.retain(|crate_name| !excluded_crates.contains(crate_name));

    if affected_crates.is_empty() {
        println!("{}", "No publishable crates affected by changes".yellow());
        std::process::exit(0);
    }

    // Build crate_name -> relative_path map for cache lookups
    let crate_to_path: std::collections::HashMap<String, PathBuf> = metadata
        .packages
        .iter()
        .filter_map(|pkg| {
            pkg.manifest_path.parent().map(|p| {
                let path = p.as_std_path();
                let rel = path.strip_prefix(&workspace_root).unwrap_or(path);
                (pkg.name.to_string(), rel.to_path_buf())
            })
        })
        .collect();

    // Load cache and filter out crates that haven't changed since last successful check
    let cache = load_check_cache();
    let mut cached_crates = Vec::new();
    affected_crates.retain(|crate_name| {
        if let Some(rel_path) = crate_to_path.get(crate_name) {
            if let Some(current_hash) = get_git_tree_hash(rel_path) {
                if cache.get(crate_name) == Some(&current_hash) {
                    cached_crates.push(crate_name.clone());
                    return false;
                }
            }
        }
        true
    });

    if !cached_crates.is_empty() {
        println!(
            "{} Skipping {} (unchanged since last check)",
            "⏭️".dimmed(),
            cached_crates.join(", ").dimmed()
        );
    }

    if affected_crates.is_empty() {
        println!("{}", "All affected crates cached, nothing to check".green());
        std::process::exit(0);
    }

    // Sort for consistent output
    let affected_crates: BTreeSet<_> = affected_crates.into_iter().collect();

    println!(
        "{} Affected crates: {}",
        "🔍".cyan(),
        affected_crates
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
            .yellow()
    );

    println!();

    // Run clippy once with all affected crates
    if config.clippy {
        print!(
            "  {} Running clippy for all affected crates... ",
            "🔍".cyan()
        );
        io::stdout().flush().unwrap();
        let start = std::time::Instant::now();
        let mut clippy_command = vec!["cargo".to_string(), "clippy".to_string()];
        for crate_name in &affected_crates {
            clippy_command.push("-p".to_string());
            clippy_command.push(crate_name.to_string());
        }
        clippy_command.extend(vec![
            "--all-targets".to_string(),
            "--all-features".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ]);
        let clippy_output = run_command_with_streaming(&clippy_command, &[]);
        let elapsed = start.elapsed();

        match clippy_output {
            Ok(output) if output.status.success() => {
                println!(
                    "{}",
                    format!("passed ({:.1}s)", elapsed.as_secs_f32()).green()
                );
            }
            Ok(output) => {
                println!(
                    "{}",
                    format!("failed ({:.1}s)", elapsed.as_secs_f32()).red()
                );
                let hint_command = clippy_command.clone();
                exit_with_command_failure(
                    &clippy_command,
                    &[],
                    output,
                    Some(Box::new(move || print_clippy_fix_hint(&hint_command))),
                );
            }
            Err(e) => {
                println!("{}", "failed".red());
                let hint_command = clippy_command.clone();
                exit_with_command_error(
                    &clippy_command,
                    &[],
                    e,
                    Some(Box::new(move || print_clippy_fix_hint(&hint_command))),
                );
            }
        }
    }

    // Run nextest once with all affected crates (better feature unification)
    if config.nextest {
        print!(
            "  {} Running nextest for all affected crates... ",
            "🧪".cyan()
        );
        io::stdout().flush().unwrap();
        let start = std::time::Instant::now();
        let mut nextest_command = vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
        ];
        for crate_name in &affected_crates {
            nextest_command.push("-p".to_string());
            nextest_command.push(crate_name.to_string());
        }
        nextest_command.push("--no-tests=pass".to_string());
        let nextest_output = run_command_with_streaming(&nextest_command, &[]);
        let elapsed = start.elapsed();

        match nextest_output {
            Ok(output) if output.status.success() => {
                println!(
                    "{}",
                    format!("passed ({:.1}s)", elapsed.as_secs_f32()).green()
                );
            }
            Ok(output) => {
                println!(
                    "{}",
                    format!("failed ({:.1}s)", elapsed.as_secs_f32()).red()
                );
                exit_with_command_failure(&nextest_command, &[], output, None);
            }
            Err(e) => {
                println!("{}", "failed".red());
                exit_with_command_error(&nextest_command, &[], e, None);
            }
        }
    }

    // Run doc tests once with all affected crates
    if config.doc_tests {
        print!(
            "  {} Running doc tests for all affected crates... ",
            "📚".cyan()
        );
        io::stdout().flush().unwrap();
        let start = std::time::Instant::now();
        let mut doctest_command =
            vec!["cargo".to_string(), "test".to_string(), "--doc".to_string()];
        for crate_name in &affected_crates {
            doctest_command.push("-p".to_string());
            doctest_command.push(crate_name.to_string());
        }
        doctest_command.push("--all-features".to_string());
        let doctest_output = run_command_with_streaming(&doctest_command, &[]);
        let elapsed = start.elapsed();

        match doctest_output {
            Ok(output) if output.status.success() => {
                println!(
                    "{}",
                    format!("passed ({:.1}s)", elapsed.as_secs_f32()).green()
                );
            }
            Ok(output) if should_skip_doc_tests(&output) => {
                println!("{}", "skipped (no lib)".yellow());
            }
            Ok(output) => {
                println!(
                    "{}",
                    format!("failed ({:.1}s)", elapsed.as_secs_f32()).red()
                );
                exit_with_command_failure(&doctest_command, &[], output, None);
            }
            Err(e) => {
                println!("{}", "failed".red());
                exit_with_command_error(&doctest_command, &[], e, None);
            }
        }
    }

    // Build docs once with all affected crates
    if config.docs {
        print!(
            "  {} Building docs for all affected crates... ",
            "📖".cyan()
        );
        io::stdout().flush().unwrap();
        let start = std::time::Instant::now();
        let mut doc_command = vec!["cargo".to_string(), "doc".to_string()];
        for crate_name in &affected_crates {
            doc_command.push("-p".to_string());
            doc_command.push(crate_name.to_string());
        }
        doc_command.push("--all-features".to_string());
        let doc_env = [("RUSTDOCFLAGS", "-D warnings")];
        let mut doc_cmd = command_with_color(&doc_command[0]);
        for arg in &doc_command[1..] {
            doc_cmd.arg(arg);
        }
        for (key, value) in &doc_env {
            doc_cmd.env(key, value);
        }
        let doc_output = doc_cmd.output();
        let elapsed = start.elapsed();

        match doc_output {
            Ok(output) if output.status.success() => {
                println!(
                    "{}",
                    format!("passed ({:.1}s)", elapsed.as_secs_f32()).green()
                );
            }
            Ok(output) => {
                println!(
                    "{}",
                    format!("failed ({:.1}s)", elapsed.as_secs_f32()).red()
                );
                exit_with_command_failure(&doc_command, &doc_env, output, None);
            }
            Err(e) => {
                println!("{}", "failed".red());
                exit_with_command_error(&doc_command, &doc_env, e, None);
            }
        }
    }

    // Run cargo-shear to check for unused dependencies
    if config.cargo_shear {
        print!(
            "  {} Running cargo-shear to check for unused dependencies... ",
            "🔍".cyan()
        );
        io::stdout().flush().unwrap();
        let start = std::time::Instant::now();

        let shear_command = vec!["cargo".to_string(), "shear".to_string()];
        let mut shear_output = match run_command_with_streaming(&shear_command, &[]) {
            Ok(output) => output,
            Err(e) => {
                println!("{}", "failed".red());
                exit_with_command_error(&shear_command, &[], e, None);
            }
        };

        if !shear_output.status.success()
            && indicates_missing_cargo_subcommand(&shear_output, "shear")
        {
            println!("    {} cargo-shear not found; installing...", "ℹ️".cyan());
            install_cargo_shear();
            shear_output = match run_command_with_streaming(&shear_command, &[]) {
                Ok(output) => output,
                Err(e) => {
                    println!("{}", "failed".red());
                    exit_with_command_error(&shear_command, &[], e, None);
                }
            };
        }
        let elapsed = start.elapsed();

        match shear_output {
            output if output.status.success() => {
                println!(
                    "{}",
                    format!("passed ({:.1}s)", elapsed.as_secs_f32()).green()
                );
            }
            output => {
                println!(
                    "{}",
                    format!("failed ({:.1}s)", elapsed.as_secs_f32()).red()
                );
                exit_with_command_failure(
                    &shear_command,
                    &[],
                    output,
                    Some(Box::new(print_shear_fix_hint)),
                );
            }
        }
    }

    println!();
    println!(
        "{} {}",
        "✅".green(),
        "All pre-push checks passed!".green().bold()
    );
    std::process::exit(0);
}

fn show_and_apply_jobs(jobs: &mut [Job]) {
    use std::io::{self, Write};

    const ACTION_REQUIRED: &str = "🚧";
    const OK: &str = "✅";
    const CANCEL: &str = "🛑";

    jobs.sort_by_key(|job| job.path.clone());

    if jobs.is_empty() {
        println!("{}", "All generated files are up-to-date".green().bold());
        return;
    }

    println!(
        "\n{}\n{}\n",
        format_args!("{ACTION_REQUIRED} GENERATION CHANGES {ACTION_REQUIRED}")
            .on_black()
            .bold()
            .yellow()
            .italic()
            .underline(),
        format_args!(
            "The following {} file{} will be updated/generated:",
            jobs.len(),
            if jobs.len() == 1 { "" } else { "s" }
        )
        .magenta()
    );
    for (idx, job) in jobs.iter().enumerate() {
        println!(
            "  {}. {}",
            (idx + 1).bold().cyan(),
            job.path.display().yellow(),
        );
    }

    let jobs_vec = jobs.to_vec();

    for job in &jobs_vec {
        print!("{} Applying {} ... ", OK, job.path.display().yellow());
        io::stdout().flush().unwrap();
        match job.apply() {
            Ok(_) => {
                println!("{}", "ok".green());
            }
            Err(e) => {
                println!("{} {}", CANCEL, format_args!("failed: {e}").red());
            }
        }
    }
    println!("{} {}", OK, "All fixes applied and staged!".green().bold());
    std::process::exit(0);
}

fn main() {
    setup_logger();

    // Accept allowed log levels: trace, debug, error, warn, info
    log::set_max_level(LevelFilter::Info);
    if let Ok(log_level) = std::env::var("RUST_LOG") {
        let allowed = ["trace", "debug", "error", "warn", "info"];
        let log_level_lc = log_level.to_lowercase();
        if allowed.contains(&log_level_lc.as_str()) {
            let level = match log_level_lc.as_str() {
                "trace" => LevelFilter::Trace,
                "debug" => LevelFilter::Debug,
                "info" => LevelFilter::Info,
                "warn" => LevelFilter::Warn,
                "error" => LevelFilter::Error,
                _ => LevelFilter::Info,
            };
            log::set_max_level(level);
        }
    }

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "pre-push" {
        run_pre_push();
        return;
    }
    if args.len() > 1 && args[1] == "debug-packages" {
        debug_packages();
        return;
    }

    // Parse --template-dir argument
    let mut template_dir: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--template-dir" && i + 1 < args.len() {
            template_dir = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else {
            i += 1;
        }
    }

    let staged_files = match collect_staged_files() {
        Ok(sf) => sf,
        Err(e) => {
            error!(
                "Failed to collect staged files: {e}\n\
                    This tool requires Git to be installed and a Git repository initialized."
            );
            std::process::exit(1);
        }
    };

    // Load facet-dev config from [workspace.metadata.facet-dev]
    let config = load_facet_dev_config();

    // Check edition 2024 requirement (bails if not met)
    if config.edition_2024 {
        check_edition_2024();
    }

    // Use a channel to collect jobs from all tasks.
    let (tx_job, rx_job) = mpsc::channel();

    let mut handles = vec![];

    if config.generate_readmes {
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            let template_dir = template_dir.clone();
            let staged_files_clone = staged_files.clone();
            move || {
                enqueue_readme_jobs(sender, template_dir.as_deref(), &staged_files_clone);
            }
        }));
    }

    if config.rustfmt {
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            move || {
                enqueue_rustfmt_jobs(sender, &staged_files);
            }
        }));
    }

    if config.cargo_lock {
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            move || {
                enqueue_cargo_lock_jobs(sender);
            }
        }));
    }

    drop(tx_job);

    // Arborium setup and rust-version enforcement run synchronously before job processing to avoid concurrent TOML edits
    let mut arborium_jobs = if config.arborium {
        enqueue_arborium_jobs_sync()
    } else {
        Vec::new()
    };
    let mut rust_version_jobs = if config.rust_version {
        enforce_rust_version_sync()
    } else {
        Vec::new()
    };

    let mut jobs: Vec<Job> = Vec::new();
    for job in rx_job {
        jobs.push(job);
    }
    jobs.append(&mut arborium_jobs);
    jobs.append(&mut rust_version_jobs);

    for handle in handles.drain(..) {
        handle.join().unwrap();
    }

    jobs.retain(|job| !job.is_noop());
    jobs.retain(|job| !is_gitignored(&job.path));
    show_and_apply_jobs(&mut jobs);
}

#[derive(Debug, Clone)]
struct StagedFiles {
    /// Files that are staged (in the index) and not dirty (working tree matches index).
    clean: Vec<PathBuf>,
}

fn collect_staged_files() -> io::Result<StagedFiles> {
    let output = command_with_color("git")
        .arg("status")
        .arg("--porcelain")
        .output()?;
    if !output.status.success() {
        panic!("Failed to run `git status --porcelain`");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut clean = Vec::new();

    for line in stdout.lines() {
        // E.g. "M  src/main.rs", "A  foo.rs", "AM foo/bar.rs"
        if line.len() < 3 {
            log::trace!("Skipping short line: {:?}", line.dimmed());
            continue;
        }
        let x = line.chars().next().unwrap();
        let y = line.chars().nth(1).unwrap();
        let path = line[3..].to_string();

        log::trace!(
            "x: {:?}, y: {:?}, path: {:?}",
            x.magenta(),
            y.cyan(),
            path.dimmed()
        );

        // Staged and not dirty (to be formatted/committed)
        if x != ' ' && x != '?' && y == ' ' {
            log::debug!(
                "{} {}",
                "-> clean (staged, not dirty):".green().bold(),
                path.as_str().blue()
            );
            clean.push(PathBuf::from(&path));
        }
    }
    Ok(StagedFiles { clean })
}

struct SimpleLogger;

impl Log for SimpleLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        // Create style based on log level
        let level_style = match record.level() {
            Level::Error => Style::new().fg_rgb::<243, 139, 168>(), // Catppuccin red (Maroon)
            Level::Warn => Style::new().fg_rgb::<249, 226, 175>(),  // Catppuccin yellow (Peach)
            Level::Info => Style::new().fg_rgb::<166, 227, 161>(),  // Catppuccin green (Green)
            Level::Debug => Style::new().fg_rgb::<137, 180, 250>(), // Catppuccin blue (Blue)
            Level::Trace => Style::new().fg_rgb::<148, 226, 213>(), // Catppuccin teal (Teal)
        };

        // Convert level to styled display
        eprintln!(
            "{} - {}: {}",
            record.level().style(level_style),
            record
                .target()
                .style(Style::new().fg_rgb::<137, 180, 250>()), // Blue for the target
            record.args()
        );
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Set up a simple logger.
fn setup_logger() {
    let logger = Box::new(SimpleLogger);
    log::set_boxed_logger(logger).unwrap();
    log::set_max_level(LevelFilter::Trace);
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        // well, it does work!
    }
}
