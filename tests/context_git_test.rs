use open_agent_sdk::context::{
    collect_git_status, is_git_work_tree, GitContextError, GIT_CONTEXT_BUDGET,
};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

static PATH_LOCK: Mutex<()> = Mutex::new(());

struct PathGuard {
    previous: Option<String>,
}

impl PathGuard {
    fn prepend(dir: &Path) -> Self {
        let previous = std::env::var("PATH").ok();
        let next = if let Some(path) = &previous {
            let sep = if cfg!(windows) { ";" } else { ":" };
            format!("{}{sep}{path}", dir.display())
        } else {
            dir.display().to_string()
        };
        std::env::set_var("PATH", &next);
        Self { previous }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
}

#[test]
fn git_context_budget_is_five_seconds() {
    assert_eq!(GIT_CONTEXT_BUDGET, Duration::from_secs(5));
}

#[test]
fn non_git_directory_is_not_a_work_tree() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!is_git_work_tree(dir.path()));
}

#[test]
fn git_file_or_dir_marks_work_tree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    assert!(is_git_work_tree(dir.path()));
}

#[tokio::test]
async fn non_git_cwd_skips_git_even_if_path_git_hangs() {
    let _lock = PATH_LOCK.lock().unwrap();
    let stub_dir = tempfile::tempdir().unwrap();
    write_hanging_git(stub_dir.path(), None);
    let _guard = PathGuard::prepend(stub_dir.path());

    let cwd = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let result = collect_git_status(
        cwd.path().to_str().unwrap(),
        &CancellationToken::new(),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(result.is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[tokio::test]
async fn git_timeout_kills_hanging_child() {
    let _lock = PATH_LOCK.lock().unwrap();
    let stub_dir = tempfile::tempdir().unwrap();
    let pid_path = stub_dir.path().join("git.pid");
    write_hanging_git(stub_dir.path(), Some(&pid_path));
    let _guard = PathGuard::prepend(stub_dir.path());

    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir(cwd.path().join(".git")).unwrap();

    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let task = tokio::spawn(async move {
        collect_git_status(&cwd_str, &CancellationToken::new(), Duration::from_secs(2)).await
    });
    for _ in 0..40 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, GitContextError::Timeout(_)));

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("hanging git should record its pid before timeout")
        .trim()
        .parse()
        .unwrap();
    let mut alive = true;
    for _ in 0..20 {
        alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!alive, "hanging git child {pid} survived timeout");
}

#[cfg(unix)]
#[tokio::test]
async fn git_cancel_kills_hanging_child() {
    let _lock = PATH_LOCK.lock().unwrap();
    let stub_dir = tempfile::tempdir().unwrap();
    let pid_path = stub_dir.path().join("git.pid");
    write_hanging_git(stub_dir.path(), Some(&pid_path));
    let _guard = PathGuard::prepend(stub_dir.path());

    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir(cwd.path().join(".git")).unwrap();
    let abort = CancellationToken::new();
    let abort_for_task = abort.clone();
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let task = tokio::spawn(async move {
        collect_git_status(&cwd_str, &abort_for_task, Duration::from_secs(5)).await
    });

    for _ in 0..40 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    abort.cancel();
    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, GitContextError::Cancelled));
}

fn write_hanging_git(dir: &Path, pid_path: Option<&Path>) {
    let script = dir.join("git");
    let body = if let Some(pid_path) = pid_path {
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            pid_path.display()
        )
    } else {
        "#!/bin/sh\nexec sleep 30\n".to_string()
    };
    std::fs::write(&script, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
