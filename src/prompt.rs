use std::fs;
use std::path::{Path, PathBuf};

use crate::error::FrameworkError;

pub struct PromptAssembler;

#[derive(Debug, Clone)]
pub struct PromptLayerInfo {
    pub title: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub file: &'static str,
    pub path: PathBuf,
    pub exists: bool,
    pub bytes: u64,
}

impl PromptAssembler {
    pub fn inspect_workspace(workspace: &Path) -> Result<Vec<PromptLayerInfo>, FrameworkError> {
        prompt_layers()
            .iter()
            .map(|(title, file)| {
                let path = workspace.join(file);
                if !path.exists() {
                    return Ok(PromptLayerInfo {
                        title,
                        file,
                        path,
                        exists: false,
                        bytes: 0,
                    });
                }

                let bytes = fs::metadata(&path)?.len();
                Ok(PromptLayerInfo {
                    title,
                    file,
                    path,
                    exists: true,
                    bytes,
                })
            })
            .collect()
    }

    pub fn from_workspace(workspace: &Path) -> Result<String, FrameworkError> {
        let mut sections = Vec::new();
        for layer in Self::inspect_workspace(workspace)? {
            append_layer(
                &mut sections,
                &layer.path,
                layer.title,
                layer.exists,
                layer.bytes,
            )?;
        }
        Ok(sections.join("\n\n"))
    }
}

fn prompt_layers() -> [(&'static str, &'static str); 5] {
    [
        ("IDENTITY", "IDENTITY.md"),
        ("AGENT", "AGENT.md"),
        ("USER", "USER.md"),
        // Static prompt layer — distinct from the dynamic embedding-based memory in memory.rs
        ("MEMORY", "MEMORY.md"),
        ("SOUL", "SOUL.md"),
    ]
}

fn append_layer(
    sections: &mut Vec<String>,
    path: &Path,
    title: &str,
    exists: bool,
    bytes: u64,
) -> Result<(), FrameworkError> {
    if !exists || bytes == 0 {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(());
    }

    sections.push(format!("# {title}\n{}", content.trim_end()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn inspect_workspace_reports_presence_and_sizes() {
        let workspace = unique_temp_workspace("prompt_inspect");
        fs::create_dir_all(&workspace).expect("temp workspace");
        fs::write(workspace.join("IDENTITY.md"), "identity\n").expect("write IDENTITY");
        fs::write(workspace.join("SOUL.md"), "").expect("write empty SOUL");

        let layers = PromptAssembler::inspect_workspace(&workspace).expect("inspect workspace");

        let identity = layers
            .iter()
            .find(|layer| layer.file == "IDENTITY.md")
            .expect("identity layer");
        assert!(identity.exists);
        assert!(identity.bytes > 0);

        let soul = layers
            .iter()
            .find(|layer| layer.file == "SOUL.md")
            .expect("soul layer");
        assert!(soul.exists);
        assert_eq!(soul.bytes, 0);

        let memory = layers
            .iter()
            .find(|layer| layer.file == "MEMORY.md")
            .expect("memory layer");
        assert!(!memory.exists);
        assert_eq!(memory.bytes, 0);

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
