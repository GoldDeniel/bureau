//! Distribution tests use real temp trees: copies, manifest determinism and
//! every refusal path.
use bureau::skills::{self, Action, Config};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "bureau-skills-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn skill(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::create_dir_all(path.join("scripts")).unwrap();
    fs::write(
        path.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test fixture\n---\n\n{body}\n"),
    )
    .unwrap();
    fs::write(path.join("scripts/run.sh"), format!("#!/bin/sh\n{body}\n")).unwrap();
    path
}

fn cfg(sources: Vec<PathBuf>, target: &Path, apply: bool) -> Config {
    Config {
        sources,
        target: target.to_path_buf(),
        apply,
    }
}

fn manifest(target: &Path) -> skills::Manifest {
    let bytes = fs::read(target.join(skills::MANIFEST_FILE)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn copies_tree_records_manifest_and_is_a_noop_when_unchanged() {
    let root = root("copy");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    let source = skill(&source_root, "alpha", "alpha body");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            source.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o750),
        )
        .unwrap();
    }
    let source_hash = skills::tree(&source).unwrap().hash;

    let config = cfg(vec![source_root], &target, true);
    let first = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(first[0].action, Action::Create);
    assert_eq!(
        fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(),
        fs::read_to_string(source.join("SKILL.md")).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(target.join("alpha/scripts/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
    }
    assert_eq!(
        skills::tree(&source).unwrap().hash,
        source_hash,
        "source tree must be untouched by the copy"
    );

    let written = manifest(&target);
    assert_eq!(written.version, skills::MANIFEST_VERSION);
    assert_eq!(written.tool_version, env!("CARGO_PKG_VERSION"));
    let entry = &written.skills["alpha"];
    assert_eq!(entry.source_hash, source_hash);
    assert_eq!(entry.installed_hash, source_hash);
    assert_eq!(entry.source, source.to_string_lossy());
    assert_eq!(entry.files, vec!["SKILL.md", "scripts/run.sh"]);
    assert!(!entry.timestamp.is_empty());
    assert_eq!(skills::tree(&target.join("alpha")).unwrap().hash, source_hash);

    let manifest_path = target.join(skills::MANIFEST_FILE);
    let bytes = fs::read(&manifest_path).unwrap();
    let second = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(second[0].action, Action::UpToDate);
    assert_eq!(
        fs::read(&manifest_path).unwrap(),
        bytes,
        "an unchanged run must leave the manifest byte-identical"
    );
}

#[test]
fn dry_run_reports_without_writing() {
    let root = root("dry-run");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    skill(&source_root, "alpha", "alpha body");

    let config = cfg(vec![source_root], &target, false);
    let outcomes = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(outcomes[0].action, Action::Create);
    assert!(outcomes[0].line().starts_with("create "));
    assert!(outcomes[0].changed());
    assert!(!target.exists(), "dry run must not create the target");
}

#[test]
fn unmanaged_target_directory_is_never_overwritten() {
    let root = root("unmanaged");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    skill(&source_root, "alpha", "new body");
    fs::create_dir_all(target.join("alpha")).unwrap();
    fs::write(target.join("alpha/SKILL.md"), "user owned\n").unwrap();

    let config = cfg(vec![source_root], &target, true);
    let outcomes = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(outcomes[0].action, Action::Skip);
    assert!(outcomes[0].detail.contains("unmanaged directory at target"));
    assert_eq!(
        fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(),
        "user owned\n"
    );
    assert!(!target.join(skills::MANIFEST_FILE).exists());
}

#[test]
fn modified_install_is_not_overwritten() {
    let root = root("modified");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    let source = skill(&source_root, "alpha", "first body");
    let config = cfg(vec![source_root], &target, true);
    skills::distribute(&config, &["alpha".to_string()]).unwrap();

    fs::write(target.join("alpha/scripts/run.sh"), "edited by hand\n").unwrap();
    fs::write(source.join("scripts/run.sh"), "second body\n").unwrap();

    let outcomes = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(outcomes[0].action, Action::Skip);
    assert!(outcomes[0].detail.contains("changed outside Bureau"));
    assert_eq!(
        fs::read_to_string(target.join("alpha/scripts/run.sh")).unwrap(),
        "edited by hand\n"
    );
}

#[test]
fn update_replaces_a_managed_install_after_the_source_changes() {
    let root = root("update");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    let source = skill(&source_root, "alpha", "first body");
    let config = cfg(vec![source_root], &target, true);
    skills::distribute(&config, &["alpha".to_string()]).unwrap();

    fs::write(source.join("scripts/run.sh"), "second body\n").unwrap();
    let outcomes = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(outcomes[0].action, Action::Update);
    assert_eq!(
        fs::read_to_string(target.join("alpha/scripts/run.sh")).unwrap(),
        "second body\n"
    );
    let entry = &manifest(&target).skills["alpha"];
    assert_eq!(entry.source_hash, skills::tree(&source).unwrap().hash);
    assert_eq!(entry.installed_hash, entry.source_hash);
}

#[test]
fn reports_collisions_empty_skills_and_missing_names() {
    let root = root("rejects");
    let first = root.join("source-a");
    let second = root.join("source-b");
    let target = root.join("agents/skills");
    skill(&first, "dup", "from a");
    skill(&second, "dup", "from b");
    fs::create_dir_all(first.join("hollow")).unwrap();
    fs::write(first.join("hollow/notes.txt"), "no SKILL.md here\n").unwrap();

    let config = cfg(vec![first, second], &target, true);
    let outcomes = skills::distribute(
        &config,
        &["dup".to_string(), "hollow".to_string(), "absent".to_string()],
    )
    .unwrap();
    let line = |name: &str| {
        outcomes
            .iter()
            .find(|outcome| outcome.name == name)
            .unwrap()
            .clone()
    };
    assert_eq!(line("dup").action, Action::Skip);
    assert!(line("dup").detail.contains("name collision"));
    assert_eq!(line("hollow").action, Action::Skip);
    assert!(line("hollow").detail.contains("no SKILL.md"));
    assert_eq!(line("absent").action, Action::Skip);
    assert!(line("absent").detail.contains("not found under"));
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn rejects_symlinks_in_a_source_tree() {
    let root = root("symlink");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    let source = skill(&source_root, "alpha", "body");
    std::os::unix::fs::symlink("SKILL.md", source.join("linked.md")).unwrap();

    let config = cfg(vec![source_root], &target, true);
    let outcomes = skills::distribute(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(outcomes[0].action, Action::Skip);
    assert!(outcomes[0].detail.contains("symlink"));
    assert!(!target.join("alpha").exists());
}

#[test]
fn uninstall_removes_only_matching_installs() {
    let root = root("uninstall");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    skill(&source_root, "alpha", "alpha body");
    skill(&source_root, "beta", "beta body");
    let config = cfg(vec![source_root], &target, true);
    skills::distribute(&config, &["alpha".to_string(), "beta".to_string()]).unwrap();

    let removed = skills::uninstall(&config, &["alpha".to_string()]).unwrap();
    assert_eq!(removed[0].action, Action::Remove);
    assert!(!target.join("alpha").exists());
    assert!(target.join("beta").exists());
    assert!(manifest(&target).skills.contains_key("beta"));

    fs::write(target.join("beta/SKILL.md"), "edited\n").unwrap();
    let refused = skills::uninstall(&config, &["beta".to_string()]).unwrap();
    assert_eq!(refused[0].action, Action::Skip);
    assert!(refused[0].detail.contains("refusing to delete"));
    assert!(target.join("beta").exists());

    // A name Bureau did not install is never removed.
    fs::create_dir_all(target.join("user-owned")).unwrap();
    let unknown = skills::uninstall(&config, &["user-owned".to_string()]).unwrap();
    assert_eq!(unknown[0].action, Action::Skip);
    assert!(target.join("user-owned").exists());
}

#[test]
fn sync_uses_the_recorded_source_even_without_candidate_roots() {
    let root = root("sync");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    let source = skill(&source_root, "alpha", "first body");
    skills::distribute(
        &cfg(vec![source_root], &target, true),
        &["alpha".to_string()],
    )
    .unwrap();

    fs::write(source.join("scripts/run.sh"), "synced body\n").unwrap();
    let outcomes = skills::sync(&cfg(Vec::new(), &target, true)).unwrap();
    assert_eq!(outcomes[0].action, Action::Update);
    assert_eq!(
        fs::read_to_string(target.join("alpha/scripts/run.sh")).unwrap(),
        "synced body\n"
    );
}

#[test]
fn list_separates_managed_unmanaged_and_available() {
    let root = root("list");
    let source_root = root.join("config-skills");
    let target = root.join("agents/skills");
    skill(&source_root, "alpha", "alpha body");
    skill(&source_root, "available-one", "available body");
    skills::distribute(
        &cfg(vec![source_root.clone()], &target, true),
        &["alpha".to_string()],
    )
    .unwrap();
    fs::create_dir_all(target.join("user-owned")).unwrap();

    let rows = skills::list(&cfg(vec![source_root], &target, false)).unwrap();
    let state = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .map(|row| row.state.as_str())
            .unwrap_or("missing")
    };
    assert_eq!(state("alpha"), "managed");
    assert_eq!(state("user-owned"), "unmanaged");
    assert_eq!(state("available-one"), "available");
}
