use std::fs;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::Duration;

use color_eyre::eyre::WrapErr;

use crate::cli::Cli;
use crate::paths::AppPaths;

use super::logging::json_log_path;

pub fn start_service() -> color_eyre::Result<()> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    paths
        .ensure_runtime_dirs()
        .wrap_err("failed to create runtime state directories")?;
    let pid_path = paths.pid_path;
    let log_path = paths.log_path;
    let json_log_path = json_log_path(&log_path);

    if let Some(pid) = read_pid(&pid_path)? {
        if is_process_running(pid) {
            println!("service already running (pid {pid})");
            return Ok(());
        }
        fs::remove_file(&pid_path).wrap_err("failed to remove stale pid file")?;
    }

    let exe = std::env::current_exe().wrap_err("failed to resolve current executable path")?;
    let mut child = ProcessCommand::new(exe);
    child
        .arg("system")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = child
        .spawn()
        .wrap_err("failed to launch background service")?;
    let pid = child.id();
    fs::write(&pid_path, format!("{pid}\n")).wrap_err("failed to write pid file")?;

    println!("service started (pid {pid})");
    println!("pid file: {}", pid_path.display());
    println!("log file: {}", log_path.display());
    println!("json log file: {}", json_log_path.display());
    Ok(())
}

pub fn stop_service(cli: &Cli) -> color_eyre::Result<()> {
    let pid_path = state_paths(cli)?.pid_path;
    let Some(pid) = read_pid(&pid_path)? else {
        println!("service is not running");
        return Ok(());
    };

    if !is_process_running(pid) {
        let _ = fs::remove_file(&pid_path);
        println!("service is not running (removed stale pid file)");
        return Ok(());
    }

    terminate_process(pid).wrap_err("failed to send stop signal")?;
    if wait_for_exit(pid, Duration::from_secs(5)) {
        let _ = fs::remove_file(&pid_path);
        println!("service stopped");
        return Ok(());
    }

    force_kill_process(pid).wrap_err("failed to force stop service")?;
    if wait_for_exit(pid, Duration::from_secs(2)) {
        let _ = fs::remove_file(&pid_path);
        println!("service stopped (forced)");
        return Ok(());
    }

    Err(color_eyre::eyre::eyre!(
        "service did not stop after termination attempts"
    ))
}

pub(crate) fn state_paths(_cli: &Cli) -> color_eyre::Result<AppPaths> {
    AppPaths::resolve().map_err(Into::into)
}

pub(crate) fn read_pid(path: &Path) -> color_eyre::Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).wrap_err("failed to read pid file")?;
    let pid = raw
        .trim()
        .parse::<u32>()
        .wrap_err("failed to parse pid file content")?;
    Ok(Some(pid))
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let step = Duration::from_millis(100);
    let polls = timeout.as_millis() / step.as_millis();
    for _ in 0..polls {
        if !is_process_running(pid) {
            return true;
        }
        thread::sleep(step);
    }
    !is_process_running(pid)
}

#[cfg(unix)]
pub(crate) fn is_process_running(pid: u32) -> bool {
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
pub(crate) fn is_process_running(pid: u32) -> bool {
    ProcessCommand::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains(&pid.to_string()))
        })
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .status()?;
    Ok(())
}

#[cfg(unix)]
fn force_kill_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn force_kill_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()?;
    Ok(())
}
