use sandbox_common::{apply_edit_command, EditArgs};
use std::io::Read;

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

    let result = apply_edit_command(&args)?;
    print!("{result}");
    Ok(())
}
