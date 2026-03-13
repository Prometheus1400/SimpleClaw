use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDefinition {
    pub id: String,
    pub description: String,
    content: String,
}

#[derive(Debug, Clone)]
pub struct SkillCatalog {
    pub skills: Vec<SkillDefinition>,
    pub stats: SkillToolLoadStats,
}

impl SkillCatalog {
    pub fn into_tool(self) -> Option<Arc<dyn Tool>> {
        if self.skills.is_empty() {
            None
        } else {
            Some(Arc::new(SkillTool::new(self.skills)) as Arc<dyn Tool>)
        }
    }

    pub fn prompt_section(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut lines = vec![
            "# Available Skills".to_owned(),
            "You can call the `skill` tool to load the full instructions for one of these skills:"
                .to_owned(),
        ];
        for skill in &self.skills {
            lines.push(format!("- `{}`: {}", skill.id, skill.description));
        }
        lines.join("\n")
    }
}

pub(crate) struct SkillFactory {
    app_base_dir: PathBuf,
}

impl SkillFactory {
    pub(crate) fn new(app_base_dir: PathBuf) -> Self {
        Self { app_base_dir }
    }

    pub(crate) fn load_for_agent(
        &self,
        agent_id: &str,
        config: &AgentInnerConfig,
        persona_root: &Path,
    ) -> Result<SkillCatalog, FrameworkError> {
        load_skill_catalog(
            agent_id,
            &config.tools.skills_config(),
            persona_root,
            &self.app_base_dir,
        )
    }
}

#[derive(Debug, Clone, Default)]
struct SkillTool {
    input_schema_json: String,
    skills_by_id: HashMap<String, String>,
}

impl SkillTool {
    fn new(skills: Vec<SkillDefinition>) -> Self {
        let skill_ids: Vec<String> = skills.iter().map(|skill| skill.id.clone()).collect();
        let input_schema_json = json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "enum": skill_ids,
                    "description": "The skill name to load."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
        .to_string();
        let skills_by_id = skills
            .into_iter()
            .map(|skill| (skill.id, skill.content))
            .collect();
        Self {
            input_schema_json,
            skills_by_id,
        }
    }

    #[cfg(test)]
    fn content_for(&self, skill_id: &str) -> Option<&str> {
        self.skills_by_id.get(skill_id).map(String::as_str)
    }
}

#[derive(Debug, Deserialize)]
struct SkillToolArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for a named skill."
    }

    fn input_schema_json(&self) -> &str {
        &self.input_schema_json
    }

    async fn execute(
        &self,
        _ctx: &ToolExecEnv<'_>,
        args_json: &str,
        _session_id: &str,
    ) -> Result<crate::tools::ToolExecutionOutcome, FrameworkError> {
        let args: SkillToolArgs = serde_json::from_str(args_json).map_err(|err| {
            FrameworkError::Tool(format!("invalid arguments for skill tool: {err}"))
        })?;
        let skill_id = args.name.trim();
        let Some(content) = self.skills_by_id.get(skill_id) else {
            return Err(FrameworkError::Tool(format!("unknown skill '{skill_id}'")));
        };

        Ok(crate::tools::ToolExecutionOutcome::completed(
            content.clone(),
        ))
    }
}

pub fn load_skill_catalog(
    agent_id: &str,
    skills: &SkillsToolConfig,
    persona_root: &Path,
    app_base_dir: &Path,
) -> Result<SkillCatalog, FrameworkError> {
    let skill_ids = active_skill_ids(skills, persona_root, app_base_dir)?;
    let mut stats = SkillToolLoadStats {
        requested: skill_ids.len(),
        loaded: 0,
        skipped_missing: 0,
        skipped_empty: 0,
    };
    let mut loaded_skills = Vec::new();

    for skill_id in skill_ids {
        let resolved = resolve_skill_path(persona_root, app_base_dir, &skill_id);
        let Some(path) = resolved else {
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

        loaded_skills.push(SkillDefinition {
            id: skill_id.clone(),
            description: extract_description(&content, &skill_id),
            content,
        });
        stats.loaded += 1;
    }

    Ok(SkillCatalog {
        skills: loaded_skills,
        stats,
    })
}

fn extract_description(content: &str, skill_id: &str) -> String {
    let fallback = || format!("Skill `{skill_id}` - call `skill` with this name to load it.");
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

fn resolve_skill_path(persona_root: &Path, base_dir: &Path, skill_id: &str) -> Option<PathBuf> {
    let agent_path = persona_root.join("skills").join(skill_id).join("SKILL.md");
    if agent_path.exists() {
        return Some(agent_path);
    }

    let global_path = base_dir.join("skills").join(skill_id).join("SKILL.md");
    if global_path.exists() {
        return Some(global_path);
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
    use serde_json::Value;
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
            load_skill_catalog("planner", &skills, &workspace, &base_dir).expect("load skills");

        let names: Vec<String> = loaded.skills.iter().map(|skill| skill.id.clone()).collect();
        assert_eq!(names, vec!["research".to_owned()]);
        assert_eq!(
            loaded.stats,
            SkillToolLoadStats {
                requested: 1,
                loaded: 1,
                skipped_missing: 0,
                skipped_empty: 0
            }
        );
        assert_eq!(loaded.skills[0].content, "AGENT\n");

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
            load_skill_catalog("planner", &skills, &base_dir, &base_dir).expect("load skills");

        let names: Vec<String> = loaded.skills.iter().map(|skill| skill.id.clone()).collect();
        assert_eq!(names, vec!["ok".to_owned()]);
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
            load_skill_catalog("planner", &skills, &base_dir, &base_dir).expect("load skills");

        let names: Vec<String> = loaded.skills.iter().map(|skill| skill.id.clone()).collect();
        assert_eq!(names, vec!["ok".to_owned()]);
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
            load_skill_catalog("planner", &skills, &base_dir, &base_dir).expect_err("invalid id");
        match err {
            FrameworkError::Config(message) => assert!(message.contains("invalid skill id")),
            other => panic!("expected config error, got {other}"),
        }
        let _ = fs::remove_dir_all(base_dir);
    }

    #[test]
    fn renders_prompt_section_and_dynamic_enum_schema() {
        let catalog = SkillCatalog {
            skills: vec![
                SkillDefinition {
                    id: "code_review".to_owned(),
                    description: "Review a diff for correctness.".to_owned(),
                    content: "alpha".to_owned(),
                },
                SkillDefinition {
                    id: "research".to_owned(),
                    description: "Investigate a topic.".to_owned(),
                    content: "beta".to_owned(),
                },
            ],
            stats: SkillToolLoadStats {
                requested: 2,
                loaded: 2,
                skipped_missing: 0,
                skipped_empty: 0,
            },
        };

        let prompt = catalog.prompt_section();
        assert!(prompt.contains("# Available Skills"));
        assert!(prompt.contains("`code_review`: Review a diff for correctness."));
        assert!(prompt.contains("`research`: Investigate a topic."));

        let tool = catalog.into_tool().expect("skill tool");
        assert_eq!(tool.name(), "skill");
        let schema: Value =
            serde_json::from_str(tool.input_schema_json()).expect("schema should parse");
        assert_eq!(
            schema["properties"]["name"]["enum"],
            serde_json::json!(["code_review", "research"])
        );
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
        assert_eq!(
            desc,
            "Skill `myskill` - call `skill` with this name to load it."
        );
    }

    #[test]
    fn extract_description_fallback_missing_field() {
        let content = "---\ntitle: Something\n---\n# Skill\n";
        let desc = extract_description(content, "myskill");
        assert_eq!(
            desc,
            "Skill `myskill` - call `skill` with this name to load it."
        );
    }

    #[test]
    fn skill_tool_returns_named_content() {
        let tool = SkillTool::new(vec![SkillDefinition {
            id: "research".to_owned(),
            description: "Investigate a topic.".to_owned(),
            content: "---\ndescription: Investigate a topic.\n---\n# Research\n".to_owned(),
        }]);

        assert_eq!(
            tool.content_for("research"),
            Some("---\ndescription: Investigate a topic.\n---\n# Research\n")
        );
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}"))
    }
}
