use log::{Level, LevelFilter, Log, Metadata, Record, debug, error, warn};
use owo_colors::{OwoColorize, Style};
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

mod readme;

#[derive(Debug, Clone)]
struct Job {
    path: PathBuf,
    old_content: Option<Vec<u8>>,
    new_content: Vec<u8>,
    executable: bool,
}

impl Job {
    fn is_noop(&self) -> bool {
        match &self.old_content {
            Some(old) => {
                if &self.new_content != old {
                    return false;
                }
                // Check if executable bit would change
                let current_executable = self
                    .path
                    .metadata()
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false);
                current_executable == self.executable
            }
            None => self.new_content.is_empty() && !self.executable,
        }
    }

    /// Applies the job by writing out the new_content to path and staging the file.
    fn apply(&self) -> std::io::Result<()> {
        use std::fs;
        use std::process::Command;

        // Create parent directories if they don't exist
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.path, &self.new_content)?;

        // Set executable bit if needed
        if self.executable {
            let mut perms = fs::metadata(&self.path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&self.path, perms)?;
        }

        // Now stage it, best effort
        let _ = Command::new("git").arg("add").arg(&self.path).status();
        Ok(())
    }
}

fn enqueue_readme_jobs(sender: std::sync::mpsc::Sender<Job>) {
    let workspace_dir = std::env::current_dir().unwrap();
    let entries = match fs_err::read_dir(&workspace_dir) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read workspace directory ({e})");
            return;
        }
    };

    let template_name = "README.md.in";

    // Helper function to process a README template
    let process_readme_template = |template_path: &Path, output_dir: &Path, crate_name: &str| {
        if !template_path.exists() {
            error!("🚫 Missing template: {}", template_path.display().red());
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
        });

        let readme_path = output_dir.join("README.md");
        let old_content = fs::read(&readme_path).ok();

        let job = Job {
            path: readme_path,
            old_content,
            new_content: readme_content.into_bytes(),
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
        if dir_name == "target" {
            continue;
        }

        let cargo_toml_path = crate_path.join("Cargo.toml");
        if !cargo_toml_path.exists() {
            continue;
        }

        let crate_name = dir_name.to_string();

        let template_path = if crate_name == "facet" {
            Path::new(template_name).to_path_buf()
        } else {
            crate_path.join(template_name)
        };

        process_readme_template(&template_path, &crate_path, &crate_name);
    }

    // Also handle the workspace/top-level README, if any
    let workspace_template_path = workspace_dir.join(template_name);

    // Get workspace name from cargo tree
    let workspace_name = match Command::new("cargo")
        .arg("tree")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|child| {
            let output = child.wait_with_output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    // Extract package name from "package-name v0.1.0 (/path/to/package)"
                    if let Some(space_pos) = first_line.find(' ') {
                        return Ok(first_line[..space_pos].to_string());
                    }
                }
            }
            Err(std::io::Error::other("Failed to parse cargo tree output"))
        }) {
        Ok(name) => name,
        Err(e) => {
            warn!("Failed to get workspace name from cargo tree: {e}, falling back to 'facet'");
            "facet".to_string()
        }
    };

    process_readme_template(&workspace_template_path, &workspace_dir, &workspace_name);
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
        let cmd = Command::new("rustfmt")
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
            error!(
                "{} {}: rustfmt failed\n{}\n{}",
                "❌".red(),
                path.display().to_string().blue(),
                String::from_utf8_lossy(&output.stderr).dimmed(),
                String::from_utf8_lossy(&output.stdout).dimmed()
            );
            continue;
        }

        let formatted = output.stdout;
        let job = Job {
            path: path.clone(),
            old_content: Some(original),
            new_content: formatted,
            executable: false,
        };
        if let Err(e) = sender.send(job) {
            error!("Failed to send rustfmt job for {}: {}", path.display(), e);
        }
    }
}

static GITHUB_TEST_WORKFLOW: &str = include_str!(".github/workflows/test.yml");

fn enqueue_github_workflow_jobs(sender: std::sync::mpsc::Sender<Job>) {
    use std::fs;
    let workflow_path = Path::new(".github/workflows/test.yml");
    let old_content = fs::read(workflow_path).ok();

    // Check if the old content contains the handwritten marker
    if let Some(content) = &old_content {
        if let Ok(content_str) = String::from_utf8(content.clone()) {
            if content_str.contains("# HANDWRITTEN: facet-dev") {
                return;
            }
        }
    }

    let new_content = GITHUB_TEST_WORKFLOW.as_bytes().to_vec();
    let job = Job {
        path: workflow_path.to_path_buf(),
        old_content,
        new_content,
        executable: false,
    };
    if let Err(e) = sender.send(job) {
        error!("Failed to send GitHub workflow job: {e}");
    }
}

static GITHUB_FUNDING_YML: &str = include_str!(".github/FUNDING.yml");

fn enqueue_github_funding_jobs(sender: std::sync::mpsc::Sender<Job>) {
    use std::fs;
    let funding_path = Path::new(".github/FUNDING.yml");
    let old_content = fs::read(funding_path).ok();
    let new_content = GITHUB_FUNDING_YML.as_bytes().to_vec();
    let job = Job {
        path: funding_path.to_path_buf(),
        old_content,
        new_content,
        executable: false,
    };
    if let Err(e) = sender.send(job) {
        error!("Failed to send GitHub funding job: {e}");
    }
}

static CARGO_HUSKY_PRECOMMIT_HOOK: &str = include_str!(".cargo-husky/hooks/pre-commit");

fn enqueue_cargo_husky_precommit_hook_jobs(sender: std::sync::mpsc::Sender<Job>) {
    use std::process::Command;

    // Check if cargo-husky is a dev dependency
    let output = Command::new("cargo")
        .arg("tree")
        .arg("-e")
        .arg("dev")
        .arg("-i")
        .arg("cargo-husky")
        .output();

    match output {
        Ok(output) => {
            if !output.status.success() {
                error!("cargo-husky is not a dev dependency or cargo tree failed");
                error!("To add cargo-husky as a dev dependency, run:");
                error!("  cargo add cargo-husky --dev --no-default-features -F user-hooks");
                std::process::exit(1);
            }
        }
        Err(e) => {
            error!("Failed to run cargo tree command: {e}");
            std::process::exit(1);
        }
    }

    let hook_path = Path::new(".cargo-husky/hooks/pre-commit");
    let old_content = fs::read(hook_path).ok();
    let new_content = CARGO_HUSKY_PRECOMMIT_HOOK.as_bytes().to_vec();
    let job = Job {
        path: hook_path.to_path_buf(),
        old_content,
        new_content,
        executable: true,
    };
    if let Err(e) = sender.send(job) {
        error!("Failed to send cargo-husky pre-commit hook job: {e}");
    }
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

    // Use a channel to collect jobs from all tasks.
    let (tx_job, rx_job) = mpsc::channel();

    let mut handles = vec![];

    handles.push(std::thread::spawn({
        let sender = tx_job.clone();
        move || {
            enqueue_readme_jobs(sender);
        }
    }));

    handles.push(std::thread::spawn({
        let sender = tx_job.clone();
        move || {
            enqueue_rustfmt_jobs(sender, &staged_files);
        }
    }));

    handles.push(std::thread::spawn({
        let sender = tx_job.clone();
        move || {
            enqueue_github_workflow_jobs(sender);
        }
    }));

    handles.push(std::thread::spawn({
        let sender = tx_job.clone();
        move || {
            enqueue_github_funding_jobs(sender);
        }
    }));

    handles.push(std::thread::spawn({
        let sender = tx_job.clone();
        move || {
            enqueue_cargo_husky_precommit_hook_jobs(sender);
        }
    }));

    drop(tx_job);

    let mut jobs: Vec<Job> = Vec::new();
    for job in rx_job {
        jobs.push(job);
    }

    for handle in handles.drain(..) {
        handle.join().unwrap();
    }

    jobs.retain(|job| !job.is_noop());
    show_and_apply_jobs(&mut jobs);
}

#[derive(Debug)]
struct StagedFiles {
    /// Files that are staged (in the index) and not dirty (working tree matches index).
    clean: Vec<PathBuf>,
}

fn collect_staged_files() -> io::Result<StagedFiles> {
    let output = Command::new("git")
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
