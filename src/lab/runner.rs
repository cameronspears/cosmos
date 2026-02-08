use crate::cache::SelfIterationCommandOutcome;
use crate::util::run_command_with_timeout;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const OUTPUT_TAIL_MAX_CHARS: usize = 8_000;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    pub fn new(name: impl Into<String>, cwd: impl AsRef<Path>, program: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.as_ref().to_path_buf(),
            timeout: Duration::from_secs(600),
            env: Vec::new(),
        }
    }

    pub fn args(mut self, args: &[&str]) -> Self {
        self.args = args.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    pub fn with_env_overrides(mut self, env: &[(String, String)]) -> Self {
        self.env.extend(env.iter().cloned());
        self
    }
}

pub fn run_command(spec: &CommandSpec) -> SelfIterationCommandOutcome {
    let mut command = Command::new(&spec.program);
    command.current_dir(&spec.cwd).args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    let start = Instant::now();
    let command_label = format!(
        "{} {}",
        spec.program,
        spec.args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    )
    .trim()
    .to_string();

    match run_command_with_timeout(&mut command, spec.timeout) {
        Ok(result) => {
            let success = !result.timed_out && result.status.map(|s| s.success()).unwrap_or(false);
            SelfIterationCommandOutcome {
                name: spec.name.clone(),
                command: command_label,
                cwd: spec.cwd.clone(),
                duration_ms: start.elapsed().as_millis() as u64,
                success,
                exit_code: result.status.and_then(|s| s.code()),
                timed_out: result.timed_out,
                stdout_tail: tail_chars(&result.stdout, OUTPUT_TAIL_MAX_CHARS),
                stderr_tail: tail_chars(&result.stderr, OUTPUT_TAIL_MAX_CHARS),
                note: None,
            }
        }
        Err(error) => SelfIterationCommandOutcome {
            name: spec.name.clone(),
            command: command_label,
            cwd: spec.cwd.clone(),
            duration_ms: start.elapsed().as_millis() as u64,
            success: false,
            exit_code: None,
            timed_out: false,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            note: Some(error),
        },
    }
}

pub fn run_commands(specs: &[CommandSpec]) -> Vec<SelfIterationCommandOutcome> {
    specs.iter().map(run_command).collect()
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    text.chars().skip(total - max_chars).collect::<String>()
}
