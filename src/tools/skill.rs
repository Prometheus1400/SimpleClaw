use async_trait::async_trait;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

use crate::config::AgentInnerConfig;
use crate::config::SkillsToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

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
    #[cfg_attr(not(test), allow(dead_code))]
    pub stats: SkillToolLoadStats,
}

pub(crate) struct SkillFactory {
    app_base_dir: PathBuf,
    tools_by_agent: HashMap<String, Vec<Arc<dyn Tool>>>,
}

impl SkillFactory {
    pub(crate) fn new(app_base_dir: PathBuf) -> Self {
        Self {
            app_base_dir,
            tools_by_agent: HashMap::new(),
        }
    }

    pub(crate) fn load_for_agent(
        &self,
        agent_id: &str,
        config: &AgentInnerConfig,
        persona_root: &Path,
    ) -> Result<Vec<Arc<dyn Tool>>, FrameworkError> {
        let loaded = load_skill_tools(
            agent_id,
            &config.tools.skills_config(),
            persona_root,
            &self.app_base_dir,
        )?;
        Ok(loaded
            .tools
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
            .collect())
    }

    pub(crate) fn insert_agent_tools(
        &mut self,
        agent_id: impl Into<String>,
        tools: Vec<Arc<dyn Tool>>,
    ) {
        self.tools_by_agent.insert(agent_id.into(), tools);
    }
}

#[derive(Debug, Clone)]
pub struct DynamicSkillTool {
    name: String,
    description: String,
    input_schema_json: String,
    content: String,
}

impl DynamicSkillTool {
    fn new(skill_id: &str, content: String, _source_scope: &str) -> Self {
        let name = format!("skill_{skill_id}");
        let description = extract_description(&content, skill_id);
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
        _ctx: &ToolExecEnv,
        _args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        Ok(self.content.clone())
    }
}

pub fn load_skill_tools(
    agent_id: &str,
    skills: &SkillsToolConfig,
    persona_root: &Path,
    app_base_dir: &Path,
) -> Result<LoadedSkillTools, FrameworkError> {
    let skill_ids = active_skill_ids(skills, persona_root, app_base_dir)?;
    let mut stats = SkillToolLoadStats {
        requested: skill_ids.len(),
        loaded: 0,
        skipped_missing: 0,
        skipped_empty: 0,
    };
    let mut tools = Vec::new();

    for skill_id in skill_ids {
        let resolved = resolve_skill_path(persona_root, app_base_dir, &skill_id);
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
        tools.push(tool);
        stats.loaded += 1;
    }

    Ok(LoadedSkillTools { tools, stats })
}

/// Extract description from YAML frontmatter in SKILL.md content.
fn extract_description(content: &str, skill_id: &str) -> String {
    let fallback = || format!("Skill `{skill_id}` — call to retrieve its instructions.");
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return fallback();
    }
    let after_open = &trimmed[3..];
    let Some(end) = after_open.find("\n---") else {
        return fallback();
    };
    let frontmatter = &after_open[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    fallback()
}

fn resolve_skill_path(
    persona_root: &Path,
    base_dir: &Path,
    skill_id: &str,
) -> Option<(PathBuf, &'static str)> {
    let agent_path = persona_root.join("skills").join(skill_id).join("SKILL.md");
    if agent_path.exists() {
        return Some((agent_path, "agent"));
    }

    let global_path = base_dir.join("skills").join(skill_id).join("SKILL.md");
    if global_path.exists() {
        return Some((global_path, "global"));
    }

    None
}

fn active_skill_ids(
    skills: &SkillsToolConfig,
    persona_root: &Path,
    base_dir: &Path,
) -> Result<Vec<String>, FrameworkError> {
    if !skills.enabled {
        return Ok(Vec::new());
    }

    let disabled_skill_ids = normalized_disabled_skill_ids(skills)?;
    let mut discovered = discover_skill_ids(persona_root, base_dir)?;
    discovered.retain(|skill_id| !disabled_skill_ids.contains(skill_id));
    Ok(discovered)
}

fn normalized_disabled_skill_ids(
    skills: &SkillsToolConfig,
) -> Result<HashSet<String>, FrameworkError> {
    let mut seen = HashSet::new();
    for raw_id in &skills.disabled_skills {
        let skill_id = raw_id.trim();
        if skill_id.is_empty() {
            return Err(FrameworkError::Config(
                "tools.skills.disabled_skills entries must be non-empty".to_owned(),
            ));
        }
        if !is_valid_skill_id(skill_id) {
            return Err(FrameworkError::Config(format!(
                "invalid skill id '{skill_id}'; only letters, numbers, '-' and '_' are allowed"
            )));
        }
        seen.insert(skill_id.to_owned());
    }
    Ok(seen)
}

fn discover_skill_ids(persona_root: &Path, base_dir: &Path) -> Result<Vec<String>, FrameworkError> {
    let mut discovered = BTreeSet::new();
    collect_skill_ids_from_root(&persona_root.join("skills"), &mut discovered)?;
    collect_skill_ids_from_root(&base_dir.join("skills"), &mut discovered)?;
    Ok(discovered.into_iter().collect())
}

fn collect_skill_ids_from_root(
    root: &Path,
    discovered: &mut BTreeSet<String>,
) -> Result<(), FrameworkError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }

        let skill_id = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().join("SKILL.md").exists() {
            continue;
        }

        if !is_valid_skill_id(&skill_id) {
            return Err(FrameworkError::Config(format!(
                "invalid skill id '{skill_id}'; only letters, numbers, '-' and '_' are allowed"
            )));
        }

        discovered.insert(skill_id);
    }

    Ok(())
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
        let workspace = unique_temp_dir("skill_tool_override_workspace");
        let agent_path = workspace.join("skills/research/SKILL.md");
        fs::create_dir_all(global_path.parent().expect("global parent")).expect("global dir");
        fs::create_dir_all(agent_path.parent().expect("agent parent")).expect("agent dir");
        fs::write(&global_path, "GLOBAL\n").expect("write global skill");
        fs::write(&agent_path, "AGENT\n").expect("write agent skill");

        let skills = SkillsToolConfig {
            enabled: true,
            disabled_skills: Vec::new(),
        };
        let loaded =
            load_skill_tools("planner", &skills, &workspace, &base_dir).expect("load skills");

        let names: Vec<String> = loaded
            .tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert_eq!(names, vec!["skill_research".to_owned()]);
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
        let _ = fs::remove_dir_all(workspace);
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

        let skills = SkillsToolConfig {
            enabled: true,
            disabled_skills: Vec::new(),
        };
        let loaded =
            load_skill_tools("planner", &skills, &base_dir, &base_dir).expect("load skills");

        let names: Vec<String> = loaded
            .tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert_eq!(names, vec!["skill_ok".to_owned()]);
        assert_eq!(
            loaded.stats,
            SkillToolLoadStats {
                requested: 2,
                loaded: 1,
                skipped_missing: 0,
                skipped_empty: 1
            }
        );

        let _ = fs::remove_dir_all(base_dir);
    }

    #[test]
    fn disables_discovered_skills() {
        let base_dir = unique_temp_dir("skill_tool_disabled");
        let ok_path = base_dir.join("skills/ok/SKILL.md");
        let skip_path = base_dir.join("skills/skip_me/SKILL.md");
        fs::create_dir_all(ok_path.parent().expect("ok parent")).expect("ok dir");
        fs::create_dir_all(skip_path.parent().expect("skip parent")).expect("skip dir");
        fs::write(&ok_path, "ok markdown\n").expect("write ok skill");
        fs::write(&skip_path, "skip markdown\n").expect("write skip skill");

        let skills = SkillsToolConfig {
            enabled: true,
            disabled_skills: vec![
                "skip_me".to_owned(),
                "missing".to_owned(),
                "skip_me".to_owned(),
            ],
        };
        let loaded =
            load_skill_tools("planner", &skills, &base_dir, &base_dir).expect("load skills");

        let names: Vec<String> = loaded
            .tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert_eq!(names, vec!["skill_ok".to_owned()]);
        assert_eq!(
            loaded.stats,
            SkillToolLoadStats {
                requested: 1,
                loaded: 1,
                skipped_missing: 0,
                skipped_empty: 0
            }
        );

        let _ = fs::remove_dir_all(base_dir);
    }

    #[test]
    fn rejects_invalid_disabled_skill_id() {
        let base_dir = unique_temp_dir("skill_tool_invalid");
        let skills = SkillsToolConfig {
            enabled: true,
            disabled_skills: vec!["bad/skill".to_owned()],
        };
        let err =
            load_skill_tools("planner", &skills, &base_dir, &base_dir).expect_err("invalid id");
        match err {
            FrameworkError::Config(message) => assert!(message.contains("invalid skill id")),
            other => panic!("expected config error, got {other}"),
        }
        let _ = fs::remove_dir_all(base_dir);
    }

    #[test]
    fn extract_description_from_frontmatter() {
        let content = "---\ndescription: Do X and Y\n---\n# My Skill\nBody.";
        let desc = extract_description(content, "test");
        assert_eq!(desc, "Do X and Y");
    }

    #[test]
    fn extract_description_from_quoted_frontmatter() {
        let content = "---\ndescription: \"Quoted desc\"\n---\n# Skill\n";
        let desc = extract_description(content, "test");
        assert_eq!(desc, "Quoted desc");
    }

    #[test]
    fn extract_description_fallback_no_frontmatter() {
        let content = "# Just markdown\nNo frontmatter here.\n";
        let desc = extract_description(content, "myskill");
        assert_eq!(desc, "Skill `myskill` — call to retrieve its instructions.");
    }

    #[test]
    fn extract_description_fallback_missing_field() {
        let content = "---\ntitle: Something\n---\n# Skill\n";
        let desc = extract_description(content, "myskill");
        assert_eq!(desc, "Skill `myskill` — call to retrieve its instructions.");
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}"))
    }
}
