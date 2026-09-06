use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::skill::LoadedSkill;

pub const RUNTIME_DIR_NAME: &str = ".aqbot-skills-runtime";

const GITIGNORE_CONTENTS: &str = "*\n!.gitignore\n";
const MANIFEST_NAME: &str = "manifest.json";

#[derive(Debug, Clone)]
pub struct SkillRuntimeEntry {
    pub name: String,
    pub alias_id: String,
    pub alias_path: PathBuf,
    pub source_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SkillRuntimeMap {
    pub session_id: String,
    pub workspace: PathBuf,
    pub runtime_root: PathBuf,
    pub session_dir: PathBuf,
    pub entries: Vec<SkillRuntimeEntry>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RuntimeManifest {
    session_id: String,
    skills: Vec<ManifestSkill>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ManifestSkill {
    name: String,
    alias_id: String,
    source: String,
}

pub fn skill_alias_id(name: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= name.len() as u64;
    format!("s_{hash:016x}")
}

pub fn parse_session_id(session_id: &str) -> Result<String, String> {
    let uuid = uuid::Uuid::parse_str(session_id.trim())
        .map_err(|_| format!("Invalid agent session id '{session_id}'"))?;
    Ok(uuid.to_string())
}

pub fn is_dir_link(path: &Path) -> bool {
    #[cfg(windows)]
    {
        if junction::exists(path).unwrap_or(false) {
            return true;
        }
    }
    path.symlink_metadata()
        .map(|meta| meta.file_type().is_symlink() && path.is_dir())
        .unwrap_or(false)
}

impl SkillRuntimeMap {
    pub fn for_session(workspace: &Path, session_id: &str) -> Result<Self, String> {
        let session_id = parse_session_id(session_id)?;
        let workspace = canonicalize_dir(workspace)?;
        let runtime_root = workspace.join(RUNTIME_DIR_NAME);
        let session_dir = runtime_root.join(&session_id);
        Ok(Self {
            session_id,
            workspace,
            runtime_root,
            session_dir,
            entries: Vec::new(),
        })
    }

    pub fn mapped_dir_for(&self, skill_name: &str) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.name == skill_name)
            .map(|entry| entry.alias_path.clone())
    }

    pub fn authorize(&self, path: &str, cwd: &str, write: bool) -> Option<Result<(), String>> {
        let logical = logical_path(path, cwd);
        if let Some(target) = runtime_tail(&logical, cwd, &self.workspace) {
            if write {
                return Some(Err(format!(
                    "Skill runtime directory is read-only: {}",
                    logical.display()
                )));
            }
            return Some(self.authorize_runtime_read(target, &logical));
        }
        if write && self.targets_mapped_source(&logical) {
            return Some(Err(format!(
                "Mapped skill files are read-only: {}",
                logical.display()
            )));
        }
        None
    }

    fn authorize_runtime_read(&self, target: RuntimePath, logical: &Path) -> Result<(), String> {
        let RuntimePath::Session {
            session_id,
            remainder,
        } = target
        else {
            return Err(format!(
                "Skill runtime root '{}' is not readable",
                logical.display()
            ));
        };
        if session_id != self.session_id {
            return Err(format!(
                "Skill runtime path '{}' belongs to another session",
                logical.display()
            ));
        }
        let mut comps = remainder.components();
        let Some(first) = comps.next() else {
            return Ok(());
        };
        let Component::Normal(alias) = first else {
            return Err(format!(
                "Invalid skill runtime path '{}'",
                logical.display()
            ));
        };
        let alias_id = alias.to_string_lossy();
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.alias_id == alias_id)
            .ok_or_else(|| {
                format!(
                    "Skill runtime path '{}' is not mapped for this session",
                    logical.display()
                )
            })?;
        if !is_dir_link(&entry.alias_path) {
            return Err(format!(
                "Skill mapping '{}' is missing or is not a directory link",
                entry.alias_path.display()
            ));
        }
        let actual = entry.alias_path.canonicalize().map_err(|error| {
            format!(
                "Cannot resolve skill mapping '{}': {error}",
                entry.alias_path.display()
            )
        })?;
        if actual != entry.source_path {
            return Err(format!(
                "Skill mapping '{}' no longer points to '{}'",
                entry.alias_path.display(),
                entry.source_path.display()
            ));
        }
        let nested: PathBuf = comps.as_path().to_path_buf();
        let candidate = actual.join(&nested);
        let resolved = if candidate.exists() {
            candidate
                .canonicalize()
                .map_err(|error| format!("Cannot resolve '{}': {error}", logical.display()))?
        } else {
            existing_canonical(&candidate)
                .ok_or_else(|| format!("Cannot verify '{}'", logical.display()))?
        };
        if !resolved.starts_with(&actual) {
            return Err(format!(
                "Path '{}' escapes skill directory '{}'",
                logical.display(),
                actual.display()
            ));
        }
        Ok(())
    }

    fn targets_mapped_source(&self, logical: &Path) -> bool {
        let resolved = if logical.exists() {
            logical.canonicalize().ok()
        } else {
            existing_canonical(logical)
        };
        let Some(resolved) = resolved else {
            return false;
        };
        self.entries
            .iter()
            .any(|entry| resolved.starts_with(&entry.source_path))
    }
}

pub fn sync_skill_runtime(
    workspace: &Path,
    session_id: &str,
    skills: &[LoadedSkill],
) -> Result<SkillRuntimeMap, String> {
    let mut map = SkillRuntimeMap::for_session(workspace, session_id)?;
    fs::create_dir_all(&map.session_dir).map_err(|error| {
        format!(
            "Failed to create skill runtime directory '{}': {error}",
            map.session_dir.display()
        )
    })?;
    write_gitignore(&map.runtime_root)?;

    let desired = desired_entries(&map.session_dir, skills)?;
    let manifest_path = map.session_dir.join(MANIFEST_NAME);
    let old = read_manifest(&manifest_path);
    remove_stale_links(&map.session_dir, &old, &desired)?;
    reject_unknown_entries(&map.session_dir, &desired)?;
    create_or_update_links(&desired)?;

    map.entries = desired.values().cloned().collect();
    map.entries.sort_by(|a, b| a.alias_id.cmp(&b.alias_id));
    write_manifest(
        &manifest_path,
        &RuntimeManifest {
            session_id: map.session_id.clone(),
            skills: map
                .entries
                .iter()
                .map(|entry| ManifestSkill {
                    name: entry.name.clone(),
                    alias_id: entry.alias_id.clone(),
                    source: entry.source_path.to_string_lossy().into_owned(),
                })
                .collect(),
        },
    )?;
    Ok(map)
}

fn desired_entries(
    session_dir: &Path,
    skills: &[LoadedSkill],
) -> Result<HashMap<String, SkillRuntimeEntry>, String> {
    let mut desired = HashMap::new();
    for skill in skills {
        let source_dir = skill
            .path
            .parent()
            .ok_or_else(|| format!("Skill '{}' is missing a directory", skill.name))?;
        let source_path = canonicalize_dir(source_dir)?;
        let alias_id = skill_alias_id(&skill.name);
        desired.insert(
            alias_id.clone(),
            SkillRuntimeEntry {
                name: skill.name.clone(),
                alias_id: alias_id.clone(),
                alias_path: session_dir.join(&alias_id),
                source_path,
            },
        );
    }
    Ok(desired)
}

fn remove_stale_links(
    session_dir: &Path,
    old: &RuntimeManifest,
    desired: &HashMap<String, SkillRuntimeEntry>,
) -> Result<(), String> {
    for skill in &old.skills {
        if desired.contains_key(&skill.alias_id) {
            continue;
        }
        let link = session_dir.join(&skill.alias_id);
        if !link.exists() && link.symlink_metadata().is_err() {
            continue;
        }
        if is_dir_link(&link) {
            remove_dir_link(&link)?;
        } else {
            return Err(format!(
                "Refusing to remove unexpected skill runtime entry '{}'",
                link.display()
            ));
        }
    }
    Ok(())
}

fn reject_unknown_entries(
    session_dir: &Path,
    desired: &HashMap<String, SkillRuntimeEntry>,
) -> Result<(), String> {
    let entries = fs::read_dir(session_dir).map_err(|error| {
        format!(
            "Failed to read skill runtime directory '{}': {error}",
            session_dir.display()
        )
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("Failed to read skill runtime entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == MANIFEST_NAME || desired.contains_key(name.as_ref()) {
            continue;
        }
        let path = entry.path();
        if is_dir_link(&path) {
            remove_dir_link(&path)?;
            continue;
        }
        return Err(format!(
            "Unexpected file in skill runtime directory: '{}'",
            path.display()
        ));
    }
    Ok(())
}

fn create_or_update_links(desired: &HashMap<String, SkillRuntimeEntry>) -> Result<(), String> {
    for entry in desired.values() {
        if is_dir_link(&entry.alias_path) {
            let actual = entry.alias_path.canonicalize().ok();
            if actual.as_ref() == Some(&entry.source_path) {
                continue;
            }
            remove_dir_link(&entry.alias_path)?;
        } else if entry.alias_path.exists() {
            return Err(format!(
                "Skill runtime path '{}' exists and is not a directory link",
                entry.alias_path.display()
            ));
        }
        create_dir_link(&entry.source_path, &entry.alias_path)?;
    }
    Ok(())
}

fn create_dir_link(source: &Path, dest: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, dest).map_err(|error| {
            format!(
                "Failed to link '{}' -> '{}': {error}",
                dest.display(),
                source.display()
            )
        })
    }
    #[cfg(windows)]
    {
        junction::create(source, dest).map_err(|error| {
            format!(
                "Failed to create junction '{}' -> '{}': {error}",
                dest.display(),
                source.display()
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (source, dest);
        Err("Skill runtime mapping is not supported on this platform".to_string())
    }
}

fn remove_dir_link(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        fs::remove_file(path).map_err(|error| {
            format!(
                "Failed to remove skill mapping '{}': {error}",
                path.display()
            )
        })
    }
    #[cfg(windows)]
    {
        junction::delete(path).map_err(|error| {
            format!(
                "Failed to remove skill mapping '{}': {error}",
                path.display()
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err("Skill runtime mapping is not supported on this platform".to_string())
    }
}

fn write_gitignore(runtime_root: &Path) -> Result<(), String> {
    let path = runtime_root.join(".gitignore");
    fs::write(&path, GITIGNORE_CONTENTS)
        .map_err(|error| format!("Failed to write '{}': {error}", path.display()))
}

fn read_manifest(path: &Path) -> RuntimeManifest {
    let Ok(text) = fs::read_to_string(path) else {
        return RuntimeManifest::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_manifest(path: &Path, manifest: &RuntimeManifest) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| format!("Failed to serialize skill runtime manifest: {error}"))?;
    fs::write(path, text).map_err(|error| format!("Failed to write '{}': {error}", path.display()))
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|error| format!("Cannot resolve directory '{}': {error}", path.display()))
}

fn logical_path(path: &str, cwd: &str) -> PathBuf {
    let raw = Path::new(path);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        Path::new(cwd).join(raw)
    };
    normalize_lexically(&absolute)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(_) => out.push(component),
        }
    }
    out
}

enum RuntimePath {
    Root,
    Session {
        session_id: String,
        remainder: PathBuf,
    },
}

fn runtime_tail(logical: &Path, cwd: &str, workspace: &Path) -> Option<RuntimePath> {
    let comps: Vec<Component<'_>> = logical.components().collect();
    let idx = comps
        .iter()
        .position(|component| component.as_os_str() == RUNTIME_DIR_NAME)?;
    let prefix: PathBuf = comps[..idx].iter().collect();
    let cwd_logical = normalize_lexically(Path::new(cwd));
    if prefix != *workspace && prefix != cwd_logical {
        return None;
    }
    let Some(session_comp) = comps.get(idx + 1) else {
        return Some(RuntimePath::Root);
    };
    let session_id = session_comp.as_os_str().to_str()?.to_string();
    let remainder: PathBuf = comps[idx + 2..].iter().collect();
    Some(RuntimePath::Session {
        session_id,
        remainder,
    })
}

fn existing_canonical(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return path.canonicalize().ok();
    }
    let mut ancestor = path.parent()?.to_path_buf();
    loop {
        if ancestor.exists() {
            return ancestor.canonicalize().ok();
        }
        if !ancestor.pop() {
            return None;
        }
    }
}
