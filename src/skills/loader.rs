use std::path::{Path, PathBuf};

use crate::types::skill::{LoadedSkill, SkillMetadata, SkillSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillScanError {
    pub path: PathBuf,
    pub code: &'static str,
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

#[derive(Debug, Default)]
pub struct SkillWalk {
    pub skills: Vec<LoadedSkill>,
    pub errors: Vec<SkillScanError>,
}

enum SkillDirClass {
    Skill(LoadedSkill),
    Group,
    Failed(SkillScanError),
}

fn is_hidden_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

/// Parse a SKILL.md file into (metadata, markdown_body).
/// Frontmatter is delimited by `---` lines at the start of the file.
pub fn parse_skill_file(content: &str) -> Option<(SkillMetadata, String)> {
    parse_skill_file_detailed(content).ok()
}

pub fn parse_skill_file_detailed(content: &str) -> Result<(SkillMetadata, String), SkillScanError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(SkillScanError {
            path: PathBuf::new(),
            code: "parse_failed",
            message: "SKILL.md is missing YAML frontmatter".to_string(),
            line: None,
            column: None,
        });
    }

    let after_first = &trimmed[3..];
    let rest = after_first.trim_start_matches(['\r', '\n']);
    let end_idx = rest.find("\n---").ok_or_else(|| SkillScanError {
        path: PathBuf::new(),
        code: "parse_failed",
        message: "SKILL.md is missing a closing frontmatter delimiter".to_string(),
        line: None,
        column: None,
    })?;
    let yaml_str = &rest[..end_idx];
    let after_closing = &rest[end_idx + 4..];
    let body = after_closing.trim_start_matches(['\r', '\n']).to_string();

    match serde_yaml::from_str::<SkillMetadata>(yaml_str) {
        Ok(metadata) => Ok((metadata, body)),
        Err(error) => {
            let (line, column) = error
                .location()
                .map(|loc| (Some(loc.line()), Some(loc.column())))
                .unwrap_or((None, None));
            Err(SkillScanError {
                path: PathBuf::new(),
                code: "parse_failed",
                message: error.to_string(),
                line,
                column,
            })
        }
    }
}

/// Load a single skill from a directory containing SKILL.md.
pub fn load_skill_from_dir(dir: &Path, source: SkillSource) -> Option<LoadedSkill> {
    load_skill_from_dir_detailed(dir, source).ok()
}

pub fn load_skill_from_dir_detailed(
    dir: &Path,
    source: SkillSource,
) -> Result<LoadedSkill, SkillScanError> {
    let skill_file = dir.join("SKILL.md");
    let content = std::fs::read_to_string(&skill_file).map_err(|error| SkillScanError {
        path: skill_file.clone(),
        code: "unreadable",
        message: error.to_string(),
        line: None,
        column: None,
    })?;
    let (metadata, body) = parse_skill_file_detailed(&content).map_err(|mut error| {
        error.path = skill_file.clone();
        error
    })?;

    let name = if metadata.name.is_empty() {
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| SkillScanError {
                path: skill_file.clone(),
                code: "parse_failed",
                message: "Skill directory name is missing".to_string(),
                line: None,
                column: None,
            })?
    } else {
        metadata.name.clone()
    };

    Ok(LoadedSkill {
        name,
        path: skill_file,
        metadata,
        content: body,
        source,
        group: None,
        mapped_dir: None,
    })
}

fn classify_skill_dir(dir: &Path, source: SkillSource) -> SkillDirClass {
    let skill_file = dir.join("SKILL.md");
    if !skill_file.exists() {
        return SkillDirClass::Group;
    }
    match load_skill_from_dir_detailed(dir, source) {
        Ok(skill) => SkillDirClass::Skill(skill),
        Err(error) => SkillDirClass::Failed(error),
    }
}

fn sorted_child_dirs(parent_dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(parent_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && !is_hidden_dir(path))
        .collect();
    dirs.sort();
    dirs
}

/// Scan all subdirectories of `parent_dir` for SKILL.md files.
pub fn load_from_dir(parent_dir: &Path, source: SkillSource) -> Vec<LoadedSkill> {
    walk_from_dir(parent_dir, source).skills
}

pub fn walk_from_dir(parent_dir: &Path, source: SkillSource) -> SkillWalk {
    let mut walk = SkillWalk::default();
    for path in sorted_child_dirs(parent_dir) {
        match classify_skill_dir(&path, source.clone()) {
            SkillDirClass::Skill(skill) => walk.skills.push(skill),
            SkillDirClass::Failed(error) => walk.errors.push(error),
            SkillDirClass::Group => {
                let Some(group_name) = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                else {
                    continue;
                };
                let mut nested = SkillWalk::default();
                collect_skills_recursive(&path, source.clone(), &mut nested, 0, 3);
                for mut skill in nested.skills {
                    skill.group = Some(group_name.clone());
                    walk.skills.push(skill);
                }
                walk.errors.extend(nested.errors);
            }
        }
    }
    walk
}

/// Recursively collect skills from nested directories up to `max_depth`.
fn collect_skills_recursive(
    dir: &Path,
    source: SkillSource,
    walk: &mut SkillWalk,
    depth: u32,
    max_depth: u32,
) {
    if depth >= max_depth {
        return;
    }
    for path in sorted_child_dirs(dir) {
        match classify_skill_dir(&path, source.clone()) {
            SkillDirClass::Skill(skill) => walk.skills.push(skill),
            SkillDirClass::Failed(error) => walk.errors.push(error),
            SkillDirClass::Group => {
                collect_skills_recursive(&path, source.clone(), walk, depth + 1, max_depth);
            }
        }
    }
}

pub(crate) fn global_skill_dirs(home_dir: &Path) -> Vec<(PathBuf, SkillSource)> {
    vec![
        (home_dir.join(".aqbot").join("skills"), SkillSource::AQBot),
        (home_dir.join(".claude").join("skills"), SkillSource::Claude),
        (home_dir.join(".agents").join("skills"), SkillSource::Agents),
        (home_dir.join(".codex").join("skills"), SkillSource::Codex),
    ]
}

/// Load skills from all global directories in priority order.
/// Deduplicates by name — first found wins.
pub fn load_all_global(home_dir: &Path) -> Vec<LoadedSkill> {
    let dirs = global_skill_dirs(home_dir);

    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (dir, source) in dirs {
        for skill in load_from_dir(&dir, source) {
            if seen.insert(skill.name.clone()) {
                result.push(skill);
            }
        }
    }
    result
}

/// Load all global skills without deduplication for management UIs.
pub fn load_all_global_for_management(home_dir: &Path) -> Vec<LoadedSkill> {
    walk_all_global(home_dir).skills
}

/// Walk every global skill source in priority order without name dedup.
pub fn walk_all_global(home_dir: &Path) -> SkillWalk {
    let mut walk = SkillWalk::default();
    for (dir, source) in global_skill_dirs(home_dir) {
        let mut source_walk = walk_from_dir(&dir, source);
        walk.skills.append(&mut source_walk.skills);
        walk.errors.append(&mut source_walk.errors);
    }
    walk
}

/// Walk up from `cwd` looking for `.git/` or `.aqbot/` to detect the project root.
pub fn detect_project_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").is_dir() || current.join(".aqbot").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Load skills from the project's `.aqbot/skills/` directory.
pub fn load_project_skills(cwd: &Path) -> Vec<LoadedSkill> {
    let root = match detect_project_root(cwd) {
        Some(r) => r,
        None => return Vec::new(),
    };
    load_from_dir(&root.join(".aqbot").join("skills"), SkillSource::Project)
}

/// Substitute template variables in skill content.
/// Replaces `$ARGUMENTS`, `${SKILL_DIR}`, and `${CLAUDE_SKILL_DIR}` (compat).
pub fn substitute_variables(content: &str, arguments: &str, skill_dir: &str) -> String {
    content
        .replace("$ARGUMENTS", arguments)
        .replace("${SKILL_DIR}", skill_dir)
        .replace("${CLAUDE_SKILL_DIR}", skill_dir)
}
