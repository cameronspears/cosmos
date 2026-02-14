use super::{
    strip_ansi_sequences, ImplementationCommandOutcome, ImplementationQuickCheckStatus,
    ImplementationQuickChecksMode,
};
use crate::lab::sandbox::SandboxSession;
use cosmos_adapters::util::{run_command_with_timeout, truncate};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const MAX_COMMAND_OUTPUT_TAIL_CHARS: usize = 4_000;

#[derive(Debug, Clone)]
pub(super) enum QuickCheckCommand {
    Shell(String),
    Program { program: String, args: Vec<String> },
}

pub(super) fn program_available_on_path(program: &str) -> bool {
    let program = program.trim();
    if program.is_empty() {
        return false;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.permissions().mode() & 0o111 != 0 {
                    return true;
                }
            }
        }
        #[cfg(not(unix))]
        {
            return true;
        }
    }
    false
}

pub(super) fn detect_quick_check_command(repo_root: &Path) -> Option<QuickCheckCommand> {
    if let Ok(shell_cmd) = std::env::var("COSMOS_FIX_HARNESS_CHECK_CMD") {
        if !shell_cmd.trim().is_empty() {
            return Some(QuickCheckCommand::Shell(shell_cmd));
        }
    }

    if repo_root.join("Cargo.toml").exists() {
        let args = if repo_root.join("Cargo.lock").exists() {
            vec!["check".to_string(), "--locked".to_string()]
        } else {
            vec!["check".to_string()]
        };
        return Some(QuickCheckCommand::Program {
            program: "cargo".to_string(),
            args,
        });
    }

    let package_json = repo_root.join("package.json");
    if package_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&package_json) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(scripts) = parsed.get("scripts").and_then(|v| v.as_object()) {
                    let deps = parsed.get("dependencies").and_then(|v| v.as_object());
                    let dev_deps = parsed.get("devDependencies").and_then(|v| v.as_object());

                    for candidate in [
                        "typecheck",
                        "type-check",
                        "check:type",
                        "check:type:ts",
                        "check:type:js",
                        "check",
                        "check:lint",
                        "test:lint",
                        "lint",
                        "test:once",
                        "test",
                        "build",
                    ] {
                        let Some(script_value) = scripts.get(candidate) else {
                            continue;
                        };
                        let script_cmd = script_value.as_str().unwrap_or_default();
                        if should_skip_js_quick_check_script(
                            candidate, script_cmd, scripts, deps, dev_deps,
                        ) {
                            continue;
                        }
                        return Some(js_script_quick_check_command(repo_root, candidate));
                    }
                }
            }
        }
    }

    if repo_root.join("go.mod").exists() {
        return Some(QuickCheckCommand::Program {
            program: "go".to_string(),
            args: vec!["test".to_string(), "./...".to_string()],
        });
    }

    if repo_root.join("pyproject.toml").exists()
        || repo_root.join("requirements.txt").exists()
        || repo_root.join("setup.py").exists()
        || repo_root.join("setup.cfg").exists()
    {
        return Some(QuickCheckCommand::Program {
            // Prefer python3 for modern environments; fall back to python at runtime
            // if python3 isn't available.
            program: "python3".to_string(),
            args: vec![
                "-m".to_string(),
                "compileall".to_string(),
                "-q".to_string(),
                ".".to_string(),
            ],
        });
    }

    None
}

pub(super) fn should_skip_js_quick_check_script(
    script_name: &str,
    script_cmd: &str,
    scripts: &serde_json::Map<String, serde_json::Value>,
    deps: Option<&serde_json::Map<String, serde_json::Value>>,
    dev_deps: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    let cmd = script_cmd.to_ascii_lowercase();

    if script_name == "lint" || script_name == "check:lint" || script_name == "test:lint" {
        // Common footgun: lint script uses `eslint` but the repo doesn't include it as a
        // dependency. Running it will always fail and makes the harness look unreliable.
        if cmd.contains("eslint") && !has_js_dep("eslint", deps, dev_deps) {
            return true;
        }

        // Next.js v16 removed `next lint`. Some repos still carry a legacy `lint: next lint`
        // script, which fails with "Invalid project directory .../lint". Prefer other checks.
        if cmd.contains("next lint") {
            let next_major = js_dep_major_version("next", deps, dev_deps).unwrap_or(0);
            if next_major >= 16 {
                return true;
            }
        }
    }

    if script_name == "test" || script_name == "test:once" {
        let is_heavy_test = cmd.contains("run /^test:/")
            || cmd.contains("run /test:/")
            || cmd.contains("c8")
            || cmd.contains("nyc")
            || cmd.contains("--coverage")
            || cmd.contains("coverage")
            || cmd.contains("bnt");
        if is_heavy_test {
            let has_lighter = [
                "typecheck",
                "type-check",
                "check:type",
                "check:type:ts",
                "check:type:js",
                "check",
                "check:lint",
                "test:lint",
                "lint",
                "build",
            ]
            .iter()
            .any(|name| *name != script_name && scripts.contains_key(*name));
            if has_lighter {
                return true;
            }
        }
    }

    false
}

pub(super) fn has_js_dep(
    name: &str,
    deps: Option<&serde_json::Map<String, serde_json::Value>>,
    dev_deps: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    deps.map(|m| m.contains_key(name)).unwrap_or(false)
        || dev_deps.map(|m| m.contains_key(name)).unwrap_or(false)
}

pub(super) fn js_dep_major_version(
    name: &str,
    deps: Option<&serde_json::Map<String, serde_json::Value>>,
    dev_deps: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<u32> {
    let raw = deps
        .and_then(|m| m.get(name))
        .or_else(|| dev_deps.and_then(|m| m.get(name)))?
        .as_str()?;
    parse_major_version(raw)
}

pub(super) fn parse_major_version(raw: &str) -> Option<u32> {
    // Handles common semver-ish specifiers: "^16.1.1", "~16.0.0", ">=16", "16".
    let trimmed = raw.trim();
    let digits = trimmed
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u32>().ok()
}

pub(super) fn js_script_quick_check_command(repo_root: &Path, script: &str) -> QuickCheckCommand {
    if repo_root.join("pnpm-lock.yaml").exists() {
        return QuickCheckCommand::Program {
            program: "pnpm".to_string(),
            args: vec![script.to_string()],
        };
    }
    if repo_root.join("yarn.lock").exists() {
        return QuickCheckCommand::Program {
            program: "yarn".to_string(),
            args: vec![script.to_string()],
        };
    }
    if repo_root.join("bun.lockb").exists() || repo_root.join("bun.lock").exists() {
        return QuickCheckCommand::Program {
            program: "bun".to_string(),
            args: vec!["run".to_string(), script.to_string()],
        };
    }
    QuickCheckCommand::Program {
        program: "npm".to_string(),
        args: vec![
            "run".to_string(),
            script.to_string(),
            "--silent".to_string(),
        ],
    }
}

pub(super) fn command_to_string(command: &QuickCheckCommand) -> String {
    match command {
        QuickCheckCommand::Shell(cmd) => format!("sh -lc '{}'", cmd),
        QuickCheckCommand::Program { program, args } => {
            if args.is_empty() {
                program.clone()
            } else {
                format!("{} {}", program, args.join(" "))
            }
        }
    }
}

pub(super) fn read_package_json_script(repo_root: &Path, script_name: &str) -> Option<String> {
    let package_json = repo_root.join("package.json");
    let content = std::fs::read_to_string(package_json).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    parsed
        .get("scripts")
        .and_then(|v| v.as_object())
        .and_then(|scripts| scripts.get(script_name))
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

pub(super) fn invoked_js_script(command: &QuickCheckCommand) -> Option<String> {
    let QuickCheckCommand::Program { program, args } = command else {
        return None;
    };
    let program = program.to_ascii_lowercase();
    if program == "npm" || program == "bun" {
        if args.len() >= 2 && args[0] == "run" {
            return Some(args[1].clone());
        }
        return None;
    }
    if program == "pnpm" || program == "yarn" {
        return args.first().cloned();
    }
    None
}

pub(super) fn quick_check_requires_real_node_modules(
    repo_root: &Path,
    command: &QuickCheckCommand,
) -> bool {
    match command {
        QuickCheckCommand::Shell(cmd) => {
            let lower = cmd.to_ascii_lowercase();
            lower.contains("next build")
                || lower.contains("turbopack")
                || lower.contains("--turbo")
                // TypeScript type-checking can break with a symlinked node_modules across
                // worktrees (type identity splits across different real paths).
                || lower.contains("tsc")
                || lower.contains("typecheck")
                || lower.contains("type-check")
        }
        QuickCheckCommand::Program { program, args } => {
            let program = program.to_ascii_lowercase();
            if program == "next" {
                return args
                    .first()
                    .map(|arg| arg.eq_ignore_ascii_case("build"))
                    .unwrap_or(false);
            }

            let Some(script) = invoked_js_script(command) else {
                return false;
            };
            let script_lower = script.to_ascii_lowercase();
            if script_lower == "typecheck" || script_lower == "type-check" {
                return true;
            }
            let Some(script_cmd) = read_package_json_script(repo_root, &script) else {
                return false;
            };
            let lower = script_cmd.to_ascii_lowercase();
            lower.contains("next build")
                || lower.contains("turbopack")
                || lower.contains("--turbo")
                // TypeScript type-checking can break with a symlinked node_modules across
                // worktrees (type identity splits across different real paths).
                || lower.contains("tsc")
                || lower.contains("typecheck")
                || lower.contains("type-check")
        }
    }
}

pub(super) fn is_node_modules_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

pub(super) fn command_needs_node_modules(repo_root: &Path, command: &QuickCheckCommand) -> bool {
    if !repo_root.join("package.json").exists() {
        return false;
    }
    match command {
        QuickCheckCommand::Shell(cmd) => {
            let lower = cmd.to_ascii_lowercase();
            lower.contains("npm ")
                || lower.contains("pnpm ")
                || lower.contains("yarn ")
                || lower.contains("bun ")
                || lower.contains("npx ")
                || lower.contains("node ")
        }
        QuickCheckCommand::Program { program, .. } => {
            matches!(
                program.to_ascii_lowercase().as_str(),
                "npm" | "pnpm" | "yarn" | "bun" | "npx" | "node"
            )
        }
    }
}

pub(super) fn copy_node_modules_from_source(
    repo_root: &Path,
    source_node_modules: &Path,
    node_modules: &Path,
    notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    if node_modules.exists() {
        return Ok(());
    }

    if let Some(parent) = node_modules.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Ensure we are not copying into a symlink path.
    if let Ok(meta) = std::fs::symlink_metadata(node_modules) {
        if meta.is_dir() {
            let _ = std::fs::remove_dir_all(node_modules);
        } else {
            let _ = std::fs::remove_file(node_modules);
        }
    }

    let resolved_source = if is_node_modules_symlink(source_node_modules) {
        match std::fs::canonicalize(source_node_modules) {
            Ok(path) => {
                notes.push("resolved_source_node_modules_symlink".to_string());
                path
            }
            Err(_) => source_node_modules.to_path_buf(),
        }
    } else {
        source_node_modules.to_path_buf()
    };
    let src = resolved_source.to_string_lossy().to_string();
    let dst = node_modules.to_string_lossy().to_string();

    // Prefer copy-on-write / reflink where available, but fall back to a plain archive copy.
    // Note: This is best-effort and must not cause apply to mutate tracked state.
    let mut attempts: Vec<Vec<&str>> = Vec::new();
    if cfg!(target_os = "macos") {
        attempts.push(vec!["-c", "-a"]);
        attempts.push(vec!["-a"]);
    } else {
        attempts.push(vec!["-a", "--reflink=auto"]);
        attempts.push(vec!["-a"]);
    }

    for (idx, opts) in attempts.iter().enumerate() {
        let mut cmd = Command::new("cp");
        cmd.current_dir(repo_root).args(opts).arg(&src).arg(&dst);
        for (k, v) in SandboxSession::env_overrides() {
            cmd.env(k, v);
        }

        let output = run_command_with_timeout(&mut cmd, Duration::from_secs(90))
            .map_err(|e| anyhow::anyhow!("Failed to start cp to copy node_modules: {}", e))?;
        if output.timed_out {
            return Err(anyhow::anyhow!(
                "Timed out copying node_modules from {}",
                source_node_modules.display()
            ));
        }

        if output.status.map(|s| s.success()).unwrap_or(false) {
            notes.push("copied_node_modules_from_source".to_string());
            return Ok(());
        }

        let stderr = output.stderr.to_ascii_lowercase();
        let unknown_option = stderr.contains("illegal option")
            || stderr.contains("unrecognized option")
            || stderr.contains("unknown option")
            || stderr.contains("invalid option");
        if unknown_option && idx + 1 < attempts.len() {
            continue;
        }

        return Err(anyhow::anyhow!(
            "Failed to copy node_modules from {}: {}",
            source_node_modules.display(),
            truncate(&output.stderr, 240)
        ));
    }

    Err(anyhow::anyhow!(
        "Failed to copy node_modules from {}",
        source_node_modules.display()
    ))
}

pub(super) fn tail_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars()
        .skip(count.saturating_sub(max_chars))
        .collect::<String>()
}

pub(super) fn run_quick_checks(
    repo_root: &Path,
    source_repo_root: Option<&Path>,
    notes: &mut Vec<String>,
    mode: ImplementationQuickChecksMode,
    timeout_ms: u64,
) -> anyhow::Result<(
    ImplementationQuickCheckStatus,
    Option<String>,
    Option<ImplementationCommandOutcome>,
)> {
    if mode == ImplementationQuickChecksMode::Disabled {
        return Ok((ImplementationQuickCheckStatus::Unavailable, None, None));
    }

    let Some(command) = detect_quick_check_command(repo_root) else {
        return Ok((ImplementationQuickCheckStatus::Unavailable, None, None));
    };

    if let Err(err) = ensure_quick_check_prereqs(repo_root, source_repo_root, &command, notes) {
        notes.push(format!(
            "quick_check_prereq_failed: {}",
            truncate(&err.to_string(), 160)
        ));
    }

    // If this looks like a JS repo check but deps are missing (or unusable for this check),
    // treat quick checks as unavailable rather than failing the whole apply.
    if command_needs_node_modules(repo_root, &command) {
        let node_modules = repo_root.join("node_modules");
        if !node_modules.exists() {
            notes.push("quick_check_unavailable_missing_node_modules".to_string());
            return Ok((
                ImplementationQuickCheckStatus::Unavailable,
                Some(command_to_string(&command)),
                None,
            ));
        }
        if quick_check_requires_real_node_modules(repo_root, &command)
            && is_node_modules_symlink(&node_modules)
        {
            notes.push("quick_check_unavailable_symlinked_node_modules".to_string());
            return Ok((
                ImplementationQuickCheckStatus::Unavailable,
                Some(command_to_string(&command)),
                None,
            ));
        }
    }

    let python3_fallback_args = match &command {
        QuickCheckCommand::Program { program, args } if program == "python3" => Some(args.clone()),
        _ => None,
    };

    let mut command_str = command_to_string(&command);
    let mut cmd = match command {
        QuickCheckCommand::Shell(shell_cmd) => {
            let mut command = Command::new("sh");
            command.current_dir(repo_root).arg("-lc").arg(shell_cmd);
            command
        }
        QuickCheckCommand::Program { program, args } => {
            let mut command = Command::new(program);
            command.current_dir(repo_root).args(args);
            command
        }
    };
    for (k, v) in SandboxSession::env_overrides() {
        cmd.env(k, v);
    }

    let start = std::time::Instant::now();
    let (output, start) =
        match run_command_with_timeout(&mut cmd, Duration::from_millis(timeout_ms)) {
            Ok(output) => (output, start),
            Err(err) => {
                if let Some(args) = python3_fallback_args {
                    notes.push(format!(
                        "quick_check_failed_to_start: {} (retrying with python)",
                        truncate(&err, 160)
                    ));

                    let fallback_command = QuickCheckCommand::Program {
                        program: "python".to_string(),
                        args: args.clone(),
                    };
                    command_str = command_to_string(&fallback_command);

                    let mut fallback_cmd = Command::new("python");
                    fallback_cmd.current_dir(repo_root).args(args);
                    for (k, v) in SandboxSession::env_overrides() {
                        fallback_cmd.env(k, v);
                    }
                    let fallback_start = std::time::Instant::now();
                    match run_command_with_timeout(
                        &mut fallback_cmd,
                        Duration::from_millis(timeout_ms),
                    ) {
                        Ok(output) => (output, fallback_start),
                        Err(err) => {
                            notes.push(format!(
                                "quick_check_failed_to_start: {}",
                                truncate(&err, 160)
                            ));
                            return Ok((
                                ImplementationQuickCheckStatus::Unavailable,
                                Some(command_str),
                                None,
                            ));
                        }
                    }
                } else {
                    notes.push(format!(
                        "quick_check_failed_to_start: {}",
                        truncate(&err, 160)
                    ));
                    return Ok((
                        ImplementationQuickCheckStatus::Unavailable,
                        Some(command_str),
                        None,
                    ));
                }
            }
        };
    let outcome = ImplementationCommandOutcome {
        command: command_str.clone(),
        duration_ms: start.elapsed().as_millis() as u64,
        success: !output.timed_out && output.status.map(|s| s.success()).unwrap_or(false),
        timed_out: output.timed_out,
        exit_code: output.status.and_then(|s| s.code()),
        stdout_tail: tail_chars(&output.stdout, MAX_COMMAND_OUTPUT_TAIL_CHARS),
        stderr_tail: tail_chars(&output.stderr, MAX_COMMAND_OUTPUT_TAIL_CHARS),
    };
    let status = if outcome.success {
        ImplementationQuickCheckStatus::Passed
    } else {
        // Known sandbox limitation: Next/Turbopack rejects a symlinked `node_modules` root.
        // If we detect this, treat quick checks as unavailable (interactive can continue with
        // reduced confidence; lab/CI policies will still block).
        let stderr_lower = outcome.stderr_tail.to_ascii_lowercase();
        if stderr_lower.contains("symlink node_modules is invalid") {
            notes.push("quick_check_unavailable_next_symlink_rejected".to_string());
            ImplementationQuickCheckStatus::Unavailable
        } else {
            ImplementationQuickCheckStatus::Failed
        }
    };
    Ok((status, Some(command_str), Some(outcome)))
}

pub(super) fn is_prettier_formatting_failure(outcome: &ImplementationCommandOutcome) -> bool {
    let stderr = strip_ansi_sequences(&outcome.stderr_tail);
    let stdout = strip_ansi_sequences(&outcome.stdout_tail);
    let combined = format!("{}\n{}", stderr, stdout).to_ascii_lowercase();

    combined.contains("prettier")
        && (combined.contains("--write") || combined.contains("code style"))
}

pub(super) fn is_eslint_fixable_failure(outcome: &ImplementationCommandOutcome) -> bool {
    let stderr = strip_ansi_sequences(&outcome.stderr_tail);
    let stdout = strip_ansi_sequences(&outcome.stdout_tail);
    let combined = format!("{}\n{}", stderr, stdout).to_ascii_lowercase();

    combined.contains("eslint")
        && (combined.contains("potentially fixable with the `--fix` option")
            || combined.contains("potentially fixable with the --fix option"))
}

pub(super) fn run_prettier_write(
    repo_root: &Path,
    target: &Path,
    timeout_ms: u64,
) -> anyhow::Result<ImplementationCommandOutcome> {
    let prettier_candidates = [
        repo_root.join("node_modules").join(".bin").join("prettier"),
        repo_root
            .join("node_modules")
            .join(".bin")
            .join("prettier.cmd"),
    ];
    let prettier = prettier_candidates
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| anyhow::anyhow!("Prettier binary not found under node_modules/.bin"))?;

    let mut cmd = Command::new(prettier);
    cmd.current_dir(repo_root)
        .arg("--write")
        .arg(target)
        // Reduce output noise; we surface tails in diagnostics if it fails.
        .arg("--log-level")
        .arg("warn");
    for (k, v) in SandboxSession::env_overrides() {
        cmd.env(k, v);
    }

    let start = std::time::Instant::now();
    let output =
        run_command_with_timeout(&mut cmd, Duration::from_millis(timeout_ms)).map_err(|e| {
            anyhow::anyhow!("Failed to run prettier --write {}: {}", target.display(), e)
        })?;
    Ok(ImplementationCommandOutcome {
        command: format!("prettier --write {}", target.display()),
        duration_ms: start.elapsed().as_millis() as u64,
        success: !output.timed_out && output.status.map(|s| s.success()).unwrap_or(false),
        timed_out: output.timed_out,
        exit_code: output.status.and_then(|s| s.code()),
        stdout_tail: tail_chars(&output.stdout, MAX_COMMAND_OUTPUT_TAIL_CHARS),
        stderr_tail: tail_chars(&output.stderr, MAX_COMMAND_OUTPUT_TAIL_CHARS),
    })
}

pub(super) fn run_eslint_fix(
    repo_root: &Path,
    target: &Path,
    timeout_ms: u64,
) -> anyhow::Result<ImplementationCommandOutcome> {
    let eslint_candidates = [
        repo_root.join("node_modules").join(".bin").join("eslint"),
        repo_root
            .join("node_modules")
            .join(".bin")
            .join("eslint.cmd"),
    ];
    let eslint = eslint_candidates
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| anyhow::anyhow!("ESLint binary not found under node_modules/.bin"))?;

    let mut cmd = Command::new(eslint);
    cmd.current_dir(repo_root).arg("--fix").arg(target);
    for (k, v) in SandboxSession::env_overrides() {
        cmd.env(k, v);
    }

    let start = std::time::Instant::now();
    let output = run_command_with_timeout(&mut cmd, Duration::from_millis(timeout_ms))
        .map_err(|e| anyhow::anyhow!("Failed to run eslint --fix {}: {}", target.display(), e))?;
    Ok(ImplementationCommandOutcome {
        command: format!("eslint --fix {}", target.display()),
        duration_ms: start.elapsed().as_millis() as u64,
        success: !output.timed_out && output.status.map(|s| s.success()).unwrap_or(false),
        timed_out: output.timed_out,
        exit_code: output.status.and_then(|s| s.code()),
        stdout_tail: tail_chars(&output.stdout, MAX_COMMAND_OUTPUT_TAIL_CHARS),
        stderr_tail: tail_chars(&output.stderr, MAX_COMMAND_OUTPUT_TAIL_CHARS),
    })
}

pub(super) fn ensure_quick_check_prereqs(
    repo_root: &Path,
    source_repo_root: Option<&Path>,
    command: &QuickCheckCommand,
    notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    // Most common failure in worktree sandboxes: JS deps are installed in the outer sandbox
    // but not present in nested attempt worktrees, so `pnpm type-check` fails immediately.
    // We keep this as a best-effort prereq step: it must not mutate repo-tracked state.
    let needs_js = repo_root.join("package.json").exists()
        && matches!(
            command,
            QuickCheckCommand::Shell(_) | QuickCheckCommand::Program { .. }
        );
    if !needs_js {
        return Ok(());
    }

    ensure_node_modules_present(repo_root, source_repo_root, command, notes)?;
    Ok(())
}

pub(super) fn ensure_node_modules_present(
    repo_root: &Path,
    source_repo_root: Option<&Path>,
    command: &QuickCheckCommand,
    notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    let node_modules = repo_root.join("node_modules");
    let needs_real_node_modules = quick_check_requires_real_node_modules(repo_root, command);

    if node_modules.exists() {
        if needs_real_node_modules && is_node_modules_symlink(&node_modules) {
            // Replace the symlink with a real directory so Next/Turbopack can run.
            let _ = std::fs::remove_file(&node_modules);
            let _ = std::fs::remove_dir_all(&node_modules);
        } else {
            return Ok(());
        }
    }

    let Some(source_root) = source_repo_root else {
        notes.push("node_modules_missing_no_source".to_string());
        return Ok(());
    };

    let source_node_modules = source_root.join("node_modules");
    if !source_node_modules.exists() {
        notes.push("node_modules_missing_in_source".to_string());
        return Ok(());
    }

    // Some tooling (notably Next/Turbopack) rejects a symlinked `node_modules` root.
    // Prefer a real directory for those checks.
    if needs_real_node_modules {
        match copy_node_modules_from_source(repo_root, &source_node_modules, &node_modules, notes) {
            Ok(()) => return Ok(()),
            Err(err) => {
                notes.push(format!(
                    "node_modules_copy_failed: {}",
                    truncate(&err.to_string(), 180)
                ));
                return Ok(());
            }
        }
    }

    // Default: create a symlink so quick checks use the already-installed dependencies from the source.
    // This preserves harness timing budgets and avoids re-installing packages per attempt.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if node_modules.exists() {
            return Ok(());
        }
        // If a broken path exists, clear it.
        if let Ok(meta) = std::fs::symlink_metadata(&node_modules) {
            if meta.file_type().is_symlink() || meta.is_dir() || meta.is_file() {
                let _ = std::fs::remove_file(&node_modules);
                let _ = std::fs::remove_dir_all(&node_modules);
            }
        }
        symlink(&source_node_modules, &node_modules).map_err(|e| {
            anyhow::anyhow!(
                "Failed to symlink node_modules from {}: {}",
                source_node_modules.display(),
                e
            )
        })?;
        notes.push("linked_node_modules_from_source".to_string());
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = source_root; // avoid unused warnings on windows builds
        notes.push("node_modules_missing_on_non_unix".to_string());
        Ok(())
    }
}
