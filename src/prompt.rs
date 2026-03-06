use std::fs;
use std::path::Path;

use crate::error::FrameworkError;

pub struct PromptAssembler;

impl PromptAssembler {
    pub fn from_workspace(workspace: &Path) -> Result<String, FrameworkError> {
        let mut sections = Vec::new();
        append_layer(&mut sections, workspace, "IDENTITY", "IDENTITY.md")?;
        append_layer(&mut sections, workspace, "AGENT", "AGENT.md")?;
        append_layer(&mut sections, workspace, "USER", "USER.md")?;
        append_layer(&mut sections, workspace, "MEMORY", "MEMORY.md")?;
        append_layer(&mut sections, workspace, "SOUL", "SOUL.md")?;
        Ok(sections.join("\n\n"))
    }
}

fn read_layer(workspace: &Path, file: &str) -> Result<String, FrameworkError> {
    let path = workspace.join(file);
    if path.exists() {
        return fs::read_to_string(path).map_err(FrameworkError::from);
    }
    Ok(String::new())
}

fn append_layer(
    sections: &mut Vec<String>,
    workspace: &Path,
    title: &str,
    file: &str,
) -> Result<(), FrameworkError> {
    let content = read_layer(workspace, file)?;
    if content.trim().is_empty() {
        return Ok(());
    }

    sections.push(format!("# {title}\n{}", content.trim_end()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn skips_missing_prompt_layers() {
        let workspace = unique_temp_workspace("prompt_missing_layers");
        fs::create_dir_all(&workspace).expect("temp workspace");
        fs::write(workspace.join("AGENT.md"), "agent content\n").expect("write AGENT");
        fs::write(workspace.join("SOUL.md"), "soul content\n").expect("write SOUL");

        let prompt = PromptAssembler::from_workspace(&workspace).expect("assemble prompt");

        assert_eq!(prompt, "# AGENT\nagent content\n\n# SOUL\nsoul content");
        assert!(!prompt.contains("# IDENTITY"));
        assert!(!prompt.contains("# USER"));
        assert!(!prompt.contains("# MEMORY"));

        let _ = fs::remove_dir_all(&workspace);
    }

    fn unique_temp_workspace(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("aux_{prefix}_{nanos}"))
    }
}
