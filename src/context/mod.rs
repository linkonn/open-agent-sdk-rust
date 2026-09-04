use crate::types::{CacheControl, SystemBlock};
use std::path::Path;
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub const GIT_CONTEXT_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum GitContextError {
    #[error("git context timed out after {0}s")]
    Timeout(u64),
    #[error("git context cancelled")]
    Cancelled,
    #[error("failed to run git: {0}")]
    Process(String),
}

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("{0}")]
    Git(#[from] GitContextError),
}

pub fn is_git_work_tree(cwd: &Path) -> bool {
    let mut current = cwd.to_path_buf();
    loop {
        let git = current.join(".git");
        if git.is_dir() || git.is_file() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

async fn kill_git_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: the child was spawned into its own process group.
            let result = unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    tracing::warn!(pid, %error, "failed to kill git process group");
                }
            }
        }
    }

    #[cfg(windows)]
    {
        if let Some(pid) = child.id() {
            let mut kill_cmd = Command::new("taskkill");
            kill_cmd.args(["/F", "/T", "/PID", &pid.to_string()]);
            kill_cmd.creation_flags(0x08000000);
            match kill_cmd.output().await {
                Ok(output) if !output.status.success() => {
                    tracing::warn!(pid, status = ?output.status, "taskkill failed for git");
                }
                Err(error) => tracing::warn!(pid, %error, "failed to start taskkill for git"),
                _ => {}
            }
        }
    }

    let _ = child.start_kill();
    if let Err(error) = child.wait().await {
        tracing::debug!(%error, "git child already stopped before wait");
    }
}

fn remaining_budget(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

struct GitOutput {
    success: bool,
    stdout: String,
}

async fn run_git(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
    abort: &CancellationToken,
) -> Result<GitOutput, GitContextError> {
    if abort.is_cancelled() {
        return Err(GitContextError::Cancelled);
    }
    if timeout.is_zero() {
        return Err(GitContextError::Timeout(GIT_CONTEXT_BUDGET.as_secs()));
    }

    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }

    let mut child = cmd.spawn().map_err(|error| GitContextError::Process(error.to_string()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitContextError::Process("git stdout pipe missing".to_string()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitContextError::Process("git stderr pipe missing".to_string()))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await.map(|_| buf)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await.map(|_| buf)
    });

    let status = tokio::select! {
        status = child.wait() => {
            status.map_err(|error| GitContextError::Process(error.to_string()))?
        }
        _ = tokio::time::sleep(timeout) => {
            kill_git_child(&mut child).await;
            return Err(GitContextError::Timeout(GIT_CONTEXT_BUDGET.as_secs()));
        }
        _ = abort.cancelled() => {
            kill_git_child(&mut child).await;
            return Err(GitContextError::Cancelled);
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|error| GitContextError::Process(error.to_string()))?
        .map_err(|error| GitContextError::Process(error.to_string()))?;
    let stderr = stderr_task
        .await
        .map_err(|error| GitContextError::Process(error.to_string()))?
        .map_err(|error| GitContextError::Process(error.to_string()))?;

    let stdout_text = String::from_utf8_lossy(&stdout).trim().to_string();
    let stderr_text = String::from_utf8_lossy(&stderr).trim().to_string();
    let lowered = stderr_text.to_ascii_lowercase();
    if !status.success()
        && (lowered.contains("permission denied") || lowered.contains("not a git repository"))
    {
        return Err(GitContextError::Process(if stderr_text.is_empty() {
            format!("git {:?} failed with status {status}", args)
        } else {
            stderr_text
        }));
    }

    Ok(GitOutput {
        success: status.success(),
        stdout: stdout_text,
    })
}

async fn detect_main_branch(
    cwd: &Path,
    deadline: Instant,
    abort: &CancellationToken,
) -> Result<String, GitContextError> {
    for branch in ["main", "master"] {
        match run_git(
            cwd,
            &["rev-parse", "--verify", branch],
            remaining_budget(deadline),
            abort,
        )
        .await
        {
            Ok(output) if output.success => return Ok(branch.to_string()),
            Ok(_) => {}
            Err(GitContextError::Timeout(secs)) => return Err(GitContextError::Timeout(secs)),
            Err(GitContextError::Cancelled) => return Err(GitContextError::Cancelled),
            Err(GitContextError::Process(_)) => {}
        }
    }
    Ok("main".to_string())
}

pub async fn collect_git_status(
    cwd: &str,
    abort: &CancellationToken,
    budget: Duration,
) -> Result<Option<String>, GitContextError> {
    let cwd_path = Path::new(cwd);
    if cwd.is_empty() || !is_git_work_tree(cwd_path) {
        return Ok(None);
    }

    let deadline = Instant::now() + budget;
    let mut result = String::new();

    let branch = run_git(
        cwd_path,
        &["branch", "--show-current"],
        remaining_budget(deadline),
        abort,
    )
    .await?;
    if !branch.stdout.is_empty() {
        result.push_str(&format!("Current branch: {}\n", branch.stdout));
    }

    let main_branch = detect_main_branch(cwd_path, deadline, abort).await?;
    result.push_str(&format!("\nMain branch: {}\n", main_branch));

    let user = run_git(
        cwd_path,
        &["config", "user.name"],
        remaining_budget(deadline),
        abort,
    )
    .await?;
    if !user.stdout.is_empty() {
        result.push_str(&format!("\nGit user: {}\n", user.stdout));
    }

    let status = run_git(
        cwd_path,
        &["status", "--short"],
        remaining_budget(deadline),
        abort,
    )
    .await?;
    if status.stdout.is_empty() {
        result.push_str("\nStatus:\n(clean)\n");
    } else {
        result.push_str(&format!("\nStatus:\n{}\n", status.stdout));
    }

    let log = run_git(
        cwd_path,
        &["log", "--oneline", "-5"],
        remaining_budget(deadline),
        abort,
    )
    .await?;
    if !log.stdout.is_empty() {
        result.push_str(&format!("\nRecent commits:\n{}\n", log.stdout));
    }

    Ok(Some(result))
}

/// Build system prompt blocks with context injection.
pub async fn build_system_blocks(
    cwd: &str,
    custom_system_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
    skills_summary: Option<&str>,
    abort: &CancellationToken,
) -> Result<Vec<SystemBlock>, ContextError> {
    build_system_blocks_with_git_budget(
        cwd,
        custom_system_prompt,
        append_system_prompt,
        skills_summary,
        abort,
        GIT_CONTEXT_BUDGET,
    )
    .await
}

pub async fn build_system_blocks_with_git_budget(
    cwd: &str,
    custom_system_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
    skills_summary: Option<&str>,
    abort: &CancellationToken,
    git_budget: Duration,
) -> Result<Vec<SystemBlock>, ContextError> {
    let mut blocks = Vec::new();

    let system_prompt = if let Some(custom) = custom_system_prompt {
        custom.to_string()
    } else {
        default_system_prompt()
    };

    blocks.push(SystemBlock {
        block_type: "text".to_string(),
        text: system_prompt,
        cache_control: Some(CacheControl::ephemeral()),
    });

    if let Some(git_status) = collect_git_status(cwd, abort, git_budget).await? {
        if !git_status.is_empty() {
            blocks.push(SystemBlock {
                block_type: "text".to_string(),
                text: format!("gitStatus: {}", git_status),
                cache_control: None,
            });
        }
    }

    let user_context = get_user_context(cwd);
    if !user_context.is_empty() {
        blocks.push(SystemBlock {
            block_type: "text".to_string(),
            text: user_context,
            cache_control: Some(CacheControl::ephemeral()),
        });
    }

    if let Some(skills) = skills_summary {
        if !skills.is_empty() {
            blocks.push(SystemBlock {
                block_type: "text".to_string(),
                text: skills.to_string(),
                cache_control: Some(CacheControl::ephemeral()),
            });
        }
    }

    if let Some(append) = append_system_prompt {
        blocks.push(SystemBlock {
            block_type: "text".to_string(),
            text: append.to_string(),
            cache_control: None,
        });
    }

    Ok(blocks)
}

fn default_system_prompt() -> String {
    "You are a helpful AI assistant with access to tools for software engineering tasks. \
     Use the available tools to help the user accomplish their goals. \
     Be concise and direct in your responses."
        .to_string()
}

fn get_user_context(cwd: &str) -> String {
    let mut context = String::new();

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    context.push_str(&format!("Current date: {}\n", date));

    for filename in &["AGENT.md", "CLAUDE.md", ".agent/AGENT.md"] {
        let path = std::path::Path::new(cwd).join(filename);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                context.push_str(&format!(
                    "\n# Project context from {}\n{}\n",
                    filename, content
                ));
            }
        }
    }

    context
}

/// Clear the cached git status.
pub fn clear_context_cache() {}
