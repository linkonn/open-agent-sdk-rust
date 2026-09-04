use open_agent_sdk::skills::{inspect_global_skills, load_all_global, load_from_dir, walk_from_dir};
use open_agent_sdk::types::SkillSource;
use std::collections::HashSet;

fn write_skill(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {}\ndescription: Test skill\n---\n\n# {}\n",
            name, name
        ),
    )
    .unwrap();
}

#[test]
fn load_all_global_includes_codex_skills() {
    let home = tempfile::tempdir().unwrap();
    write_skill(&home.path().join(".codex/skills/demo"), "demo");

    let skills = load_all_global(home.path());

    let skill = skills.iter().find(|s| s.name == "demo").unwrap();
    assert_eq!(skill.source, SkillSource::Codex);
    assert_eq!(skill.source.as_str(), "codex");
}

#[test]
fn load_all_global_skips_hidden_codex_skill_dirs() {
    let home = tempfile::tempdir().unwrap();
    write_skill(
        &home.path().join(".codex/skills/.system/internal"),
        "internal",
    );
    write_skill(&home.path().join(".codex/skills/visible"), "visible");

    let names = load_all_global(home.path())
        .into_iter()
        .map(|s| s.name)
        .collect::<Vec<_>>();

    assert!(names.contains(&"visible".to_string()));
    assert!(!names.contains(&"internal".to_string()));
}

fn write_invalid_skill(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: [broken\ndescription: not yaml\n---\n\n# Broken\n",
    )
    .unwrap();
}

#[test]
fn parse_failed_skill_is_scan_error_not_group() {
    let root = tempfile::tempdir().unwrap();
    let broken = root.path().join("broken");
    write_invalid_skill(&broken);
    write_skill(&broken.join("nested"), "nested");

    let walk = walk_from_dir(root.path(), SkillSource::AQBot);
    assert!(walk.skills.is_empty(), "broken SKILL.md must not recurse as a group");
    assert_eq!(walk.errors.len(), 1);
    assert_eq!(walk.errors[0].code, "parse_failed");
    assert!(walk.errors[0].line.is_some());
}

#[test]
fn unreadable_skill_md_directory_is_scan_error() {
    let root = tempfile::tempdir().unwrap();
    let broken = root.path().join("broken");
    std::fs::create_dir_all(broken.join("SKILL.md")).unwrap();
    write_skill(&broken.join("nested"), "nested");

    let walk = walk_from_dir(root.path(), SkillSource::AQBot);
    assert!(walk.skills.is_empty());
    assert_eq!(walk.errors.len(), 1);
    assert_eq!(walk.errors[0].code, "unreadable");
}

#[test]
fn load_from_dir_skips_broken_skills_and_keeps_valid_ones() {
    let root = tempfile::tempdir().unwrap();
    write_invalid_skill(&root.path().join("broken"));
    write_skill(&root.path().join("ok"), "ok");

    let skills = load_from_dir(root.path(), SkillSource::AQBot);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "ok");
}

#[test]
fn inspect_marks_override_and_disable_model_invocation() {
    let home = tempfile::tempdir().unwrap();
    write_skill(&home.path().join(".aqbot/skills/shared"), "shared");
    write_skill(&home.path().join(".claude/skills/shared"), "shared");
    let manual_dir = home.path().join(".aqbot/skills/manual");
    std::fs::create_dir_all(&manual_dir).unwrap();
    std::fs::write(
        manual_dir.join("SKILL.md"),
        "---\nname: manual\ndescription: hidden from summary\ndisable-model-invocation: true\n---\n\n# Manual\n",
    )
    .unwrap();

    let mut disabled = HashSet::new();
    disabled.insert("unused".to_string());
    let report = inspect_global_skills(home.path(), &disabled, true);
    let aqbot = report
        .items
        .iter()
        .find(|item| item.source == "aqbot" && item.name == "shared")
        .unwrap();
    let claude = report
        .items
        .iter()
        .find(|item| item.source == "claude" && item.name == "shared")
        .unwrap();
    let manual = report
        .items
        .iter()
        .find(|item| item.name == "manual")
        .unwrap();

    assert!(aqbot.effective);
    assert!(aqbot.callable);
    assert!(!claude.effective);
    assert!(!claude.callable);
    assert!(claude.reasons.iter().any(|reason| reason.code == "overridden"));
    assert!(manual.callable);
    assert!(manual
        .reasons
        .iter()
        .any(|reason| reason.code == "disable_model_invocation"));
}

#[tokio::test]
async fn skill_tool_result_includes_skill_directory() {
    use open_agent_sdk::skills::SkillRegistry;
    use open_agent_sdk::tools::skill_tool::SkillTool;
    use open_agent_sdk::types::{Tool, ToolUseContext};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo");
    let skill = open_agent_sdk::skills::load_skill_from_dir(dir.path(), SkillSource::AQBot).unwrap();
    let mut registry = SkillRegistry::new();
    registry.register(skill);
    let tool = SkillTool::new(Arc::new(tokio::sync::RwLock::new(registry)));
    let result = tool
        .call(
            serde_json::json!({"skill_name": "demo"}),
            &ToolUseContext::new(dir.path().to_string_lossy().into_owned()),
        )
        .await
        .unwrap();
    let text = result.get_text();
    assert!(text.contains(&format!("Skill directory: {}", dir.path().display())));
}

#[test]
fn inspect_skill_tool_disabled_is_not_callable() {
    let home = tempfile::tempdir().unwrap();
    write_skill(&home.path().join(".aqbot/skills/demo"), "demo");
    let report = inspect_global_skills(home.path(), &HashSet::new(), false);
    assert!(!report.skill_tool_allowed);
    assert!(report.items.iter().all(|item| !item.callable));
    assert!(report.items.iter().all(|item| {
        item.reasons
            .iter()
            .any(|reason| reason.code == "skill_tool_disabled")
    }));
}
