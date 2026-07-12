use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::model::{Client, LineRecord, ParseBatch, SourceSpec};

pub mod claude;
pub mod codex;

/// Maximum source paths retained in one built-in discovery candidate page.
pub(crate) const MAX_DISCOVERED_SOURCES: usize = 100_000;
/// Maximum directory entries admitted by one built-in traversal. One extra
/// sentinel read may be used to distinguish exact exhaustion from truncation.
pub(crate) const MAX_DISCOVERY_ENTRIES: usize = 250_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryRequest {
    /// A bounded, non-secret rotation seed. Persisted scan-run ids provide this
    /// for normal scans; dry-runs deliberately use zero for reproducibility.
    pub rotation_seed: u64,
    pub max_sources: usize,
    pub max_entries: usize,
}

impl DiscoveryRequest {
    pub(crate) fn scanner(rotation_seed: usize, max_sources: usize, max_entries: usize) -> Self {
        Self {
            rotation_seed: u64::try_from(rotation_seed).unwrap_or(u64::MAX),
            max_sources,
            max_entries,
        }
    }
}

impl Default for DiscoveryRequest {
    fn default() -> Self {
        Self {
            rotation_seed: 0,
            max_sources: MAX_DISCOVERED_SOURCES,
            max_entries: MAX_DISCOVERY_ENTRIES,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub sources: Vec<SourceSpec>,
    /// Relevant source candidates observed inside this invocation's bounded
    /// traversal. When `entry_limit_reached` is true this is a lower bound.
    pub observed_source_count: usize,
    /// Directory entries read, including at most one truncation sentinel.
    pub inspected_entry_count: usize,
    pub source_limit_reached: bool,
    pub entry_limit_reached: bool,
    /// A nested entry could not be inspected. Safe candidates are retained,
    /// but discovery coverage cannot be claimed complete.
    pub io_incomplete: bool,
}

impl DiscoveryResult {
    pub fn complete(sources: Vec<SourceSpec>) -> Self {
        let observed_source_count = sources.len();
        Self {
            sources,
            observed_source_count,
            inspected_entry_count: 0,
            source_limit_reached: false,
            entry_limit_reached: false,
            io_incomplete: false,
        }
    }

    pub fn truncated(&self) -> bool {
        self.source_limit_reached || self.entry_limit_reached || self.io_incomplete
    }
}

pub trait SourceAdapter: Send + Sync {
    fn client(&self) -> Client;
    fn display_name(&self) -> &'static str;
    /// Convenience for callers that only need the seed-zero bounded page.
    /// Completeness-sensitive callers must use `discover_bounded` and inspect
    /// its structured partial-result flags.
    fn discover(&self, config: &Config) -> Result<Vec<SourceSpec>> {
        Ok(self
            .discover_bounded(config, DiscoveryRequest::default())?
            .sources)
    }
    fn discover_bounded(
        &self,
        config: &Config,
        request: DiscoveryRequest,
    ) -> Result<DiscoveryResult>;
    fn parse_lines(
        &self,
        path: &Path,
        lines: &[LineRecord],
        previous_state: Option<&Value>,
    ) -> Result<ParseBatch>;
}

#[derive(Debug, Clone, Copy, Default)]
struct WalkStatus {
    entry_limit_reached: bool,
    io_incomplete: bool,
    inspected_entry_count: usize,
}

/// Performs one bounded pass over a deterministic, seed-rotated traversal and
/// materializes one source-candidate page without stopping candidate counting.
/// At most `max_entries + 1` directory entries are inspected and at most
/// `2 * max_sources` paths are retained (the selected page plus a useful
/// fallback page when the seeded page lies beyond the observed candidate set).
/// The traversal frontier itself retains at most `max_entries + roots.len()`
/// additional paths, so both work and memory remain bounded.
///
/// Portable directory iterators expose no resumable opaque cursor. Therefore
/// an entry-capped pathological flat directory or single deep chain cannot be
/// proven exhaustive without violating the hard work bound; such results keep
/// `entry_limit_reached` set and are never reported as complete.
pub(crate) fn discover_bounded_files<F>(
    roots: &[PathBuf],
    request: DiscoveryRequest,
    classify: F,
) -> Result<DiscoveryResult>
where
    F: Fn(&Path) -> Result<Option<SourceSpec>>,
{
    if request.max_entries == 0 {
        return Ok(DiscoveryResult {
            entry_limit_reached: !roots.is_empty(),
            ..Default::default()
        });
    }
    let max_sources = request.max_sources.min(MAX_DISCOVERED_SOURCES);
    let selection_capacity = max_sources.max(1);
    let max_entries = request.max_entries.min(MAX_DISCOVERY_ENTRIES);
    let page_slots = max_entries.div_ceil(selection_capacity).max(1);
    let traversal_seed = request.rotation_seed / u64::try_from(page_slots).unwrap_or(u64::MAX);

    let page_hint =
        usize::try_from(request.rotation_seed % u64::try_from(page_slots).unwrap_or(u64::MAX))
            .unwrap_or(0);
    let start = page_hint.saturating_mul(selection_capacity);
    let end = if max_sources == 0 {
        start
    } else {
        start.saturating_add(selection_capacity)
    };
    let mut observed_source_count = 0_usize;
    let mut sources = Vec::with_capacity(max_sources);
    let mut fallback_sources = Vec::with_capacity(max_sources);
    let retain_fallback = start > 0;
    let status = walk_files_bounded(roots, max_entries, traversal_seed, |path| {
        let Some(source) = classify(path)? else {
            return Ok(());
        };
        let candidate_index = observed_source_count;
        observed_source_count = observed_source_count.saturating_add(1);
        if retain_fallback && fallback_sources.len() < max_sources {
            fallback_sources.push(source.clone());
        }
        if (start..end).contains(&candidate_index) && sources.len() < max_sources {
            sources.push(source);
        }
        Ok(())
    })?;
    if sources.is_empty() && observed_source_count > 0 && max_sources > 0 {
        sources = fallback_sources;
    }
    debug_assert!(sources.len() <= max_sources);
    debug_assert!(status.inspected_entry_count <= max_entries.saturating_add(1));
    Ok(DiscoveryResult {
        sources,
        observed_source_count,
        inspected_entry_count: status.inspected_entry_count,
        source_limit_reached: observed_source_count > max_sources,
        entry_limit_reached: status.entry_limit_reached,
        io_incomplete: status.io_incomplete,
    })
}

fn walk_files_bounded<F>(
    roots: &[PathBuf],
    max_entries: usize,
    rotation_seed: u64,
    mut visit_file: F,
) -> Result<WalkStatus>
where
    F: FnMut(&Path) -> Result<()>,
{
    let mut roots = roots.to_vec();
    roots.sort();
    rotate_paths(&mut roots, rotation_seed);
    let mut stack = roots.into_iter().rev().collect::<Vec<_>>();
    let mut inspected_entries = 0_usize;
    let mut inspected_entry_count = 0_usize;
    let mut entry_limit_reached = false;
    let mut io_incomplete = false;

    while let Some(path) = stack.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                io_incomplete = true;
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            // Links remain unfollowed, but omitting a readable linked source is
            // still a coverage limitation rather than complete discovery.
            io_incomplete = true;
            continue;
        }
        if metadata.is_file() {
            if visit_file(&path).is_err() {
                io_incomplete = true;
            }
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        if inspected_entries >= max_entries {
            entry_limit_reached = true;
            continue;
        }

        let mut children = Vec::new();
        let mut entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(_) => {
                io_incomplete = true;
                continue;
            }
        };
        while inspected_entries < max_entries {
            let Some(entry) = entries.next() else {
                break;
            };
            inspected_entries += 1;
            inspected_entry_count += 1;
            match entry {
                Ok(entry) => children.push(entry.path()),
                Err(_) => io_incomplete = true,
            }
        }
        if inspected_entries >= max_entries
            && let Some(sentinel) = entries.next()
        {
            // One sentinel read distinguishes exact exhaustion from truncation
            // while keeping the per-pass work bound fixed at max_entries + 1.
            entry_limit_reached = true;
            inspected_entry_count += 1;
            match sentinel {
                Ok(entry) => match entry.file_type() {
                    Ok(file_type) if file_type.is_symlink() => io_incomplete = true,
                    Ok(_) => {}
                    Err(_) => io_incomplete = true,
                },
                Err(_) => io_incomplete = true,
            }
        }
        children.sort();
        rotate_paths(&mut children, rotation_seed);
        stack.extend(children.into_iter().rev());
    }
    Ok(WalkStatus {
        entry_limit_reached,
        io_incomplete,
        inspected_entry_count,
    })
}

fn rotate_paths(paths: &mut [PathBuf], rotation_seed: u64) {
    if paths.len() > 1 {
        let rotation = usize::try_from(rotation_seed % paths.len() as u64).unwrap_or(0);
        paths.rotate_left(rotation);
    }
}

pub fn built_in_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter),
        Box::new(codex::CodexAdapter),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io;

    use tempfile::tempdir;

    use super::*;

    fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file symlinks are unsupported on this platform",
            ))
        }
    }

    fn try_create_file_symlink(target: &Path, link: &Path) -> Result<bool> {
        match create_file_symlink(target, link) {
            Ok(()) => Ok(true),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                ) || error.raw_os_error() == Some(1314) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn classify_jsonl(path: &Path) -> Result<Option<SourceSpec>> {
        Ok(path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            .then(|| SourceSpec {
                path: path.to_path_buf(),
                client: Client::ClaudeCode,
                compressed: false,
            }))
    }

    #[test]
    fn source_candidate_pages_eventually_expose_every_completed_traversal_candidate() -> Result<()>
    {
        let directory = tempdir()?;
        for index in 0..5 {
            fs::write(directory.path().join(format!("{index}.jsonl")), "")?;
        }

        let mut observed = HashSet::new();
        for rotation_seed in 0..3 {
            let result = discover_bounded_files(
                &[directory.path().to_path_buf()],
                DiscoveryRequest {
                    rotation_seed,
                    max_sources: 2,
                    max_entries: 10,
                },
                classify_jsonl,
            )?;
            assert_eq!(result.observed_source_count, 5);
            assert!(result.source_limit_reached);
            assert!(!result.entry_limit_reached);
            assert!(result.sources.len() <= 2);
            assert!(result.inspected_entry_count <= 11);
            observed.extend(result.sources.into_iter().map(|source| source.path));
        }
        assert_eq!(observed.len(), 5);
        Ok(())
    }

    #[test]
    fn entry_cap_returns_useful_partial_candidates_instead_of_an_error() -> Result<()> {
        let directory = tempdir()?;
        for index in 0..3 {
            fs::write(directory.path().join(format!("{index}.jsonl")), "")?;
        }

        let result = discover_bounded_files(
            &[directory.path().to_path_buf()],
            DiscoveryRequest {
                rotation_seed: 0,
                max_sources: 2,
                max_entries: 2,
            },
            classify_jsonl,
        )?;
        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.observed_source_count, 2);
        assert!(result.entry_limit_reached);
        assert!(result.truncated());
        assert!(result.inspected_entry_count <= 3);
        Ok(())
    }

    #[test]
    fn entry_capped_nested_traversal_rotates_the_leading_branch() -> Result<()> {
        let directory = tempdir()?;
        for branch in ["a", "b"] {
            let branch = directory.path().join(branch);
            fs::create_dir(&branch)?;
            for index in 0..2 {
                fs::write(branch.join(format!("{index}.jsonl")), "")?;
            }
        }

        let request = |rotation_seed| DiscoveryRequest {
            rotation_seed,
            max_sources: 4,
            max_entries: 4,
        };
        let first = discover_bounded_files(
            &[directory.path().to_path_buf()],
            request(0),
            classify_jsonl,
        )?;
        let second = discover_bounded_files(
            &[directory.path().to_path_buf()],
            request(1),
            classify_jsonl,
        )?;
        assert!(first.entry_limit_reached);
        assert!(second.entry_limit_reached);
        assert!(first.inspected_entry_count <= 5);
        assert!(second.inspected_entry_count <= 5);
        assert_eq!(first.sources.len(), 2);
        assert_eq!(second.sources.len(), 2);
        let branches = first
            .sources
            .iter()
            .chain(&second.sources)
            .filter_map(|source| {
                source
                    .path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
            })
            .collect::<HashSet<_>>();
        assert_eq!(branches, HashSet::from(["a", "b"]));
        Ok(())
    }

    #[test]
    fn flat_suffix_beyond_entry_cap_is_never_mislabeled_complete() -> Result<()> {
        let directory = tempdir()?;
        for index in 0..5 {
            fs::write(directory.path().join(format!("{index}.jsonl")), "")?;
        }

        for rotation_seed in 0..4 {
            let result = discover_bounded_files(
                &[directory.path().to_path_buf()],
                DiscoveryRequest {
                    rotation_seed,
                    max_sources: 2,
                    max_entries: 2,
                },
                classify_jsonl,
            )?;
            // Portable directory iterators have no resumable opaque cursor.
            // The hard enumeration ceiling therefore remains an explicit,
            // honest partial result for a pathological wide flat directory.
            assert!(result.entry_limit_reached);
            assert!(result.truncated());
        }
        Ok(())
    }

    #[test]
    fn deep_suffix_beyond_entry_cap_keeps_safe_prefix_but_remains_partial() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("a-safe.jsonl"), "")?;
        let deep = directory.path().join("z-deep");
        fs::create_dir(&deep)?;
        fs::write(deep.join("hidden.jsonl"), "")?;

        let result = discover_bounded_files(
            &[directory.path().to_path_buf()],
            DiscoveryRequest {
                rotation_seed: 0,
                max_sources: 2,
                max_entries: 2,
            },
            classify_jsonl,
        )?;
        assert_eq!(result.sources.len(), 1);
        assert!(result.sources[0].path.ends_with("a-safe.jsonl"));
        assert!(result.entry_limit_reached);
        assert!(result.truncated());
        Ok(())
    }

    #[test]
    fn classifier_error_keeps_safe_candidates_and_marks_io_partial() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("a-good.jsonl"), "")?;
        fs::write(directory.path().join("b-bad.jsonl"), "")?;
        fs::write(directory.path().join("c-good.jsonl"), "")?;

        let result = discover_bounded_files(
            &[directory.path().to_path_buf()],
            DiscoveryRequest {
                rotation_seed: 0,
                max_sources: 3,
                max_entries: 4,
            },
            |path| {
                if path.file_name().is_some_and(|name| name == "b-bad.jsonl") {
                    anyhow::bail!("private classifier diagnostic must not be retained");
                }
                classify_jsonl(path)
            },
        )?;
        assert!(result.io_incomplete);
        assert!(result.truncated());
        assert_eq!(result.sources.len(), 2);
        assert!(
            result
                .sources
                .iter()
                .all(|source| !source.path.ends_with("b-bad.jsonl"))
        );
        assert!(!format!("{result:?}").contains("private classifier diagnostic"));
        Ok(())
    }

    #[test]
    fn symlink_candidate_is_not_followed_and_marks_discovery_partial() -> Result<()> {
        let directory = tempdir()?;
        let outside = tempdir()?;
        let real_source = directory.path().join("real.jsonl");
        let outside_target = outside.path().join("outside-target.jsonl");
        let linked_source = directory.path().join("linked-source.jsonl");
        fs::write(&real_source, "")?;
        fs::write(&outside_target, "")?;
        if !try_create_file_symlink(&outside_target, &linked_source)? {
            return Ok(());
        }

        let result = discover_bounded_files(
            &[directory.path().to_path_buf()],
            DiscoveryRequest {
                rotation_seed: 0,
                max_sources: 4,
                max_entries: 4,
            },
            classify_jsonl,
        )?;
        assert!(result.io_incomplete);
        assert!(result.truncated());
        assert_eq!(result.observed_source_count, 1);
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].path, real_source);
        assert!(
            result
                .sources
                .iter()
                .all(|source| source.path != linked_source && source.path != outside_target)
        );
        Ok(())
    }

    #[test]
    fn zero_source_bound_retains_no_paths_and_reports_truncation() -> Result<()> {
        let directory = tempdir()?;
        fs::write(directory.path().join("source.jsonl"), "")?;
        let result = discover_bounded_files(
            &[directory.path().to_path_buf()],
            DiscoveryRequest {
                rotation_seed: 0,
                max_sources: 0,
                max_entries: 2,
            },
            classify_jsonl,
        )?;
        assert!(result.sources.is_empty());
        assert_eq!(result.observed_source_count, 1);
        assert!(result.source_limit_reached);
        Ok(())
    }
}
