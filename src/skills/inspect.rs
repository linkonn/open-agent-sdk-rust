use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::loader::walk_all_global;
use crate::types::skill::LoadedSkill;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillAvailabilityReason {
    pub code: String,
    pub params: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillInspectItem {
    pub name: String,
    pub description: String,
    pub source: String,
    pub source_path: String,
    pub enabled: bool,
    pub disable_model_invocation: bool,
    pub user_invocable: bool,
    pub group: Option<String>,
    pub effective: bool,
    pub effective_source_path: Option<String>,
    pub callable: bool,
    pub reasons: Vec<SkillAvailabilityReason>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillInspectScanError {
    pub path: String,
    pub code: String,
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillInspectReport {
    pub items: Vec<SkillInspectItem>,
    pub scan_errors: Vec<SkillInspectScanError>,
    pub skill_tool_allowed: bool,
}

fn reason(code: &str, params: HashMap<String, String>) -> SkillAvailabilityReason {
    SkillAvailabilityReason {
        code: code.to_string(),
        params,
    }
}

fn runtime_winners(skills: &[LoadedSkill]) -> HashMap<String, PathBuf> {
    let mut order: Vec<usize> = (0..skills.len()).collect();
    order.sort_by(|&left, &right| {
        skills[left]
            .source
            .load_priority()
            .cmp(&skills[right].source.load_priority())
            .then_with(|| skills[left].path.cmp(&skills[right].path))
    });
    let mut winners = HashMap::new();
    for index in order {
        let skill = &skills[index];
        winners
            .entry(skill.name.clone())
            .or_insert_with(|| skill.path.clone());
    }
    winners
}

fn inspect_loaded_skill(
    skill: &LoadedSkill,
    disabled: &HashSet<String>,
    skill_tool_allowed: bool,
    winners: &HashMap<String, PathBuf>,
) -> SkillInspectItem {
    let enabled = !disabled.contains(&skill.name);
    let winner_path = winners.get(&skill.name);
    let effective = winner_path.is_some_and(|path| path == &skill.path);
    let effective_source_path = winner_path.map(|path| path.to_string_lossy().into_owned());
    let mut reasons = Vec::new();

    if !enabled {
        reasons.push(reason("disabled", HashMap::new()));
    }
    if !skill_tool_allowed {
        reasons.push(reason("skill_tool_disabled", HashMap::new()));
    }
    if !effective {
        let mut params = HashMap::new();
        if let Some(path) = &effective_source_path {
            params.insert("path".to_string(), path.clone());
        }
        reasons.push(reason("overridden", params));
    }
    if skill.metadata.disable_model_invocation {
        reasons.push(reason("disable_model_invocation", HashMap::new()));
    }

    let callable = enabled && effective && skill_tool_allowed;
    if callable && reasons.is_empty() {
        reasons.push(reason("callable", HashMap::new()));
    }

    SkillInspectItem {
        name: skill.name.clone(),
        description: skill.metadata.description.clone().unwrap_or_default(),
        source: skill.source.as_str().to_string(),
        source_path: skill.path.to_string_lossy().into_owned(),
        enabled,
        disable_model_invocation: skill.metadata.disable_model_invocation,
        user_invocable: skill.metadata.user_invocable,
        group: skill.group.clone(),
        effective,
        effective_source_path,
        callable,
        reasons,
    }
}

fn source_from_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.contains("/.aqbot/skills/") || text.contains("\\.aqbot\\skills\\") {
        "aqbot".to_string()
    } else if text.contains("/.claude/skills/") || text.contains("\\.claude\\skills\\") {
        "claude".to_string()
    } else if text.contains("/.agents/skills/") || text.contains("\\.agents\\skills\\") {
        "agents".to_string()
    } else if text.contains("/.codex/skills/") || text.contains("\\.codex\\skills\\") {
        "codex".to_string()
    } else {
        String::new()
    }
}

fn inspect_scan_error_item(
    error: &super::loader::SkillScanError,
    skill_tool_allowed: bool,
) -> SkillInspectItem {
    let name = error
        .path
        .parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| error.path.to_string_lossy().into_owned());
    let mut params = HashMap::new();
    params.insert("message".to_string(), error.message.clone());
    if let Some(line) = error.line {
        params.insert("line".to_string(), line.to_string());
    }
    if let Some(column) = error.column {
        params.insert("column".to_string(), column.to_string());
    }
    let mut reasons = vec![reason(error.code, params)];
    if !skill_tool_allowed {
        reasons.push(reason("skill_tool_disabled", HashMap::new()));
    }
    SkillInspectItem {
        name,
        description: String::new(),
        source: source_from_path(&error.path),
        source_path: error.path.to_string_lossy().into_owned(),
        enabled: false,
        disable_model_invocation: false,
        user_invocable: false,
        group: None,
        effective: false,
        effective_source_path: None,
        callable: false,
        reasons,
    }
}

/// Inspect every discovered skill path for management UIs.
/// Display entries are not name-deduped; `effective` marks the runtime winner.
pub fn inspect_global_skills(
    home_dir: &Path,
    disabled: &HashSet<String>,
    skill_tool_allowed: bool,
) -> SkillInspectReport {
    let walk = walk_all_global(home_dir);
    let winners = runtime_winners(&walk.skills);
    let mut items: Vec<SkillInspectItem> = walk
        .skills
        .iter()
        .map(|skill| inspect_loaded_skill(skill, disabled, skill_tool_allowed, &winners))
        .collect();
    items.extend(
        walk.errors
            .iter()
            .map(|error| inspect_scan_error_item(error, skill_tool_allowed)),
    );

    let scan_errors = walk
        .errors
        .into_iter()
        .map(|error| SkillInspectScanError {
            path: error.path.to_string_lossy().into_owned(),
            code: error.code.to_string(),
            message: error.message,
            line: error.line,
            column: error.column,
        })
        .collect();

    SkillInspectReport {
        items,
        scan_errors,
        skill_tool_allowed,
    }
}
