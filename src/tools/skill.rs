use async_trait::async_trait;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::config::SkillsConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkillToolLoadStats {
    pub requested: usize,
    pub loaded: usize,
    pub skipped_missing: usize,
    pub skipped_empty: usize,
}

#[derive(Debug, Clone)]
pub struct LoadedSkillTools {
    pub tools: Vec<DynamicSkillTool>,
    pub tool_names: Vec<String>,
    pub stats: SkillToolLoadStats,
}

#[derive(Debug, Clone)]
pub struct DynamicSkillTool {
    name: String,
    description: String,
    input_schema_json: String,
    content: String,
}

impl DynamicSkillTool {
    fn new(skill_id: &str, content: String, source_scope: &str) -> Self {
        let name = format!("skill_{skill_id}");
        let description = format!(
            "Return raw SKILL.md markdown for skill `{skill_id}` ({source_scope} scope). Call when you need this skill's instructions."
        );
        Self {
            name,
            description,
            input_schema_json: "{\"type\":\"object\",\"properties\":{}}".to_owned(),
            content,
        }
    }

    #[cfg(test)]
    fn raw_markdown(&self) -> &str {
        &self.content
    }
}

#[async_trait]
impl Tool for DynamicSkillTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema_json(&self) -> &str {
        &self.input_schema_json
    }

    async fn execute(
        &self,
        _ctx: &ToolCtx,
        _args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        Ok(self.content.clone())
    }
}

pub fn load_skill_tools(
    agent_id: &str,
    skills: &SkillsConfig,
    app_base_dir: &Path,
) -> Result<LoadedSkillTools, FrameworkError> {
    let skill_ids = normalized_skill_ids(skills)?;
    let mut stats = SkillToolLoadStats {
        requested: skill_ids.len(),
        loaded: 0,
        skipped_missing: 0,
        skipped_empty: 0,
    };
    let mut tools = Vec::new();
    let mut tool_names = Vec::new();

    for skill_id in skill_ids {
        let resolved = resolve_skill_path(app_base_dir, agent_id, &skill_id);
        let Some((path, source_scope)) = resolved else {
            warn!(
                agent_id = %agent_id,
                skill_id = %skill_id,
                "skill file missing in agent and global scopes; skipping"
            );
            stats.skipped_missing += 1;
            continue;
        };

        let content = fs::read_to_string(&path)?;
        if content.trim().is_empty() {
            warn!(
                agent_id = %agent_id,
                skill_id = %skill_id,
                path = %path.display(),
                "skill file is empty; skipping"
            );
            stats.skipped_empty += 1;
            continue;
        }

        let tool = DynamicSkillTool::new(&skill_id, content, source_scope);
        tool_names.push(tool.name().to_owned());
        tools.push(tool);
        stats.loaded += 1;
    }

    Ok(LoadedSkillTools {
        tools,
        tool_names,
        stats,
    })
}

fn resolve_skill_path(
    base_dir: &Path,
    agent_id: &str,
    skill_id: &str,
) -> Option<(PathBuf, &'static str)> {
    let agent_path = base_dir
        .join("workspaces")
        .join(agent_id)
        .join("skills")
        .join(skill_id)
        .join("SKILL.md");
    if agent_path.exists() {
        return Some((agent_path, "agent"));
    }

    let global_path = base_dir.join("skills").join(skill_id).join("SKILL.md");
    if global_path.exists() {
        return Some((global_path, "global"));
    }

    None
}

fn normalized_skill_ids(skills: &SkillsConfig) -> Result<Vec<String>, FrameworkError> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for raw_id in &skills.enabled_skills {
        let skill_id = raw_id.trim();
        if skill_id.is_empty() {
            return Err(FrameworkError::Config(
                "skills.enabled_skills entries must be non-empty".to_owned(),
            ));
        }
        if !is_valid_skill_id(skill_id) {
            return Err(FrameworkError::Config(format!(
                "invalid skill id '{skill_id}'; only letters, numbers, '-' and '_' are allowed"
            )));
        }
        if seen.insert(skill_id.to_owned()) {
            ids.push(skill_id.to_owned());
        }
    }
    Ok(ids)
}

fn is_valid_skill_id(skill_id: &str) -> bool {
    skill_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn agent_scope_overrides_global_scope() {
        let base_dir = unique_temp_dir("skill_tool_override");
        let global_path = base_dir.join("skills/research/SKILL.md");
        let agent_path = base_dir.join("workspaces/planner/skills/research/SKILL.md");
        fs::create_dir_all(global_path.parent().expect("global parent")).expect("global dir");
        fs::create_dir_all(agent_path.parent().expect("agent parent")).expect("agent dir");
        fs::write(&global_path, "GLOBAL\n").expect("write global skill");
        fs::write(&agent_path, "AGENT\n").expect("write agent skill");

        let skills = SkillsConfig {
            enabled_skills: vec!["research".to_owned()],
        };
        let loaded = load_skill_tools("planner", &skills, &base_dir).expect("load skills");

        assert_eq!(loaded.tool_names, vec!["skill_research".to_owned()]);
        assert_eq!(
            loaded.stats,
            SkillToolLoadStats {
                requested: 1,
                loaded: 1,
                skipped_missing: 0,
                skipped_empty: 0
            }
        );
        assert_eq!(loaded.tools[0].raw_markdown(), "AGENT\n");

        let _ = fs::remove_dir_all(base_dir);
    }

    #[test]
    fn skips_missing_and_empty_skills() {
        let base_dir = unique_temp_dir("skill_tool_skip");
        let ok_path = base_dir.join("skills/ok/SKILL.md");
        let empty_path = base_dir.join("skills/empty/SKILL.md");
        fs::create_dir_all(ok_path.parent().expect("ok parent")).expect("ok dir");
        fs::create_dir_all(empty_path.parent().expect("empty parent")).expect("empty dir");
        fs::write(&ok_path, "ok markdown\n").expect("write ok skill");
        fs::write(&empty_path, "   \n").expect("write empty skill");

        let skills = SkillsConfig {
            enabled_skills: vec![
                "missing".to_owned(),
                "empty".to_owned(),
                "ok".to_owned(),
                "ok".to_owned(),
            ],
        };
        let loaded = load_skill_tools("planner", &skills, &base_dir).expect("load skills");

        assert_eq!(loaded.tool_names, vec!["skill_ok".to_owned()]);
        assert_eq!(
            loaded.stats,
            SkillToolLoadStats {
                requested: 3,
                loaded: 1,
                skipped_missing: 1,
                skipped_empty: 1
            }
        );

        let _ = fs::remove_dir_all(base_dir);
    }

    #[test]
    fn rejects_invalid_skill_id() {
        let base_dir = unique_temp_dir("skill_tool_invalid");
        let skills = SkillsConfig {
            enabled_skills: vec!["bad/skill".to_owned()],
        };
        let err = load_skill_tools("planner", &skills, &base_dir).expect_err("invalid id");
        match err {
            FrameworkError::Config(message) => assert!(message.contains("invalid skill id")),
            other => panic!("expected config error, got {other}"),
        }
        let _ = fs::remove_dir_all(base_dir);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}"))
    }
}
