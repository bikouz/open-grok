//! Built-in files extracted to `~/.opengrok/` on startup.

const BUILTIN_FILES: &[(&str, &str)] = &[("README.md", include_str!("../README.md"))];

/// Built-in skills shipped with the binary as `(skill-name, SKILL.md content)`.
///
/// These are seeded into `~/.opengrok/skills/<name>/SKILL.md` on first extraction
/// and are **never overwritten** afterwards, so a user edit always survives (see
/// `extract_builtin_files`). Keep each entry's name in sync with the `name:` in
/// its frontmatter.
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    (
        "agent-swarm",
        include_str!("../skills/agent-swarm/SKILL.md"),
    ),
    ("best-of-n", include_str!("../skills/best-of-n/SKILL.md")),
    ("check-work", include_str!("../skills/check-work/SKILL.md")),
    (
        "code-review",
        include_str!("../skills/code-review/SKILL.md"),
    ),
    (
        "create-skill",
        include_str!("../skills/create-skill/SKILL.md"),
    ),
    (
        "create-workflow",
        include_str!("../skills/create-workflow/SKILL.md"),
    ),
    ("help", include_str!("../skills/help/SKILL.md")),
    ("imagine", include_str!("../skills/imagine/SKILL.md")),
    (
        "import-claude-workflow",
        include_str!("../skills/import-claude-workflow/SKILL.md"),
    ),
];

/// Extract built-in metadata files to `~/.opengrok/` on startup.
///
/// User skills under `~/.opengrok/skills/` are never managed here. Platform skills
/// are delivered separately through the bundled skill cache.
pub fn extract_builtin_files(grok_home: &std::path::Path) {
    let version = xai_grok_version::VERSION;
    let marker = grok_home.join(".metadata_version");

    if let Ok(existing) = std::fs::read_to_string(&marker)
        && existing.trim() == version
    {
        return;
    }

    let _ = std::fs::create_dir_all(grok_home);

    // Clean up cached changelog files from previous version so
    // /release-notes fetches fresh content for the new version.
    for stale in &["CHANGELOG.json", "CHANGELOG.md"] {
        let _ = std::fs::remove_file(grok_home.join(stale));
    }

    for &(filename, content) in BUILTIN_FILES {
        if let Err(e) = std::fs::write(grok_home.join(filename), content) {
            tracing::debug!(error = %e, filename, "Failed to extract built-in file");
        }
    }

    // Seed built-in skills, but NEVER overwrite one that already exists on disk:
    // once seeded, the SKILL.md belongs to the user and their edits must survive
    // every version bump. This preserves the invariant that user skills under
    // `~/.opengrok/skills/` are never managed here.
    for &(name, content) in BUILTIN_SKILLS {
        let skill_path = grok_home.join("skills").join(name).join("SKILL.md");
        if skill_path.exists() {
            continue;
        }
        if let Some(dir) = skill_path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::debug!(error = %e, skill = name, "Failed to create built-in skill dir");
            continue;
        }
        if let Err(e) = std::fs::write(&skill_path, content) {
            tracing::debug!(error = %e, skill = name, "Failed to extract built-in skill");
        }
    }

    let _ = std::fs::write(&marker, version);
    tracing::debug!(version, "Extracted built-in files");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_bump_reextracts_metadata_without_touching_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        extract_builtin_files(home);
        std::fs::write(home.join("README.md"), "old").unwrap();
        std::fs::write(home.join(".metadata_version"), "0.0.0-stale").unwrap();

        let skill_names = [
            "help",
            "create-skill",
            "code-review",
            "imagine",
            "check-work",
            "check",
            "best-of-n",
            "docx",
            "pptx",
            "xlsx",
        ];
        for name in skill_names {
            let dir = home.join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("custom {name}")).unwrap();
            std::fs::write(dir.join("user-file.txt"), "keep").unwrap();
        }

        extract_builtin_files(home);

        assert_ne!(
            std::fs::read_to_string(home.join("README.md")).unwrap(),
            "old"
        );
        for name in skill_names {
            let dir = home.join("skills").join(name);
            assert_eq!(
                std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                format!("custom {name}")
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("user-file.txt")).unwrap(),
                "keep"
            );
        }
    }

    #[test]
    fn builtin_skills_seed_once_and_never_clobber_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // Fresh home: every built-in skill is created with our shipped content.
        extract_builtin_files(home);
        for &(name, content) in BUILTIN_SKILLS {
            let path = home.join("skills").join(name).join("SKILL.md");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }

        // The user edits one skill; force a re-extraction with a stale marker.
        let edited_name = BUILTIN_SKILLS[0].0;
        let edited = home.join("skills").join(edited_name).join("SKILL.md");
        std::fs::write(&edited, "user edit").unwrap();
        std::fs::write(home.join(".metadata_version"), "0.0.0-stale").unwrap();
        extract_builtin_files(home);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "user edit");
        // The untouched sibling is left exactly as first seeded (not duplicated).
        let (other_name, other_content) = BUILTIN_SKILLS[1];
        let other = home.join("skills").join(other_name).join("SKILL.md");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), other_content);

        // Deleted marker + version bump: still must not clobber the edited skill.
        std::fs::remove_file(home.join(".metadata_version")).unwrap();
        extract_builtin_files(home);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "user edit");
    }

    #[test]
    fn same_version_does_not_restore_missing_or_delete_legacy_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("skills/check")).unwrap();
        std::fs::write(home.join("skills/check/SKILL.md"), "custom check").unwrap();
        std::fs::write(home.join(".metadata_version"), xai_grok_version::VERSION).unwrap();

        extract_builtin_files(home);

        assert!(!home.join("skills/help/SKILL.md").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("skills/check/SKILL.md")).unwrap(),
            "custom check"
        );
    }
}
