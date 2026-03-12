use sandbox_common::resolve_guest_path;
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct EditArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed reading stdin: {e}"))?;
    let args: EditArgs =
        serde_json::from_str(&input).map_err(|e| format!("edit requires JSON object args: {e}"))?;

    if args.file_path.trim().is_empty() {
        return Err("edit requires a non-empty filePath".to_owned());
    }
    if args.old_string == args.new_string {
        return Err("No changes to apply: oldString and newString are identical.".to_owned());
    }

    let path = resolve_guest_path(&args.file_path)?;
    apply_edit_at_path(&path, &args)?;
    print!("Edit applied successfully.");
    Ok(())
}

fn apply_edit_at_path(path: &Path, args: &EditArgs) -> Result<(), String> {
    if args.old_string.is_empty() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        fs::write(path, &args.new_string)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        return Ok(());
    }

    let original =
        fs::read_to_string(path).map_err(|e| format!("File {} not found: {e}", path.display()))?;
    let ending = detect_line_ending(&original);
    let old_string = convert_to_line_ending(&normalize_line_endings(&args.old_string), ending);
    let new_string = convert_to_line_ending(&normalize_line_endings(&args.new_string), ending);
    let occurrences = original.match_indices(&old_string).count();
    if occurrences == 0 {
        return Err("Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.".to_owned());
    }
    if occurrences > 1 && !args.replace_all {
        return Err(
            "Found multiple matches for oldString. Provide more surrounding context to make the match unique."
                .to_owned(),
        );
    }
    let updated = if args.replace_all {
        original.replace(&old_string, &new_string)
    } else {
        original.replacen(&old_string, &new_string, 1)
    };
    fs::write(path, updated).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn convert_to_line_ending(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_owned()
    } else {
        text.replace('\n', "\r\n")
    }
}
