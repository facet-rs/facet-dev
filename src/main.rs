use log::{Level, LevelFilter, Log, Metadata, Record, debug, error, warn};
use owo_colors::{OwoColorize, Style};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

mod readme;

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
        use std::process::Command;

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

    let output = Command::new("cargo")
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
            #[cfg(unix)]
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
        #[cfg(unix)]
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
        #[cfg(unix)]
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
        #[cfg(unix)]
        executable: true,
    };
    if let Err(e) = sender.send(job) {
        error!("Failed to send cargo-husky pre-commit hook job: {e}");
    }
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

fn print_stream(label: &str, data: &[u8]) {
    if data.is_empty() {
        println!("    {}: <empty>", label);
    } else {
        println!(
            "    {}:\n{}",
            label,
            String::from_utf8_lossy(data).trim_end()
        );
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
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    match output.status.code() {
        Some(code) => println!("    exit code: {}", code),
        None => println!("    exit code: terminated by signal"),
    }
    print_stream("stdout", &output.stdout);
    print_stream("stderr", &output.stderr);
    std::process::exit(1);
}

fn exit_with_command_error(command: &[String], envs: &[(&str, &str)], error: std::io::Error) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    println!("    error: {}", error);
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
/// - Buffers output for the first 5 seconds
/// - If command completes within 5s, returns without printing
/// - If it takes >5s, starts streaming output in real-time
/// - Shows elapsed time during execution
fn run_command_with_streaming(
    command: &[String],
    envs: &[(&str, &str)],
) -> Result<std::process::Output, std::io::Error> {
    let mut cmd = Command::new(&command[0]);
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
    let mut last_update = Instant::now();

    loop {
        // Check if process has exited
        match child.try_wait()? {
            Some(status) => {
                // Process has finished, collect remaining output
                while let Ok(line) = stdout_rx.try_recv() {
                    if streaming {
                        println!("{}", line);
                    }
                    stdout_buffer.push(line);
                }
                while let Ok(line) = stderr_rx.try_recv() {
                    if streaming {
                        eprintln!("{}", line);
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
                        println!("{}", line);
                    }
                    for line in &stderr_buffer {
                        eprintln!("{}", line);
                    }
                }

                // Collect new output
                let mut got_output = false;
                while let Ok(line) = stdout_rx.try_recv() {
                    if streaming {
                        println!("{}", line);
                    }
                    stdout_buffer.push(line);
                    got_output = true;
                }
                while let Ok(line) = stderr_rx.try_recv() {
                    if streaming {
                        eprintln!("{}", line);
                    }
                    stderr_buffer.push(line);
                    got_output = true;
                }

                // Update timer display if streaming and enough time has passed
                if streaming && last_update.elapsed() >= Duration::from_secs(1) {
                    eprint!(
                        "\r  {} Elapsed: {:.1}s",
                        "⏱️".yellow(),
                        elapsed.as_secs_f32()
                    );
                    io::stderr().flush().unwrap();
                    last_update = Instant::now();
                }

                // Sleep briefly to avoid busy-waiting
                if !got_output {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn run_pre_push() {
    use std::collections::{BTreeSet, HashSet};

    println!("{}", "Running pre-push checks...".cyan().bold());

    // Find the merge base with origin/main
    let merge_base_output = Command::new("git")
        .args(["merge-base", "HEAD", "origin/main"])
        .output();

    let merge_base = match merge_base_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            warn!("Failed to find merge base with origin/main, using HEAD");
            "HEAD".to_string()
        }
    };

    // Get the list of changed files
    let mut changed_files: std::collections::BTreeSet<String> = BTreeSet::new();

    let diff_output = Command::new("git")
        .args(["diff", "--name-only", &format!("{}...HEAD", merge_base)])
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

    // Include currently staged files so local runs before committing still work
    let staged_output = Command::new("git")
        .args(["diff", "--name-only", "--cached"])
        .output();

    match staged_output {
        Ok(output) if output.status.success() => {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                changed_files.insert(line.to_string());
            }
        }
        Err(e) => {
            error!("Failed to get staged files: {}", e);
            std::process::exit(1);
        }
        Ok(output) => {
            error!(
                "git diff --cached failed: {}",
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

    // Find which crates are affected
    let mut affected_crates = HashSet::new();

    for file in &changed_files {
        let path = Path::new(file);

        // Find the crate directory by looking for Cargo.toml
        let mut current = path;
        while let Some(parent) = current.parent() {
            let cargo_toml = if parent.as_os_str().is_empty() {
                PathBuf::from("Cargo.toml")
            } else {
                parent.join("Cargo.toml")
            };

            if cargo_toml.exists() {
                // Read Cargo.toml to get the package name
                if let Ok(content) = fs::read_to_string(&cargo_toml) {
                    // Simple parsing: look for [package] section and name field
                    let mut in_package = false;
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if trimmed == "[package]" {
                            in_package = true;
                        } else if trimmed.starts_with('[') {
                            in_package = false;
                        } else if in_package && trimmed.starts_with("name") {
                            if let Some(name_part) = trimmed.split('=').nth(1) {
                                let name = name_part.trim().trim_matches('"').trim_matches('\'');
                                affected_crates.insert(name.to_string());
                                break;
                            }
                        }
                    }
                }
                break;
            }

            if parent.as_os_str().is_empty() {
                break;
            }
            current = parent;
        }
    }

    if affected_crates.is_empty() {
        println!("{}", "No crates affected by changes".yellow());
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

    // Run clippy for each crate individually (fast, per-crate feedback is useful)
    for crate_name in &affected_crates {
        print!(
            "  {} Running clippy for {}... ",
            "🔍".cyan(),
            crate_name.yellow()
        );
        io::stdout().flush().unwrap();
        let clippy_command = vec![
            "cargo".to_string(),
            "clippy".to_string(),
            "-p".to_string(),
            crate_name.to_string(),
            "--all-targets".to_string(),
            "--all-features".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ];
        let mut clippy_cmd = Command::new(&clippy_command[0]);
        for arg in &clippy_command[1..] {
            clippy_cmd.arg(arg);
        }
        let clippy_output = clippy_cmd.output();

        match clippy_output {
            Ok(output) if output.status.success() => {
                println!("{}", "passed".green());
            }
            Ok(output) => {
                println!("{}", "failed".red());
                exit_with_command_failure(&clippy_command, &[], output);
            }
            Err(e) => {
                println!("{}", "failed".red());
                exit_with_command_error(&clippy_command, &[], e);
            }
        }
    }

    // Run nextest once with all affected crates (better feature unification)
    print!(
        "  {} Running nextest for all affected crates... ",
        "🧪".cyan()
    );
    io::stdout().flush().unwrap();
    let mut nextest_command = vec![
        "cargo".to_string(),
        "nextest".to_string(),
        "run".to_string(),
    ];
    for crate_name in &affected_crates {
        nextest_command.push("-p".to_string());
        nextest_command.push(crate_name.to_string());
    }
    nextest_command.push("--all-features".to_string());
    nextest_command.push("--no-tests=pass".to_string());
    let nextest_output = run_command_with_streaming(&nextest_command, &[]);

    match nextest_output {
        Ok(output) if output.status.success() => {
            println!("{}", "passed".green());
        }
        Ok(output) => {
            println!("{}", "failed".red());
            exit_with_command_failure(&nextest_command, &[], output);
        }
        Err(e) => {
            println!("{}", "failed".red());
            exit_with_command_error(&nextest_command, &[], e);
        }
    }

    // Run doc tests once with all affected crates
    print!(
        "  {} Running doc tests for all affected crates... ",
        "📚".cyan()
    );
    io::stdout().flush().unwrap();
    let mut doctest_command = vec!["cargo".to_string(), "test".to_string(), "--doc".to_string()];
    for crate_name in &affected_crates {
        doctest_command.push("-p".to_string());
        doctest_command.push(crate_name.to_string());
    }
    doctest_command.push("--all-features".to_string());
    let doctest_output = run_command_with_streaming(&doctest_command, &[]);

    match doctest_output {
        Ok(output) if output.status.success() => {
            println!("{}", "passed".green());
        }
        Ok(output) if should_skip_doc_tests(&output) => {
            println!("{}", "skipped (no lib)".yellow());
        }
        Ok(output) => {
            println!("{}", "failed".red());
            exit_with_command_failure(&doctest_command, &[], output);
        }
        Err(e) => {
            println!("{}", "failed".red());
            exit_with_command_error(&doctest_command, &[], e);
        }
    }

    // Build docs for each crate individually (per-crate feedback is useful)
    for crate_name in &affected_crates {
        print!(
            "  {} Building docs for {}... ",
            "📖".cyan(),
            crate_name.yellow()
        );
        io::stdout().flush().unwrap();
        let doc_command = vec![
            "cargo".to_string(),
            "doc".to_string(),
            "-p".to_string(),
            crate_name.to_string(),
            "--all-features".to_string(),
        ];
        let doc_env = [("RUSTDOCFLAGS", "-D warnings")];
        let mut doc_cmd = Command::new(&doc_command[0]);
        for arg in &doc_command[1..] {
            doc_cmd.arg(arg);
        }
        for (key, value) in &doc_env {
            doc_cmd.env(key, value);
        }
        let doc_output = doc_cmd.output();

        match doc_output {
            Ok(output) if output.status.success() => {
                println!("{}", "passed".green());
            }
            Ok(output) => {
                println!("{}", "failed".red());
                exit_with_command_failure(&doc_command, &doc_env, output);
            }
            Err(e) => {
                println!("{}", "failed".red());
                exit_with_command_error(&doc_command, &doc_env, e);
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
