use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, NaiveDate};
use color_eyre::eyre::WrapErr;

pub const RETAIN_DAILY_LOG_FILES: usize = 2;

pub(crate) fn json_log_path(log_path: &Path) -> PathBuf {
    log_path.with_file_name("service.jsonl")
}

#[derive(Clone)]
pub struct RotatingLogWriter {
    inner: Arc<Mutex<RotatingLogState>>,
}

struct RotatingLogState {
    log_path: PathBuf,
    current_day: NaiveDate,
    retain_daily_files: usize,
    file: File,
}

impl RotatingLogWriter {
    pub fn new(log_path: PathBuf, retain_daily_files: usize) -> color_eyre::Result<Self> {
        let today = Local::now().date_naive();
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .wrap_err("failed to open active log file")?;
        rotate_stale_active_log_if_needed(&log_path, today)
            .wrap_err("failed to rotate stale active log file")?;
        prune_daily_logs(&log_path, retain_daily_files).wrap_err("failed to prune daily logs")?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .wrap_err("failed to reopen active log file")?;
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingLogState {
                log_path,
                current_day: today,
                retain_daily_files,
                file,
            })),
        })
    }
}

impl Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock rotating log writer"))?;
        state.rotate_if_needed()?;
        state.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock rotating log writer"))?;
        state.file.flush()
    }
}

impl RotatingLogState {
    fn rotate_if_needed(&mut self) -> std::io::Result<()> {
        let today = Local::now().date_naive();
        if today == self.current_day {
            return Ok(());
        }
        self.file.flush()?;
        rotate_active_log_to_day(&self.log_path, self.current_day)?;
        prune_daily_logs(&self.log_path, self.retain_daily_files)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        self.current_day = today;
        Ok(())
    }
}

fn rotate_stale_active_log_if_needed(log_path: &Path, today: NaiveDate) -> std::io::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }
    let metadata = fs::metadata(log_path)?;
    if metadata.len() == 0 {
        return Ok(());
    }
    let modified_day = DateTime::<Local>::from(metadata.modified()?).date_naive();
    if modified_day < today {
        rotate_active_log_to_day(log_path, modified_day)?;
    }
    Ok(())
}

fn rotate_active_log_to_day(log_path: &Path, day: NaiveDate) -> std::io::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }
    let target = dated_log_path(log_path, day);
    if target.exists() {
        let mut src = OpenOptions::new().read(true).open(log_path)?;
        let mut dst = OpenOptions::new().create(true).append(true).open(&target)?;
        std::io::copy(&mut src, &mut dst)?;
        fs::remove_file(log_path)?;
    } else {
        fs::rename(log_path, target)?;
    }
    Ok(())
}

fn prune_daily_logs(log_path: &Path, retain_daily_files: usize) -> std::io::Result<()> {
    let mut daily_logs = list_daily_log_files(log_path)?;
    if daily_logs.len() <= retain_daily_files {
        return Ok(());
    }
    daily_logs.sort_by_key(|(day, _)| *day);
    let remove_count = daily_logs.len().saturating_sub(retain_daily_files);
    for (_, path) in daily_logs.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn list_daily_log_files(log_path: &Path) -> std::io::Result<Vec<(NaiveDate, PathBuf)>> {
    let Some(dir) = log_path.parent() else {
        return Ok(Vec::new());
    };
    let Some(base_name) = log_path.file_name().and_then(|n| n.to_str()) else {
        return Ok(Vec::new());
    };

    let prefix = format!("{base_name}.");
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let raw_date = &name[prefix.len()..];
        if let Ok(day) = NaiveDate::parse_from_str(raw_date, "%Y-%m-%d") {
            out.push((day, path));
        }
    }
    Ok(out)
}

fn dated_log_path(log_path: &Path, day: NaiveDate) -> PathBuf {
    let name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("service.log");
    log_path.with_file_name(format!("{name}.{}", day.format("%Y-%m-%d")))
}

pub(crate) fn collect_log_history(log_path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut history = list_daily_log_files(log_path)?;
    history.sort_by_key(|(day, _)| *day);
    let mut out = history
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    if log_path.exists() {
        out.push(log_path.to_path_buf());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::NaiveDate;

    use super::{collect_log_history, dated_log_path, prune_daily_logs};

    fn temp_log_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic enough for tests")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}.log"))
    }

    #[test]
    fn prune_daily_logs_keeps_latest_two_days() {
        let log_path = temp_log_path("prune_daily_logs");
        let first = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 4).expect("date should be valid"),
        );
        let second = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 5).expect("date should be valid"),
        );
        let third = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 6).expect("date should be valid"),
        );

        fs::write(&first, "oldest\n").expect("should create first dated log");
        fs::write(&second, "middle\n").expect("should create second dated log");
        fs::write(&third, "newest\n").expect("should create third dated log");

        prune_daily_logs(&log_path, 2).expect("pruning should succeed");

        assert!(!first.exists());
        assert!(second.exists());
        assert!(third.exists());

        let _ = fs::remove_file(&second);
        let _ = fs::remove_file(&third);
    }

    #[test]
    fn collect_log_history_orders_days_then_active() {
        let log_path = temp_log_path("history_order");
        let old = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 5).expect("date should be valid"),
        );
        let new = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 6).expect("date should be valid"),
        );

        fs::write(&old, "older\n").expect("should create old dated log");
        fs::write(&new, "newer\n").expect("should create new dated log");
        fs::write(&log_path, "active\n").expect("should create active log");

        let history = collect_log_history(&log_path).expect("history should be discovered");
        assert_eq!(history, vec![old.clone(), new.clone(), log_path.clone()]);

        let _ = fs::remove_file(&old);
        let _ = fs::remove_file(&new);
        let _ = fs::remove_file(&log_path);
    }
}
