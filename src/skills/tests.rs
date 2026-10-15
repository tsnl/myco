use super::*;
use crate::test_support::temp_dir;
use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::symlink};

fn skill(root: &Path, layout: &str, id: &str, name: &str, description: &str) -> PathBuf {
    let path = root.join(layout).join("skills").join(id).join("SKILL.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\nInstructions\n"),
    )
    .unwrap();
    path
}

#[test]
fn project_scope_stops_at_nearest_git_root_and_preserves_duplicate_names() {
    let tmp = temp_dir("skills-project");
    let outer = tmp.path();
    let project = outer.join("repo");
    let nested = project.join("src/deep");
    let home = outer.join("home");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(project.join(".git"), "gitdir: elsewhere").unwrap();
    skill(outer, ".agents", "excluded", "outer", "outside project");
    let a = skill(&project, ".agents", "one", "shared", "project");
    let b = skill(&project.join("src"), ".claude", "two", "shared", "ancestor");
    let c = skill(&nested, ".grok", "three", "shared", "nearest");
    let d = skill(&home, ".agents", "four", "shared", "home");
    // A recursively nested skill is outside the documented one-directory layout.
    skill(
        &project.join(".agents/skills/one"),
        ".agents",
        "nested",
        "hidden",
        "not recursively scanned",
    );
    let catalog = SkillCatalog::scan(&nested, Some(&home));
    assert!(catalog.issues.is_empty(), "{:?}", catalog.issues);
    assert!(!catalog.truncated);
    let found: BTreeSet<_> = catalog.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(found, [a, b, c, d].into_iter().collect());
    assert!(catalog.entries.iter().all(|entry| entry.name == "shared"));
    assert_eq!(catalog, SkillCatalog::scan(&nested, Some(&home)));
}

#[test]
fn outside_git_only_the_supplied_directory_and_home_are_scanned() {
    let tmp = temp_dir("skills-no-git");
    let child = tmp.path().join("child");
    std::fs::create_dir(&child).unwrap();
    skill(tmp.path(), ".agents", "parent", "excluded", "parent");
    let local = skill(&child, ".agents", "local", "included", "child");
    // An ambient .git above the temp fixture must not change this unit case.
    let mut scan = Scan::default();
    let roots = scan.search_roots([child.as_path(), tmp.path()].into_iter());
    assert_eq!(roots.len(), 1);
    scan.root(&roots[0]);
    assert_eq!(scan.catalog.entries.len(), 1);
    assert_eq!(scan.catalog.entries[0].path, local);
    // Public scan also deduplicates the same directory supplied as home.
    std::fs::create_dir(child.join(".git")).unwrap();
    let result = SkillCatalog::scan(&child, Some(&child));
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].path, local);
    assert!(result.issues.is_empty());
}

#[test]
fn quoted_and_block_metadata_are_supported_but_malformed_fields_are_reported() {
    let tmp = temp_dir("skills-metadata");
    skill(
        tmp.path(),
        ".agents",
        "a",
        "'it''s a skill' # comment",
        "\"quoted \\\"description\\\"\" # comment",
    );
    skill(
        tmp.path(),
        ".agents",
        "b",
        "literal",
        "|-\n  First line\n  Second line",
    );
    skill(
        tmp.path(),
        ".agents",
        "c",
        "folded",
        ">\n  First line\n  Second line",
    );
    skill(
        tmp.path(),
        ".agents",
        "d",
        "broken",
        "[unsupported, sequence]",
    );
    skill(tmp.path(), ".agents", "e", "duplicate\nname: again", "bad");
    let catalog = SkillCatalog::scan(tmp.path(), None);
    assert_eq!(catalog.entries.len(), 3);
    assert_eq!(catalog.entries[0].name, "it's a skill");
    assert_eq!(catalog.entries[0].description, "quoted \"description\"");
    assert_eq!(catalog.entries[1].description, "First line\nSecond line");
    assert_eq!(catalog.entries[2].description, "First line Second line");
    assert_eq!(catalog.issues.len(), 2);
    assert!(
        catalog
            .issues
            .iter()
            .any(|issue| issue.contains("unsupported YAML"))
    );
    assert!(
        catalog
            .issues
            .iter()
            .any(|issue| issue.contains("duplicate name"))
    );
}

#[test]
fn symlinked_roots_directories_and_manifests_and_fifos_are_never_read() {
    let tmp = temp_dir("skills-links");
    let outside = tmp.path().join("outside");
    let source = skill(&outside, ".agents", "source", "hidden", "not discovered");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(local.join(".agents/skills/link-file")).unwrap();
    let layouts = local.join(".agents/skills");
    symlink(source.parent().unwrap(), layouts.join("link-dir")).unwrap();
    symlink(&source, layouts.join("link-file/SKILL.md")).unwrap();
    symlink(outside.join(".agents"), local.join(".claude")).unwrap();
    let fifo_dir = layouts.join("fifo");
    std::fs::create_dir(&fifo_dir).unwrap();
    let fifo = CString::new(fifo_dir.join("SKILL.md").as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated fixture path; no pointers are retained.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let result = SkillCatalog::scan(&local, None);
    assert!(result.entries.is_empty());
    assert_eq!(result.issues.len(), 4, "{:?}", result.issues);
    symlink(&outside, tmp.path().join("root-link")).unwrap();
    assert!(
        !SkillCatalog::scan(&tmp.path().join("root-link"), None)
            .issues
            .is_empty()
    );
    assert!(
        !SkillCatalog::scan(&local.join("../outside"), None)
            .issues
            .is_empty()
    );
}

#[test]
fn metadata_updates_and_deletions_change_catalogue_but_large_bodies_do_not() {
    let tmp = temp_dir("skills-refresh");
    let path = skill(tmp.path(), ".agents", "skill", "name", "before");
    let before = SkillCatalog::scan(tmp.path(), None);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    file.write_all(&vec![b'x'; MAX_READ_BYTES * 2]).unwrap();
    assert_eq!(before, SkillCatalog::scan(tmp.path(), None));
    skill(tmp.path(), ".agents", "skill", "name", "after");
    let after = SkillCatalog::scan(tmp.path(), None);
    assert_ne!(before.entries[0].digest, after.entries[0].digest);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    assert!(SkillCatalog::scan(tmp.path(), None).entries.is_empty());
    assert_eq!(
        SkillCatalog::scan(tmp.path(), None).render_notice(),
        "No skills found within this scan's scope."
    );
}

#[test]
fn global_entry_read_and_output_budgets_report_incomplete_discovery() {
    let tmp = temp_dir("skills-budgets");
    let layout = tmp.path().join(".agents/skills");
    std::fs::create_dir_all(&layout).unwrap();
    for i in 0..MAX_ENTRIES + 1 {
        std::fs::write(layout.join(format!("entry-{i}")), "").unwrap();
    }
    let bounded = SkillCatalog::scan(tmp.path(), None);
    assert!(bounded.truncated);
    assert!(bounded.render_notice().contains("Partial discovery"));
    std::fs::remove_dir_all(&layout).unwrap();
    for i in 0..12 {
        let path = skill(
            tmp.path(),
            ".agents",
            &format!("skill-{i:02}"),
            "name",
            "description",
        );
        std::fs::write(path, format!("---\n{}", "x".repeat(MAX_MANIFEST_BYTES))).unwrap();
    }
    let mut scan = Scan::default();
    scan.root(&Directory::open(tmp.path()).unwrap());
    assert_eq!(scan.read_bytes, MAX_READ_BYTES);
    assert!(scan.catalog.truncated);
    assert!(scan.catalog.entries.is_empty());
    let catalog = SkillCatalog {
        entries: (0..MAX_SKILLS)
            .map(|i| SkillEntry {
                name: format!("name-{i}\nforged row"),
                description: "x".repeat(2048),
                disable_model_invocation: false,
                path: PathBuf::from(format!("/skill-{i}/SKILL.md")),
                digest: String::new(),
            })
            .collect(),
        ..Default::default()
    };
    let rendered = catalog.render_notice();
    assert!(rendered.len() <= MAX_NOTICE_BYTES);
    assert!(rendered.contains("omitted_notice_entries_or_warnings="));
    assert!(!rendered.contains("\nforged row"));
}

#[test]
fn ancestor_and_catalogue_limits_are_explicit() {
    let tmp = temp_dir("skills-depth");
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let mut path = tmp.path().to_path_buf();
    for _ in 0..MAX_ANCESTORS {
        path.push("d");
    }
    std::fs::create_dir_all(&path).unwrap();
    assert!(SkillCatalog::scan(&path, None).truncated);
    for i in 0..MAX_SKILLS + 1 {
        skill(
            tmp.path(),
            ".agents",
            &format!("s{i:03}"),
            "name",
            "description",
        );
    }
    let result = SkillCatalog::scan(tmp.path(), None);
    assert_eq!(result.entries.len(), MAX_SKILLS);
    assert!(result.truncated);
}

#[test]
fn chunk_boundary_is_not_mistaken_for_a_closing_delimiter() {
    let tmp = temp_dir("skills-delimiter");
    let path = skill(tmp.path(), ".agents", "a", "name", "description");
    let start = "---\nname: name\ndescription: description\n#";
    let bytes = format!(
        "{start}{}\n---not-a-delimiter\n",
        "x".repeat(512 - start.len() - 4)
    );
    assert_eq!(&bytes.as_bytes()[509..512], b"---");
    std::fs::write(&path, bytes).unwrap();
    let result = SkillCatalog::scan(tmp.path(), None);
    assert!(result.entries.is_empty());
    assert!(!result.issues.is_empty());
    std::fs::write(path, "---\r\nname: name\r\ndescription: description\r\n---").unwrap();
    assert_eq!(SkillCatalog::scan(tmp.path(), None).entries.len(), 1);
}

#[test]
fn explicit_only_skills_preserve_the_selection_flag_and_reject_ambiguous_values() {
    let tmp = temp_dir("skills-explicit");
    skill(tmp.path(), ".agents", "skill", "name", "description");
    let original = SkillCatalog::scan(tmp.path(), None);
    assert!(!original.entries[0].disable_model_invocation);
    skill(
        tmp.path(),
        ".agents",
        "skill",
        "name",
        "description\ndisable-model-invocation: true # explicit only",
    );
    let explicit = SkillCatalog::scan(tmp.path(), None);
    assert!(explicit.entries[0].disable_model_invocation);
    assert_ne!(explicit.entries[0].digest, original.entries[0].digest);
    assert!(
        explicit
            .render_notice()
            .contains("\"disable_model_invocation\":true")
    );
    for value in ["'true'", "yes", "false\ndisable-model-invocation: true"] {
        skill(
            tmp.path(),
            ".agents",
            "skill",
            "name",
            &format!("description\ndisable-model-invocation: {value}"),
        );
        let invalid = SkillCatalog::scan(tmp.path(), None);
        assert!(invalid.entries.is_empty());
        assert!(
            invalid
                .issues
                .iter()
                .any(|error| error.contains("disable-model-invocation"))
        );
    }
}
