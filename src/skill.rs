//! Installing the agent skill.
//!
//! The skill is one Markdown file telling a coding agent which `hototel`
//! subcommands answer which questions. It ships inside this binary rather than
//! as a separate download, so installing hototel installs the skill too --
//! that is the whole point of there being one thing to install.
//!
//! The binary's own path is substituted in, because `~/.local/bin` is often
//! missing from the environment an agent runs commands in; an absolute path
//! always works.

use crate::parsers::home_dir;
use std::fs;
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("../skill/SKILL.md");
const NAME: &str = "hototel";

/// Where each agent keeps its skills, and what to call it in a message.
fn targets() -> Vec<(&'static str, PathBuf)> {
    vec![
        ("Claude Code", home_dir().join(".claude")),
        ("Codex", home_dir().join(".codex")),
    ]
}

/// The command the skill file tells the agent to run.
///
/// Quoted when the path contains whitespace: SKILL.md uses it bare in fenced
/// code blocks, and `C:\Users\Jane Doe\...\hototel summary` would otherwise be
/// two words -- "command not found" for every documented invocation.
fn quote_if_spaced(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

fn binary_path() -> String {
    let path = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        // if the exe path cannot be read, the bare name is still right for
        // anyone whose PATH includes the install directory
        .unwrap_or_else(|| NAME.to_string());
    quote_if_spaced(&path)
}

/// Present in every file this program writes, so uninstall can tell its own
/// file from one somebody hand-wrote under the same name.
const MARKER: &str = "<!-- installed by hototel; local edits are overwritten -->";

fn rendered() -> String {
    // CRLF is normalized away, not tolerated: on Windows git checks this file
    // out with CRLF unless .gitattributes says otherwise, include_str! embeds
    // exactly what is on disk, and the frontmatter an agent parses would then
    // start "---\r\n". The file we write is LF on every platform.
    let body = TEMPLATE.replace("\r\n", "\n").replace("{{BIN}}", &binary_path());
    format!("{body}\n{MARKER}\n")
}

fn is_ours(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

/// The skill was installed under the old binary name (with the old marker)
/// before the hototel rename. Two skills answering the same questions would
/// race each other in the agent's skill listing, so retire the old one.
/// Best-effort, and only for a file the old binary wrote.
const LEGACY_NAME: &str = "hotusage";
const LEGACY_MARKER: &str = "<!-- installed by hotusage;";

fn remove_legacy(root: &Path) {
    let dir = root.join("skills").join(LEGACY_NAME);
    let path = dir.join("SKILL.md");
    let is_legacy_ours = fs::read_to_string(&path)
        .map(|s| s.contains(LEGACY_MARKER))
        .unwrap_or(false);
    if is_legacy_ours {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir); // only when empty; siblings are not ours
    }
}

fn write_skill(root: &Path) -> std::io::Result<PathBuf> {
    let dir = root.join("skills").join(NAME);
    fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    fs::write(&path, rendered())?;
    Ok(path)
}

/// Install into every agent found (or all of them, when `force`). Returns one
/// line per install for the caller to print.
///
/// Silence when nothing is found is deliberate: `install` runs this as a side
/// step, and someone who uses neither agent should not be told about a skill
/// they did not ask for.
pub fn install(force: bool) -> Vec<String> {
    let mut done = Vec::new();
    for (label, root) in targets() {
        if !root.is_dir() && !force {
            continue;
        }
        // Someone else's skill under this name is theirs, not ours to replace.
        // `skill install`, which is asked for by name, may still overwrite --
        // that is how an edited copy is reset.
        let existing = root.join("skills").join(NAME).join("SKILL.md");
        if existing.is_file() && !is_ours(&existing) && !force {
            done.push(format!(
                "left the existing {label} skill at {} alone (not written by \
                 hototel); run `hototel skill install` to replace it",
                existing.display()
            ));
            continue;
        }
        // Only retire the old hotusage skill once a replacement is certain to
        // be written: above the guard, a hand-written skills/hototel would
        // have cost the user their working hotusage skill with nothing new.
        remove_legacy(&root);
        match write_skill(&root) {
            Ok(path) => done.push(format!("skill installed for {label}: {}", path.display())),
            Err(e) => done.push(format!("could not install the skill for {label}: {e}")),
        }
    }
    if done.is_empty() && force {
        done.push("no agent directory found (~/.claude, ~/.codex)".into());
    }
    done
}

/// Remove the skill wherever it was installed. Used by `uninstall`, so the
/// machine is left as it was found.
pub fn uninstall() -> Vec<String> {
    let mut done = Vec::new();
    for (label, root) in targets() {
        let dir = root.join("skills").join(NAME);
        let path = dir.join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        if !is_ours(&path) {
            done.push(format!(
                "left {} alone (not written by hototel)",
                path.display()
            ));
            continue;
        }
        // Remove the file this program wrote, never the directory wholesale:
        // a `references/` or `scripts/` folder beside it belongs to whoever
        // put it there. The now-empty directory goes only if it is empty.
        match fs::remove_file(&path) {
            Ok(()) => {
                let _ = fs::remove_dir(&dir); // fails harmlessly if not empty
                done.push(format!("skill removed for {label}"));
            }
            Err(e) => done.push(format!("could not remove the {label} skill: {e}")),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_file_is_a_usable_skill_file() {
        // Assert on what is WRITTEN, not on the embedded template: a Windows
        // checkout can hand include_str! CRLF, and the agent only ever sees
        // the rendered output. Frontmatter is what makes a skill load at all.
        let out = rendered();
        assert!(out.starts_with("---\n"), "needs YAML frontmatter");
        assert!(out.contains("\nname: hototel\n"));
        assert!(out.contains("\ndescription: "));
        assert!(!out.contains('\r'), "CRLF must not reach the agent");
        assert!(TEMPLATE.contains("{{BIN}}"), "nothing to substitute");
    }

    #[test]
    fn the_skill_documents_the_provider_ids_the_parsers_actually_emit() {
        // Found against real data: the file said `claude-code`, nothing emits
        // that, and --provider is a substring match -- so the agent would have
        // reported "nobody uses Claude Code" rather than erroring. Pin the
        // documentation to the constants the parsers use.
        let out = rendered();
        for p in crate::core::PROVIDERS {
            assert!(out.contains(&format!("`{p}`")), "SKILL.md never names `{p}`");
        }
        assert!(
            !out.contains("`claude-code`"),
            "SKILL.md documents a provider id nothing emits"
        );
    }

    #[test]
    fn rendering_leaves_no_placeholder_behind() {
        let out = rendered();
        assert!(!out.contains("{{BIN}}"), "a placeholder survived");
        assert!(!out.contains("{{"), "an unrendered placeholder survived");
        // and the real command name still appears, so the agent can run it
        assert!(out.contains("signin"));
    }

    #[test]
    fn writing_creates_the_nested_skill_directory() {
        let root = std::env::temp_dir().join(format!("hototel-skill-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = write_skill(&root).expect("write");
        assert!(path.ends_with("skills/hototel/SKILL.md"), "{path:?}");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("name: hototel"));
        assert!(!body.contains("{{BIN}}"));
        // re-installing must overwrite rather than fail: that is the upgrade
        write_skill(&root).expect("overwrite");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_with_a_space_is_quoted() {
        // the path is substituted into fenced code blocks; unquoted, a home
        // directory with a space makes every documented command fail
        assert_eq!(quote_if_spaced("/opt/hototel"), "/opt/hototel");
        assert_eq!(
            quote_if_spaced("/Users/Jane Doe/bin/hototel"),
            "\"/Users/Jane Doe/bin/hototel\""
        );
    }

    #[test]
    fn what_we_write_is_recognisably_ours() {
        let root = std::env::temp_dir().join(format!("hu-marker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = write_skill(&root).unwrap();
        assert!(is_ours(&path), "uninstall could not tell this was ours");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn uninstall_spares_a_skill_this_program_did_not_write() {
        let root = std::env::temp_dir().join(format!("hu-foreign-{}", std::process::id()));
        let dir = root.join("skills").join(NAME);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, "---\nname: hototel\n---\nsomeone's own skill").unwrap();
        assert!(!is_ours(&path), "a hand-written file must not look like ours");

        // a sibling file belongs to whoever put it there; remove_dir_all,
        // which this used to do, would have taken it along
        let sibling = dir.join("notes.md");
        fs::write(&sibling, "keep me").unwrap();
        write_skill(&root).unwrap();
        assert!(sibling.is_file(), "a sibling file must survive an install");
        let _ = fs::remove_dir_all(&root);
    }
}
