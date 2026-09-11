use super::common::SessionFileCandidate;
use super::common::detect_recent_sessions;
use crate::model::ExternalAgentSessionImportLimits;
use crate::sessions::ExternalAgentSessionMigration;
use crate::sessions::SessionRecordFormat;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const MAX_CUR_PROJECT_PATH_PROBES: usize = 128;
const CUR_PROJECT_SEPARATORS: [&str; 11] =
    ["-", "_", ".", " ", "--", "..", "__", "  ", "+", "@", "&"];

pub fn detect_recent_cur_sessions(
    external_agent_home: &Path,
    codex_home: &Path,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    detect_recent_cur_sessions_with_limits(
        external_agent_home,
        codex_home,
        ExternalAgentSessionImportLimits::default(),
    )
}

pub(crate) fn detect_recent_cur_sessions_with_limits(
    external_agent_home: &Path,
    codex_home: &Path,
    limits: ExternalAgentSessionImportLimits,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    let projects_root = external_agent_home.join("projects");
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for project_entry in fs::read_dir(projects_root)? {
        let Ok(project_entry) = project_entry else {
            continue;
        };
        let project_storage = project_entry.path();
        if !project_storage.is_dir() {
            continue;
        }
        let fallback_cwd = cur_project_cwd(&project_storage, external_agent_home);
        for path in cur_transcript_files(&project_storage.join("agent-transcripts")) {
            candidates.push(SessionFileCandidate {
                path,
                fallback_cwd: fallback_cwd.clone(),
                record_format: SessionRecordFormat::Cur,
            });
        }
    }
    detect_recent_sessions(
        codex_home, candidates, /*require_existing_cwd*/ false, limits,
    )
}

fn cur_transcript_files(transcripts_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![transcripts_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if entry.file_name() != "subagents" {
                    pending.push(path);
                }
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn cur_project_cwd(project_storage: &Path, external_agent_home: &Path) -> Option<PathBuf> {
    let encoded = project_storage.file_name()?.to_str()?;
    // Cursor stores projectless chats under this reserved project name.
    if encoded == "empty-window" {
        let external_agent_home = if external_agent_home.is_absolute() {
            external_agent_home.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(external_agent_home)
        };
        return external_agent_home.parent().map(Path::to_path_buf);
    }
    decode_cur_project_path(encoded)
}

fn decode_cur_project_path(encoded: &str) -> Option<PathBuf> {
    #[cfg(not(windows))]
    let root = PathBuf::from("/");

    #[cfg(windows)]
    let (encoded, root) = {
        let (drive, encoded) = decode_cur_windows_project_drive(encoded)?;
        (encoded, PathBuf::from(format!("{drive}:\\")))
    };

    let encoded = encoded.strip_prefix('-').unwrap_or(encoded);
    let components = encoded.split('-').collect::<Vec<_>>();
    if components.iter().any(|component| {
        component.is_empty()
            || matches!(*component, "." | "..")
            || component.contains(['/', '\\', ':'])
    }) {
        return None;
    }

    let mut probes = 0;
    decode_cur_project_components(&root, &components, &mut probes)
}

/// Resolves the encoded `components` below `base`. Cursor replaced every path
/// separator and every character in [`CUR_PROJECT_SEPARATORS`] with `-`, so
/// each `-` may be a directory boundary or part of a name. The search probes
/// the literal path, merged trailing names, and merged ancestor pairs, and
/// resolves the remaining components below a merged ancestor the same way.
/// Two distinct existing directories with the same encoding, or a search that
/// exhausts the probe budget, decode to `None`.
fn decode_cur_project_components(
    base: &Path,
    components: &[&str],
    probes: &mut usize,
) -> Option<PathBuf> {
    let join = |components: &[&str]| {
        components
            .iter()
            .fold(base.to_path_buf(), |path, component| path.join(component))
    };
    let mut matched_path = None;
    match_cur_project_candidate(join(components), &mut matched_path, probes)?;

    for suffix_length in 2..=4 {
        if suffix_length > components.len() {
            break;
        }
        let (parent, suffix) = components.split_at(components.len() - suffix_length);
        let parent = join(parent);
        if !probe_cur_project_dir(&parent, probes)? {
            continue;
        }
        for separator in CUR_PROJECT_SEPARATORS {
            match_cur_project_candidate(
                parent.join(suffix.join(separator)),
                &mut matched_path,
                probes,
            )?;
        }
    }

    for right in (1..components.len().saturating_sub(1)).rev() {
        let left = right - 1;
        let prefix = join(&components[..left]);
        if !probe_cur_project_dir(&prefix, probes)? {
            continue;
        }
        let left_name = components[left];
        let right_name = components[right];
        let trailing = &components[right + 1..];
        for separator in CUR_PROJECT_SEPARATORS {
            let merged_prefix = prefix.join(format!("{left_name}{separator}{right_name}"));
            if !probe_cur_project_dir(&merged_prefix, probes)? {
                continue;
            }
            let candidate = decode_cur_project_components(&merged_prefix, trailing, probes)?;
            if matched_path
                .as_ref()
                .is_some_and(|matched_path| matched_path != &candidate)
            {
                return None;
            }
            matched_path = Some(candidate);
        }
    }

    matched_path
}

/// Spends one probe of the budget on a directory check; `None` once the
/// budget is exhausted.
fn probe_cur_project_dir(path: &Path, probes: &mut usize) -> Option<bool> {
    if *probes >= MAX_CUR_PROJECT_PATH_PROBES {
        return None;
    }
    *probes += 1;
    Some(path.is_dir())
}

/// Records `candidate` when it exists; `None` when the budget is exhausted or
/// a different directory already matched.
fn match_cur_project_candidate(
    candidate: PathBuf,
    matched_path: &mut Option<PathBuf>,
    probes: &mut usize,
) -> Option<()> {
    if !probe_cur_project_dir(&candidate, probes)? {
        return Some(());
    }
    if matched_path
        .as_ref()
        .is_some_and(|matched_path| matched_path != &candidate)
    {
        return None;
    }
    *matched_path = Some(candidate);
    Some(())
}

#[cfg(any(windows, test))]
fn decode_cur_windows_project_drive(encoded: &str) -> Option<(char, &str)> {
    let drive = encoded.as_bytes().first().copied()?;
    if !drive.is_ascii_alphabetic() || encoded.as_bytes().get(1) != Some(&b'-') {
        return None;
    }

    Some((char::from(drive), encoded.get(2..)?))
}

#[cfg(test)]
#[path = "cur_tests.rs"]
mod tests;
