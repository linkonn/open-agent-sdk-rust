use open_agent_sdk::skills::{
    load_skill_from_dir, skill_alias_id, sync_skill_runtime, SkillRuntimeMap, RUNTIME_DIR_NAME,
};
use open_agent_sdk::types::SkillSource;
use std::fs;
use std::path::Path;

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";
const SESSION_B: &str = "22222222-2222-4222-8222-222222222222";

fn write_skill(dir: &Path, name: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test\n---\n\n{body}\n"),
    )
    .unwrap();
}

#[test]
fn skill_alias_id_is_stable_and_safe() {
    let first = skill_alias_id("demo skill/../x");
    let second = skill_alias_id("demo skill/../x");
    assert_eq!(first, second);
    assert!(first.starts_with("s_"));
    assert!(!first.contains('/'));
    assert_ne!(first, skill_alias_id("other"));
}

#[test]
fn sync_maps_skill_and_keeps_source_after_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let source = root.path().join("global/demo");
    fs::create_dir_all(&workspace).unwrap();
    write_skill(&source, "demo", "# Demo\nAQBOT_RUNTIME_PROBE");
    let skill = load_skill_from_dir(&source, SkillSource::AQBot).unwrap();

    let map = sync_skill_runtime(&workspace, SESSION_A, &[skill.clone()]).unwrap();
    let alias = map.mapped_dir_for("demo").unwrap();
    assert!(alias.starts_with(&map.session_dir));
    assert_eq!(map.session_id, SESSION_A);
    assert_eq!(
        fs::read_to_string(alias.join("SKILL.md")).unwrap(),
        fs::read_to_string(source.join("SKILL.md")).unwrap()
    );
    assert!(workspace
        .join(RUNTIME_DIR_NAME)
        .join(".gitignore")
        .is_file());

    let _ = sync_skill_runtime(&workspace, SESSION_A, &[]).unwrap();
    assert!(!alias.exists());
    assert!(source.join("SKILL.md").is_file());
}

#[test]
fn authorize_allows_mapped_read_and_denies_writes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let source = root.path().join("global/demo");
    fs::create_dir_all(&workspace).unwrap();
    write_skill(&source, "demo", "# Demo");
    let skill = load_skill_from_dir(&source, SkillSource::AQBot).unwrap();
    let map = sync_skill_runtime(&workspace, SESSION_A, &[skill]).unwrap();
    let cwd = workspace.to_str().unwrap();
    let alias = map.mapped_dir_for("demo").unwrap();
    let skill_md = alias.join("SKILL.md");

    assert!(map
        .authorize(skill_md.to_str().unwrap(), cwd, false)
        .unwrap()
        .is_ok());
    assert!(map
        .authorize(skill_md.to_str().unwrap(), cwd, true)
        .unwrap()
        .is_err());
    assert!(map
        .authorize(source.join("SKILL.md").to_str().unwrap(), cwd, true)
        .unwrap()
        .is_err());
    assert!(map
        .authorize(
            workspace.join(RUNTIME_DIR_NAME).to_str().unwrap(),
            cwd,
            true
        )
        .unwrap()
        .is_err());
}

#[test]
fn authorize_rejects_other_session_and_unknown_alias() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let source = root.path().join("global/demo");
    fs::create_dir_all(&workspace).unwrap();
    write_skill(&source, "demo", "# Demo");
    let skill = load_skill_from_dir(&source, SkillSource::AQBot).unwrap();
    let map = sync_skill_runtime(&workspace, SESSION_A, &[skill]).unwrap();
    let cwd = workspace.to_str().unwrap();
    let other = workspace
        .join(RUNTIME_DIR_NAME)
        .join(SESSION_B)
        .join(skill_alias_id("demo"))
        .join("SKILL.md");

    assert!(map
        .authorize(other.to_str().unwrap(), cwd, false)
        .unwrap()
        .is_err());
    let unknown = map.session_dir.join("not-a-skill").join("SKILL.md");
    assert!(map
        .authorize(unknown.to_str().unwrap(), cwd, false)
        .unwrap()
        .is_err());
}

#[test]
fn invalid_session_id_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let err = SkillRuntimeMap::for_session(root.path(), "not-a-uuid").unwrap_err();
    assert!(err.contains("Invalid agent session id"));
}

#[test]
fn unexpected_regular_file_in_session_dir_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session_dir = workspace.join(RUNTIME_DIR_NAME).join(SESSION_A);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("stray.txt"), "nope").unwrap();
    let err = sync_skill_runtime(&workspace, SESSION_A, &[]).unwrap_err();
    assert!(err.contains("Unexpected file"));
}
