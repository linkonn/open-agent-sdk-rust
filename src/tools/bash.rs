use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::process::Stdio;
use tokio::process::Command;

use crate::tools::command_runner;
use crate::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_SIZE: usize = 100_000;

/// Destructive command patterns that should be flagged.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf .",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "chmod 777",
    "chmod -R 777",
    "> /dev/sda",
    "mkfs.",
    "dd if=",
    ":(){ :|:& };:",
];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Executes a given bash command and returns its output. Use for system commands and terminal operations that require shell execution. Long-running commands will periodically report partial output."
    }

    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "command".to_string(),
                    json!({
                        "type": "string",
                        "description": "The command to execute"
                    }),
                ),
                (
                    "timeout".to_string(),
                    json!({
                        "type": "number",
                        "description": "Optional timeout in milliseconds (max 600000)"
                    }),
                ),
                (
                    "description".to_string(),
                    json!({
                        "type": "string",
                        "description": "Clear description of what this command does"
                    }),
                ),
            ]),
            required: vec!["command".to_string()],
            additional_properties: Some(false),
        }
    }

    fn is_read_only(&self, input: &Value) -> bool {
        let command = input.get("command").and_then(|c| c.as_str()).unwrap_or("");

        // Check if command starts with a read-only command
        let cmd_trimmed = command.trim();
        let first_cmd = cmd_trimmed.split_whitespace().next().unwrap_or("");
        let single_word_reads = [
            "ls", "cat", "head", "tail", "find", "grep", "rg", "wc", "pwd", "echo", "which",
            "type", "file", "stat", "du", "df",
        ];
        let prefix_reads = [
            "git status",
            "git log",
            "git diff",
            "git show",
            "git branch",
            "cargo check",
            "cargo test --no-run",
            "rustc --version",
        ];

        single_word_reads.contains(&first_cmd)
            || prefix_reads.iter().any(|p| cmd_trimmed.starts_with(p))
    }

    async fn call(&self, input: Value, context: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| ToolError::InvalidInput("Missing 'command' field".to_string()))?;

        // Security check
        if let Some(warning) = check_destructive(command) {
            return Ok(ToolResult::error(format!(
                "Potentially destructive command detected: {}. Proceed with caution.",
                warning
            )));
        }

        // Build shell command
        let shell = build_shell_runner(command, context.shell_binary.as_deref());
        tracing::debug!(program = %shell.program, "Bash tool resolved shell");
        let mut cmd = Command::new(&shell.program);
        cmd.args(&shell.args).current_dir(&context.working_dir);
        let shell_path = crate::mcp::shell_path::get_shell_path();
        if !shell_path.is_empty() {
            cmd.env("PATH", shell_path);
        }
        let description = input.get("description").and_then(|d| d.as_str());
        let timeout_ms = input
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        let event_context = crate::tools::executor::current_tool_event_context();

        let output = command_runner::run_command(
            &mut cmd,
            &context.abort_signal,
            command_runner::CommandRunOptions {
                timeout: Some(std::time::Duration::from_millis(timeout_ms)),
                event_sender: event_context.as_ref().map(|(sender, _)| sender),
                tool_name: "Bash",
                description,
                tool_use_id: event_context
                    .as_ref()
                    .map(|(_, tool_use_id)| tool_use_id.as_str()),
            },
        )
        .await;

        match output {
            Ok(out) => {
                let mut result = String::new();

                if !out.stdout.is_empty() {
                    result.push_str(&out.stdout);
                }
                if !out.stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str("STDERR:\n");
                    result.push_str(&out.stderr);
                }

                if result.len() > MAX_OUTPUT_SIZE {
                    truncate_utf8(&mut result, MAX_OUTPUT_SIZE);
                    result.push_str("\n... (output truncated)");
                }

                if out.exit_code != 0 {
                    result.push_str(&format!("\n\nExit code: {}", out.exit_code));
                }

                if result.is_empty() {
                    result = "(no output)".to_string();
                }

                Ok(if out.exit_code != 0 {
                    ToolResult::error(result)
                } else {
                    ToolResult::text(result)
                })
            }
            Err(e) => Ok(ToolResult::error(format!("Command failed: {}", e))),
        }
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }

    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellRunner {
    program: String,
    args: Vec<String>,
}

fn build_shell_runner(command: &str, shell_override: Option<&str>) -> ShellRunner {
    if let Some(runner) =
        try_shell_override(command, shell_override, |path| Path::new(path).is_file())
    {
        return runner;
    }
    select_shell_runner(
        command,
        cfg!(windows),
        std::env::var("ComSpec").ok(),
        resolve_programs,
    )
}

const PROBE_COMMAND: &str = "echo __aqbot_shell_ok__";
const PROBE_MARKER: &str = "__aqbot_shell_ok__";
/// 自检日志里 stderr 最多保留的字符数，WSL 的报错会带一大段提示。
const MAX_PROBE_STDERR: usize = 600;

/// 一次 shell 冒烟检查的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProbe {
    /// 实际选中的 shell 可执行文件（完整路径）。
    pub program: String,
    /// 命令是否成功执行并输出了预期标记。
    pub success: bool,
    /// 一句话结论，方便直接读日志。
    pub detail: String,
    pub stdout: String,
    pub stderr: String,
}

/// 用真正会选中的那个 shell 跑一条无害命令，验证 Bash 工具在本机能不能用。
///
/// 走的是和 [`BashTool::call`] 完全相同的 shell 选择与 PATH 注入逻辑，
/// 所以这里失败就等于工具调用一定失败。函数是同步阻塞的，调用方应放到阻塞任务里。
pub fn probe_shell(shell_override: Option<&str>) -> ShellProbe {
    let runner = build_shell_runner(PROBE_COMMAND, shell_override);

    let mut command = StdCommand::new(&runner.program);
    command.args(&runner.args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let shell_path = crate::mcp::shell_path::get_shell_path();
    if !shell_path.is_empty() {
        command.env("PATH", shell_path);
    }

    match command.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = truncate_text(
                String::from_utf8_lossy(&output.stderr).trim(),
                MAX_PROBE_STDERR,
            );
            let success = output.status.success() && stdout.contains(PROBE_MARKER);
            let detail = if success {
                "echo round-trip ok".to_string()
            } else if !output.status.success() {
                match output.status.code() {
                    Some(code) => format!("shell exited with code {code}"),
                    None => "shell was terminated by a signal".to_string(),
                }
            } else {
                "shell ran but produced unexpected output".to_string()
            };

            ShellProbe {
                program: runner.program,
                success,
                detail,
                stdout,
                stderr,
            }
        }
        Err(error) => ShellProbe {
            program: runner.program,
            success: false,
            detail: format!("could not launch shell: {error}"),
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

/// 日志里不需要整段报错，超长部分截断并标注。
fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated: String = value.chars().take(max_chars).collect();
    truncated.push_str("… (truncated)");
    truncated
}

/// Prefer a configured shell binary when the path exists; otherwise warn and return None.
fn try_shell_override(
    command: &str,
    shell_override: Option<&str>,
    exists: impl FnOnce(&str) -> bool,
) -> Option<ShellRunner> {
    let shell = shell_override.filter(|s| !s.is_empty())?;
    if exists(shell) && !is_wsl_bash_launcher(Path::new(shell)) {
        return Some(ShellRunner {
            program: shell.to_string(),
            args: vec!["-c".to_string(), command.to_string()],
        });
    }
    tracing::warn!(
        shell = %shell,
        "Configured shell binary is missing or is a WSL launcher; falling back to PATH auto-detection"
    );
    None
}

/// Windows 上 `bash.exe` 在 `System32` 与 `WindowsApps` 下各有一份，两个都是 WSL 的入口，
/// 不是真正的 bash。机器没装 WSL 时执行会直接返回 `HCS_E_SERVICE_NOT_AVAILABLE`，
/// 表现为 Bash 工具完全不能用，所以选 shell 时必须把它们跳过。
fn is_wsl_bash_launcher(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.eq_ignore_ascii_case("bash.exe") {
        return false;
    }
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    normalized.contains("\\windows\\system32\\") || normalized.contains("\\windowsapps\\")
}

fn select_shell_runner<F>(
    command: &str,
    is_windows: bool,
    comspec: Option<String>,
    mut resolve: F,
) -> ShellRunner
where
    F: FnMut(&str) -> Vec<PathBuf>,
{
    if is_windows {
        // Git for Windows 装好后 `bash.exe` 通常在 PATH 里，但 System32 的 WSL 入口往往排在更前面，
        // 所以这里按候选逐个找"非 WSL 入口"的那一个，再退化到固定的 Git 安装位置。
        for candidate in [
            "bash.exe",
            "bash",
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ] {
            for path in resolve(candidate) {
                if is_wsl_bash_launcher(&path) {
                    tracing::warn!(
                        shell = %path.display(),
                        "Skipping WSL bash launcher; it fails when WSL is not installed"
                    );
                    continue;
                }
                return ShellRunner {
                    program: path.to_string_lossy().into_owned(),
                    args: vec!["-c".to_string(), command.to_string()],
                };
            }
        }

        for candidate in ["pwsh.exe", "pwsh", "powershell.exe", "powershell"] {
            if let Some(path) = resolve(candidate).into_iter().next() {
                return ShellRunner {
                    program: path.to_string_lossy().into_owned(),
                    args: vec![
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-Command".to_string(),
                        command.to_string(),
                    ],
                };
            }
        }

        return ShellRunner {
            program: comspec
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "cmd.exe".to_string()),
            args: vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                command.to_string(),
            ],
        };
    }

    for candidate in ["/bin/bash", "bash", "/bin/sh", "sh"] {
        if let Some(path) = resolve(candidate).into_iter().next() {
            return ShellRunner {
                program: path.to_string_lossy().into_owned(),
                args: vec!["-c".to_string(), command.to_string()],
            };
        }
    }

    ShellRunner {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), command.to_string()],
    }
}

/// 解析 PATH 中所有匹配的可执行文件，保持 PATH 顺序。
fn resolve_programs(candidate: &str) -> Vec<PathBuf> {
    if candidate.contains(std::path::MAIN_SEPARATOR)
        || candidate.contains('/')
        || candidate.contains('\\')
    {
        let path = PathBuf::from(candidate);
        return if path.is_file() { vec![path] } else { Vec::new() };
    }

    let Some(paths) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for directory in std::env::split_paths(&paths) {
        let direct = directory.join(candidate);
        if direct.is_file() {
            found.push(direct);
            continue;
        }

        #[cfg(windows)]
        {
            let with_extension = directory.join(format!("{candidate}.exe"));
            if with_extension.is_file() {
                found.push(with_extension);
            }
        }
    }
    found
}

fn check_destructive(command: &str) -> Option<&'static str> {
    for pattern in DESTRUCTIVE_PATTERNS {
        if command.contains(pattern) {
            return Some(pattern);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{select_shell_runner, truncate_utf8, try_shell_override, ShellRunner};
    use std::path::{Path, PathBuf};

    #[test]
    fn output_truncation_preserves_utf8_boundaries() {
        let mut output = "你".repeat(40_000);

        truncate_utf8(&mut output, 100_000);

        assert!(output.len() <= 100_000);
        assert!(output.ends_with('你'));
    }

    #[test]
    fn shell_override_uses_configured_binary_when_it_exists() {
        let runner = try_shell_override("node -v", Some("/custom/bash"), |_| true)
            .expect("override should apply");

        assert_eq!(
            runner,
            ShellRunner {
                program: "/custom/bash".to_string(),
                args: vec!["-c".to_string(), "node -v".to_string()],
            }
        );
    }

    #[test]
    fn shell_override_falls_back_when_binary_missing() {
        assert!(try_shell_override("node -v", Some("/missing/bash"), |_| false).is_none());
    }

    #[test]
    fn shell_override_ignores_empty_string() {
        assert!(try_shell_override("node -v", Some(""), |_| true).is_none());
        assert!(try_shell_override("node -v", None, |_| true).is_none());
    }

    #[test]
    fn select_shell_runner_falls_back_to_powershell_on_windows() {
        let runner = select_shell_runner("node -v", true, None, |candidate| {
            if candidate == "pwsh.exe" {
                vec![PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")]
            } else {
                Vec::new()
            }
        });

        assert_eq!(
            runner,
            ShellRunner {
                program: r"C:\Program Files\PowerShell\7\pwsh.exe".to_string(),
                args: vec![
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    "node -v".to_string(),
                ],
            }
        );
    }

    #[test]
    fn windows_skips_wsl_bash_launcher_and_uses_git_bash() {
        let runner = select_shell_runner("echo hi", true, None, |candidate| match candidate {
            "bash.exe" => vec![
                PathBuf::from(r"C:\Windows\System32\bash.exe"),
                PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
            ],
            _ => Vec::new(),
        });

        assert_eq!(
            runner,
            ShellRunner {
                program: r"C:\Program Files\Git\bin\bash.exe".to_string(),
                args: vec!["-c".to_string(), "echo hi".to_string()],
            }
        );
    }

    #[test]
    fn windows_skips_wsl_bash_launcher_and_falls_back_to_powershell() {
        let runner = select_shell_runner("echo hi", true, None, |candidate| match candidate {
            "bash.exe" => vec![PathBuf::from(
                r"C:\Users\ceef\AppData\Local\Microsoft\WindowsApps\bash.exe",
            )],
            "powershell.exe" => vec![PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")],
            _ => Vec::new(),
        });

        assert_eq!(
            runner,
            ShellRunner {
                program: r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".to_string(),
                args: vec![
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    "echo hi".to_string(),
                ],
            }
        );
    }

    #[test]
    fn wsl_launcher_detection_ignores_real_bash() {
        assert!(super::is_wsl_bash_launcher(Path::new(
            r"C:\Windows\System32\bash.exe"
        )));
        assert!(super::is_wsl_bash_launcher(Path::new(
            r"C:\Users\ceef\AppData\Local\Microsoft\WindowsApps\bash.exe"
        )));
        assert!(!super::is_wsl_bash_launcher(Path::new(
            r"C:\Program Files\Git\bin\bash.exe"
        )));
        assert!(!super::is_wsl_bash_launcher(Path::new("/bin/bash")));
    }

    #[test]
    fn select_shell_runner_falls_back_to_cmd_on_windows() {
        let runner = select_shell_runner(
            "node -v",
            true,
            Some("C:\\Windows\\System32\\cmd.exe".to_string()),
            |_| Vec::new(),
        );

        assert_eq!(
            runner,
            ShellRunner {
                program: "C:\\Windows\\System32\\cmd.exe".to_string(),
                args: vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "node -v".to_string(),
                ],
            }
        );
    }

    #[test]
    fn select_shell_runner_prefers_bash_on_unix() {
        let runner = select_shell_runner("node -v", false, None, |candidate| {
            if candidate == "/bin/bash" {
                vec![PathBuf::from("/bin/bash")]
            } else {
                Vec::new()
            }
        });

        assert_eq!(
            runner,
            ShellRunner {
                program: "/bin/bash".to_string(),
                args: vec!["-c".to_string(), "node -v".to_string()],
            }
        );
    }
}
