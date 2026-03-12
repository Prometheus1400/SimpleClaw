use sandbox_common::resolve_guest_path;
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::Path;

const DEFAULT_READ_LIMIT: usize = 2000;
const MAX_LINE_LENGTH: usize = 2000;

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn main() {
    if let Err(msg) = run() {
        eprintln!("{msg}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed reading stdin: {e}"))?;
    let args: ReadArgs =
        serde_json::from_str(&input).map_err(|e| format!("read requires JSON object args: {e}"))?;

    let path = resolve_guest_path(&args.path)?;
    let offset = args.offset.unwrap_or(1);
    if offset == 0 {
        return Err("read offset must be greater than or equal to 1".to_owned());
    }
    let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT);
    if limit == 0 {
        return Err("read limit must be greater than or equal to 1".to_owned());
    }

    let output = if path.is_dir() {
        render_directory_output(&path, offset, limit)?
    } else {
        render_file_output(&path, offset, limit)?
    };
    print!("{output}");
    Ok(())
}

fn render_file_output(path: &Path, offset: usize, limit: usize) -> Result<String, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        if offset != 1 {
            return Err(format!(
                "Offset {offset} is out of range for this file (0 lines)"
            ));
        }
    } else if offset > total_lines {
        return Err(format!(
            "Offset {offset} is out of range for this file ({total_lines} lines)"
        ));
    }

    let start = offset.saturating_sub(1);
    let selected = lines
        .iter()
        .skip(start)
        .take(limit)
        .enumerate()
        .map(|(idx, line)| {
            let value = if line.chars().count() > MAX_LINE_LENGTH {
                format!(
                    "{}...",
                    line.chars().take(MAX_LINE_LENGTH).collect::<String>()
                )
            } else {
                (*line).to_owned()
            };
            format!("{}: {}", idx + offset, value)
        })
        .collect::<Vec<_>>();

    let last_read_line = if selected.is_empty() {
        offset.saturating_sub(1)
    } else {
        offset + selected.len() - 1
    };
    let has_more = last_read_line < total_lines;
    let mut output = vec![
        format!("<path>{}</path>", path.display()),
        "<type>file</type>".to_owned(),
        "<content>".to_owned(),
    ];
    output.extend(selected);
    if has_more {
        output.push(String::new());
        output.push(format!(
            "(Showing lines {}-{} of {}. Use offset={} to continue.)",
            offset,
            last_read_line,
            total_lines,
            last_read_line + 1
        ));
    } else {
        output.push(String::new());
        output.push(format!("(End of file - total {} lines)", total_lines));
    }
    output.push("</content>".to_owned());
    Ok(output.join("\n"))
}

fn render_directory_output(path: &Path, offset: usize, limit: usize) -> Result<String, String> {
    let mut entries = fs::read_dir(path)
        .map_err(|e| format!("failed to list {}: {e}", path.display()))?
        .map(|entry| {
            let entry = entry.map_err(|e| e.to_string())?;
            let mut name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| "directory entry is not utf-8".to_owned())?
                .to_owned();
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                name.push('/');
            }
            Ok(name)
        })
        .collect::<Result<Vec<_>, String>>()?;
    entries.sort();

    let start = offset - 1;
    let selected = entries
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let truncated = start + selected.len() < entries.len();

    let mut output = vec![
        format!("<path>{}</path>", path.display()),
        "<type>directory</type>".to_owned(),
        "<entries>".to_owned(),
        selected.join("\n"),
    ];
    if truncated {
        output.push(format!(
            "\n(Showing {} of {} entries. Use offset={} to continue.)",
            selected.len(),
            entries.len(),
            offset + selected.len()
        ));
    } else {
        output.push(format!("\n({} entries)", entries.len()));
    }
    output.push("</entries>".to_owned());
    Ok(output.join("\n"))
}
