use std::collections::{HashMap, HashSet};

use crate::types::skill::LoadedSkill;

pub struct SkillRegistry {
    skills: HashMap<String, LoadedSkill>,
    disabled: HashSet<String>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            disabled: HashSet::new(),
        }
    }

    /// Register a skill. If a skill with the same name exists, ignore (first wins).
    pub fn register(&mut self, skill: LoadedSkill) {
        self.skills.entry(skill.name.clone()).or_insert(skill);
    }

    /// Set the disabled skills set (from database).
    pub fn set_disabled(&mut self, disabled: HashSet<String>) {
        self.disabled = disabled;
    }

    /// Get all registered skills (including disabled).
    pub fn all(&self) -> Vec<&LoadedSkill> {
        self.skills.values().collect()
    }

    /// Get all enabled skills.
    pub fn all_enabled(&self) -> Vec<&LoadedSkill> {
        self.skills
            .values()
            .filter(|s| !self.disabled.contains(&s.name))
            .collect()
    }

    /// Get a skill by name (regardless of enabled/disabled).
    pub fn get(&self, name: &str) -> Option<&LoadedSkill> {
        self.skills.get(name)
    }

    /// Get skills that the model can auto-invoke (enabled + not disable_model_invocation).
    pub fn auto_invocable(&self) -> Vec<&LoadedSkill> {
        self.skills
            .values()
            .filter(|s| !self.disabled.contains(&s.name) && !s.metadata.disable_model_invocation)
            .collect()
    }

    /// Get skills that the user can invoke (enabled + user_invocable).
    pub fn user_invocable(&self) -> Vec<&LoadedSkill> {
        self.skills
            .values()
            .filter(|s| !self.disabled.contains(&s.name) && s.metadata.user_invocable)
            .collect()
    }

    pub fn apply_mapped_dirs(&mut self, runtime: &crate::skills::runtime::SkillRuntimeMap) {
        for skill in self.skills.values_mut() {
            skill.mapped_dir = runtime.mapped_dir_for(&skill.name);
        }
    }

    /// Generate a summary for system prompt injection (auto-invocable skills only).
    pub fn generate_context_summary(&self) -> String {
        let skills = self.auto_invocable();
        if skills.is_empty() {
            return String::new();
        }
        let mut summary = String::from(
            "# Available Skills\n\nYou have access to the following skills via the Skill tool:\n\n",
        );
        for skill in &skills {
            summary.push_str(&format!("- **{}**", skill.name));
            if let Some(desc) = &skill.metadata.description {
                summary.push_str(&format!(": {}", truncate_skill_description(desc)));
            }
            if let Some(when) = &skill.metadata.when_to_use {
                summary.push_str(&format!(" TRIGGER when: {}", when));
            }
            if let Some(hint) = &skill.metadata.argument_hint {
                summary.push_str(&format!(" (usage: /{})", hint));
            }
            summary.push('\n');
        }
        summary.push_str(
            "\nWhen a skill matches the user's request, invoke it using the Skill tool. \
             When the Skill tool returns instructions, treat them as mandatory operational \
             procedures, not as optional reference material. Follow the instructions step by step.\n\
             Current skill names and resource paths apply to this turn only. \
             To keep using a skill, invoke the Skill tool again and use the directory it returns.\n",
        );
        summary
    }
}

/// Keep a 250-byte summary budget without splitting a UTF-8 code point.
const SKILL_DESCRIPTION_BUDGET: usize = 250;

fn truncate_skill_description(desc: &str) -> String {
    if desc.len() <= SKILL_DESCRIPTION_BUDGET {
        return desc.to_string();
    }
    let end = desc.floor_char_boundary(SKILL_DESCRIPTION_BUDGET);
    format!("{}...", &desc[..end])
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::skill::{LoadedSkill, SkillMetadata, SkillSource};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn skill_with(name: &str, description: &str, disable_model_invocation: bool) -> LoadedSkill {
        LoadedSkill {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: Some(description.to_string()),
                license: None,
                metadata: None,
                argument_hint: None,
                when_to_use: None,
                disable_model_invocation,
                user_invocable: true,
                allowed_tools: None,
                context: None,
                agent: None,
            },
            content: String::new(),
            source: SkillSource::AQBot,
            group: None,
            mapped_dir: None,
        }
    }

    fn summary_for(description: &str) -> String {
        let mut registry = SkillRegistry::new();
        registry.register(skill_with("demo", description, false));
        registry.generate_context_summary()
    }

    #[test]
    fn summary_keeps_250_ascii_bytes_without_ellipsis() {
        let desc = "a".repeat(250);
        let summary = summary_for(&desc);
        assert!(summary.contains(&desc));
        assert!(!summary.contains("a..."));
    }

    #[test]
    fn summary_truncates_251_ascii_bytes_at_budget() {
        let desc = "a".repeat(251);
        let summary = summary_for(&desc);
        assert!(summary.contains(&format!("{}...", "a".repeat(250))));
        assert!(!summary.contains(&desc));
    }

    #[test]
    fn summary_truncates_cjk_on_char_boundary() {
        let desc = "技".repeat(84);
        assert_eq!(desc.len(), 252);
        let summary = summary_for(&desc);
        let expected = format!("{}...", &desc[..desc.floor_char_boundary(250)]);
        assert!(summary.contains(&expected));
        assert!(!summary.contains(&desc));
    }

    #[test]
    fn summary_truncates_four_byte_emoji_on_char_boundary() {
        let desc = "😀".repeat(63);
        assert_eq!(desc.len(), 252);
        let summary = summary_for(&desc);
        let expected = format!("{}...", &desc[..desc.floor_char_boundary(250)]);
        assert!(summary.contains(&expected));
    }

    #[test]
    fn summary_truncates_mixed_ascii_cjk_on_mid_character_index() {
        let desc = format!("{}{}", "a".repeat(248), "技");
        assert_eq!(desc.len(), 251);
        let summary = summary_for(&desc);
        assert!(summary.contains(&format!("{}...", "a".repeat(248))));
        assert!(!summary.contains(&desc));
    }

    #[test]
    fn summary_omits_disable_model_invocation_but_skill_remains_gettable() {
        let mut registry = SkillRegistry::new();
        registry.register(skill_with("visible", "keep me", false));
        registry.register(skill_with("hidden", "do not summarize", true));
        registry.set_disabled(HashSet::new());

        let summary = registry.generate_context_summary();
        assert!(summary.contains("**visible**"));
        assert!(!summary.contains("**hidden**"));
        assert!(registry.get("hidden").is_some());
        assert_eq!(registry.all_enabled().len(), 2);
        assert_eq!(registry.auto_invocable().len(), 1);
        assert!(summary.contains("Current skill names and resource paths apply to this turn only"));
    }
}
