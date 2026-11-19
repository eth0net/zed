use crate::{
    project_settings::ProjectSettings,
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use anyhow::{Context as _, Result};
use collections::HashMap;
use fs::{Fs, MTime};
use gpui::{App, Context, Entity, Subscription, Task};
use language::{Buffer, BufferId, Point};
use quick_xml::{XmlVersion, events::Event};
use settings::{Settings as _, SettingsLocation, SettingsStore};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use text::Edit;
use util::{ResultExt as _, rel_path::RelPath};
use worktree::{UpdatedEntriesSet, Worktree, WorktreeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineCoverage {
    Covered,
    /// Executed, but some of the line's branches were never taken.
    Partial,
    Uncovered,
}

/// Coverage for one file, keyed by zero-based row.
#[derive(Debug, Default, PartialEq)]
pub struct FileCoverage {
    pub lines: BTreeMap<u32, LineCoverage>,
}

#[derive(Debug, Default)]
struct LineCounts {
    hits: u64,
    branches: u32,
    branches_taken: u32,
}

impl LineCounts {
    fn status(&self) -> LineCoverage {
        if self.hits == 0 {
            LineCoverage::Uncovered
        } else if self.branches_taken < self.branches {
            LineCoverage::Partial
        } else {
            LineCoverage::Covered
        }
    }
}

fn into_file_coverage(
    files: HashMap<PathBuf, BTreeMap<u32, LineCounts>>,
) -> HashMap<PathBuf, FileCoverage> {
    files
        .into_iter()
        .map(|(path, lines)| {
            let lines = lines
                .into_iter()
                .map(|(row, counts)| (row, counts.status()))
                .collect();
            (path, FileCoverage { lines })
        })
        .collect()
}

/// Parses an lcov tracefile. Paths are returned as written.
pub fn parse_lcov(text: &str) -> HashMap<PathBuf, FileCoverage> {
    let mut files: HashMap<PathBuf, BTreeMap<u32, LineCounts>> = HashMap::default();
    let mut current = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("SF:") {
            current = Some(PathBuf::from(path));
        } else if line == "end_of_record" {
            current = None;
        } else if let Some(path) = &current {
            if let Some(record) = line.strip_prefix("DA:") {
                let mut fields = record.split(',');
                let (Some(Ok(line_number)), Some(Ok(hits))) = (
                    fields.next().map(str::parse::<u32>),
                    fields.next().map(str::parse::<u64>),
                ) else {
                    continue;
                };
                let Some(row) = line_number.checked_sub(1) else {
                    continue;
                };
                let counts = files
                    .entry(path.clone())
                    .or_default()
                    .entry(row)
                    .or_default();
                counts.hits += hits;
            } else if let Some(record) = line.strip_prefix("BRDA:") {
                let mut fields = record.split(',');
                let Some(Ok(line_number)) = fields.next().map(str::parse::<u32>) else {
                    continue;
                };
                let Some(row) = line_number.checked_sub(1) else {
                    continue;
                };
                let taken = fields
                    .nth(2)
                    .is_some_and(|taken| taken.parse::<u64>().is_ok_and(|count| count > 0));
                let counts = files
                    .entry(path.clone())
                    .or_default()
                    .entry(row)
                    .or_default();
                counts.branches += 1;
                if taken {
                    counts.branches_taken += 1;
                }
            }
        }
    }
    into_file_coverage(files)
}

/// Parses a Cobertura XML report. Relative file names are joined to the report's first
/// `<source>`, when there is one.
pub fn parse_cobertura(text: &str) -> Result<HashMap<PathBuf, FileCoverage>> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut files: HashMap<PathBuf, BTreeMap<u32, LineCounts>> = HashMap::default();
    let mut source: Option<PathBuf> = None;
    let mut in_source = false;
    let mut current: Option<PathBuf> = None;
    loop {
        match reader.read_event().context("parsing Cobertura report")? {
            Event::Eof => break,
            Event::Start(element) if element.local_name().as_ref() == b"source" => {
                in_source = source.is_none();
            }
            Event::End(element) if element.local_name().as_ref() == b"source" => {
                in_source = false;
            }
            Event::Text(text) if in_source => {
                let text = text.decode()?;
                let text = text.trim();
                if !text.is_empty() {
                    source = Some(PathBuf::from(text));
                }
            }
            Event::Start(element) if element.local_name().as_ref() == b"class" => {
                current = None;
                for attribute in element.attributes().flatten() {
                    if attribute.key.local_name().as_ref() == b"filename" {
                        current = Some(PathBuf::from(
                            attribute.normalized_value(XmlVersion::default())?.as_ref(),
                        ));
                    }
                }
            }
            Event::End(element) if element.local_name().as_ref() == b"class" => {
                current = None;
            }
            Event::Start(element) | Event::Empty(element)
                if element.local_name().as_ref() == b"line" =>
            {
                let Some(path) = &current else {
                    continue;
                };
                let mut line_number = None;
                let mut hits = None;
                let mut branches = None;
                for attribute in element.attributes().flatten() {
                    let value = attribute.normalized_value(XmlVersion::default())?;
                    match attribute.key.local_name().as_ref() {
                        b"number" => line_number = value.parse::<u32>().ok(),
                        b"hits" => hits = value.parse::<u64>().ok(),
                        b"condition-coverage" => branches = parse_condition_coverage(&value),
                        _ => {}
                    }
                }
                let (Some(row), Some(hits)) =
                    (line_number.and_then(|number| number.checked_sub(1)), hits)
                else {
                    continue;
                };
                let counts = files
                    .entry(path.clone())
                    .or_default()
                    .entry(row)
                    .or_default();
                counts.hits += hits;
                if let Some((taken, total)) = branches {
                    counts.branches = counts.branches.max(total);
                    counts.branches_taken = counts.branches_taken.max(taken);
                }
            }
            _ => {}
        }
    }
    let files = match source {
        Some(source) => files
            .into_iter()
            .map(|(path, lines)| {
                let path = if path.is_absolute() {
                    path
                } else {
                    source.join(path)
                };
                (path, lines)
            })
            .collect(),
        None => files,
    };
    Ok(into_file_coverage(files))
}

/// Reads the `(1/2)` out of `50% (1/2)`.
fn parse_condition_coverage(value: &str) -> Option<(u32, u32)> {
    let counts = value.split_once('(')?.1.strip_suffix(')')?;
    let (taken, total) = counts.split_once('/')?;
    Some((taken.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn parse_report(path: &Path, text: &str) -> Result<HashMap<PathBuf, FileCoverage>> {
    let is_xml = path.extension().is_some_and(|extension| extension == "xml")
        || text.trim_start().starts_with('<');
    if is_xml {
        parse_cobertura(text)
    } else {
        Ok(parse_lcov(text))
    }
}

/// Removes `.` and `..` without touching the filesystem, since report paths often name
/// files on another machine or in a container.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component),
        }
    }
    normalized
}

/// Resolves a path written in a report to an existing file in the worktree. A relative
/// path may be relative to the report's directory or to the worktree root.
async fn resolve_report_path(
    fs: &dyn Fs,
    report_path: &Path,
    worktree_root: &Path,
    report_dir: &Path,
) -> Option<Arc<RelPath>> {
    let candidates = if report_path.is_absolute() {
        vec![normalize(report_path)]
    } else {
        vec![
            normalize(&worktree_root.join(report_path)),
            normalize(&report_dir.join(report_path)),
        ]
    };
    for candidate in candidates {
        let Ok(relative) = candidate.strip_prefix(worktree_root) else {
            continue;
        };
        let Ok(relative) = RelPath::new(relative, util::paths::PathStyle::local()) else {
            continue;
        };
        if fs.is_file(&candidate).await {
            return Some(relative.into_arc());
        }
    }
    None
}

struct Report {
    modified: MTime,
    files: HashMap<Arc<RelPath>, Arc<FileCoverage>>,
}

struct WorktreeCoverage {
    root: Arc<Path>,
    report_paths: Vec<Arc<RelPath>>,
    reports: HashMap<Arc<RelPath>, Report>,
    loads: HashMap<Arc<RelPath>, Task<()>>,
}

struct BufferCoverage {
    file: Arc<FileCoverage>,
    /// The buffer version whose text the report describes.
    base_version: clock::Global,
    mapped: Option<(clock::Global, Arc<HashMap<u32, LineCoverage>>)>,
}

/// Loads coverage reports from each local worktree and maps them onto open buffers.
pub struct CoverageStore {
    fs: Arc<dyn Fs>,
    worktree_store: Entity<WorktreeStore>,
    worktrees: HashMap<WorktreeId, WorktreeCoverage>,
    buffers: HashMap<BufferId, BufferCoverage>,
    _subscriptions: Vec<Subscription>,
}

impl CoverageStore {
    pub fn new(
        fs: Arc<dyn Fs>,
        worktree_store: Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&worktree_store, |this, _, event, cx| match event {
                WorktreeStoreEvent::WorktreeAdded(_) => this.sync_worktrees(cx),
                WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, entries) => {
                    this.entries_changed(*worktree_id, entries, cx)
                }
                WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                    if this.worktrees.remove(worktree_id).is_some() {
                        this.reports_changed(cx);
                    }
                }
                _ => {}
            }),
            cx.observe_global::<SettingsStore>(|this, cx| this.sync_worktrees(cx)),
        ];
        let mut this = Self {
            fs,
            worktree_store,
            worktrees: HashMap::default(),
            buffers: HashMap::default(),
            _subscriptions: subscriptions,
        };
        this.sync_worktrees(cx);
        this
    }

    /// Loads each local worktree's configured reports, asking the worktree to scan their
    /// directories so that changes arrive as entry updates even when they're gitignored.
    fn sync_worktrees(&mut self, cx: &mut Context<Self>) {
        let worktrees = self
            .worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .filter(|worktree| worktree.read(cx).is_local())
            .collect::<Vec<_>>();
        let live_ids = worktrees
            .iter()
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>();
        let mut changed = false;
        self.worktrees.retain(|id, _| {
            let keep = live_ids.contains(id);
            changed |= !keep;
            keep
        });

        for worktree in worktrees {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            let settings = ProjectSettings::get(
                Some(SettingsLocation {
                    worktree_id,
                    path: RelPath::empty(),
                }),
                cx,
            );
            let report_paths = settings
                .coverage
                .reports
                .iter()
                .filter_map(|report| {
                    RelPath::new(Path::new(report), util::paths::PathStyle::local())
                        .ok()
                        .map(|path| path.into_arc())
                })
                .collect::<Vec<_>>();
            if self
                .worktrees
                .get(&worktree_id)
                .is_some_and(|coverage| coverage.report_paths == report_paths)
            {
                continue;
            }
            scan_report_directories(worktree, report_paths.iter());
            changed |= self.worktrees.remove(&worktree_id).is_some();
            self.worktrees.insert(
                worktree_id,
                WorktreeCoverage {
                    root: worktree.abs_path(),
                    report_paths: report_paths.clone(),
                    reports: HashMap::default(),
                    loads: HashMap::default(),
                },
            );
            for report_path in report_paths {
                self.load_report(worktree_id, report_path, cx);
            }
        }
        if changed {
            self.reports_changed(cx);
        }
    }

    fn entries_changed(
        &mut self,
        worktree_id: WorktreeId,
        entries: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        let Some(coverage) = self.worktrees.get(&worktree_id) else {
            return;
        };
        // The worktree drops a scan request for a directory that doesn't exist yet, so ask
        // again once something above the report changes.
        let created_ancestor = coverage.report_paths.iter().filter(|report_path| {
            entries
                .iter()
                .any(|(path, _, _)| *path != **report_path && report_path.starts_with(path))
        });
        if let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        {
            scan_report_directories(worktree.read(cx), created_ancestor);
        }
        let changed = coverage
            .report_paths
            .iter()
            .filter(|report_path| entries.iter().any(|(path, _, _)| path == *report_path))
            .cloned()
            .collect::<Vec<_>>();
        for report_path in changed {
            self.load_report(worktree_id, report_path, cx);
        }
    }

    /// Replacing an in-flight load cancels it, so an older read can't overwrite a newer one.
    fn load_report(
        &mut self,
        worktree_id: WorktreeId,
        report_path: Arc<RelPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(coverage) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let fs = self.fs.clone();
        let root = coverage.root.clone();
        let task = cx.spawn({
            let report_path = report_path.clone();
            async move |this, cx| {
                let report = read_report(fs.as_ref(), &root, &report_path).await;
                this.update(cx, |this, cx| {
                    let Some(coverage) = this.worktrees.get_mut(&worktree_id) else {
                        return;
                    };
                    coverage.loads.remove(&report_path);
                    let changed = match report {
                        Some(report) => {
                            coverage.reports.insert(report_path, report);
                            true
                        }
                        None => coverage.reports.remove(&report_path).is_some(),
                    };
                    if changed {
                        this.reports_changed(cx);
                    }
                })
                .log_err();
            }
        });
        coverage.loads.insert(report_path, task);
    }

    fn reports_changed(&mut self, cx: &mut Context<Self>) {
        self.buffers.clear();
        cx.notify();
    }

    /// Returns the buffer's coverage keyed by current row. Lines edited since the report
    /// was written are left out, and a file saved after the report gets no coverage.
    pub fn buffer_coverage(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &App,
    ) -> Option<Arc<HashMap<u32, LineCoverage>>> {
        let buffer = buffer.read(cx);
        let file = buffer.file()?;
        let (report_modified, file_coverage) = self
            .worktrees
            .get(&file.worktree_id(cx))?
            .reports
            .values()
            .filter_map(|report| Some((report.modified, report.files.get(file.path())?)))
            .reduce(|newest, candidate| {
                if candidate.0.bad_is_greater_than(newest.0) {
                    candidate
                } else {
                    newest
                }
            })?;

        let buffer_id = buffer.remote_id();
        let reuse_base = self
            .buffers
            .get(&buffer_id)
            .is_some_and(|coverage| Arc::ptr_eq(&coverage.file, file_coverage));
        if !reuse_base {
            let file_modified = file.disk_state().mtime()?;
            if file_modified.bad_is_greater_than(report_modified) {
                self.buffers.remove(&buffer_id);
                return None;
            }
            self.buffers.insert(
                buffer_id,
                BufferCoverage {
                    file: file_coverage.clone(),
                    base_version: buffer.saved_version().clone(),
                    mapped: None,
                },
            );
        }

        let coverage = self.buffers.get_mut(&buffer_id)?;
        let version = buffer.version();
        if let Some((mapped_version, mapped)) = &coverage.mapped
            && *mapped_version == version
        {
            return Some(mapped.clone());
        }
        let mapped = Arc::new(map_rows(
            &coverage.file.lines,
            buffer.edits_since::<Point>(&coverage.base_version),
        ));
        coverage.mapped = Some((version, mapped.clone()));
        Some(mapped)
    }
}

fn scan_report_directories<'a>(
    worktree: &Worktree,
    report_paths: impl Iterator<Item = &'a Arc<RelPath>>,
) {
    let Some(worktree) = worktree.as_local() else {
        return;
    };
    for report_path in report_paths {
        if let Some(parent) = report_path.parent()
            && !parent.is_empty()
        {
            drop(worktree.add_path_prefix_to_scan(parent.into_arc()));
        }
    }
}

async fn read_report(fs: &dyn Fs, root: &Path, report_path: &RelPath) -> Option<Report> {
    let report_path = root.join(report_path.as_std_path());
    let report_path = report_path.as_path();
    let modified = fs.metadata(report_path).await.ok()??.mtime;
    let text = fs.load(report_path).await.ok()?;
    let report_dir = report_path.parent().unwrap_or(root);
    let files = parse_report(report_path, &text)
        .with_context(|| format!("reading coverage report {report_path:?}"))
        .log_err()?;
    let mut resolved = HashMap::default();
    for (path, coverage) in files {
        if let Some(path) = resolve_report_path(fs, &path, root, report_dir).await {
            resolved.insert(path, Arc::new(coverage));
        }
    }
    Some(Report {
        modified,
        files: resolved,
    })
}

/// Moves each covered row through `edits`, dropping rows whose text an edit touched.
/// Inserting or deleting whole lines only shifts the rows below.
fn map_rows(
    lines: &BTreeMap<u32, LineCoverage>,
    edits: impl Iterator<Item = Edit<Point>>,
) -> HashMap<u32, LineCoverage> {
    let mut mapped = HashMap::default();
    let mut edits = edits.peekable();
    let mut delta: i64 = 0;
    for (&row, &status) in lines {
        let mut touched = false;
        while let Some(edit) = edits.peek() {
            let ends_at_line_start = edit.old.end.column == 0 && edit.new.end.column == 0;
            let last_touched_row = if ends_at_line_start {
                edit.old.end.row as i64 - 1
            } else {
                edit.old.end.row as i64
            };
            if (row as i64) < edit.old.start.row as i64 {
                break;
            }
            if row as i64 <= last_touched_row {
                touched = true;
                break;
            }
            delta = edit.new.end.row as i64 - edit.old.end.row as i64;
            edits.next();
        }
        if !touched && let Ok(row) = u32::try_from(row as i64 + delta) {
            mapped.insert(row, status);
        }
    }
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::time::Duration;
    use util::path;

    fn rows(coverage: &FileCoverage) -> Vec<(u32, LineCoverage)> {
        coverage
            .lines
            .iter()
            .map(|(row, status)| (*row, *status))
            .collect()
    }

    #[test]
    fn test_parse_lcov() {
        let files = parse_lcov(
            "TN:\n\
             SF:src/lib.rs\n\
             DA:1,4\n\
             DA:2,0\n\
             DA:3,2\n\
             BRDA:3,0,0,2\n\
             BRDA:3,0,1,-\n\
             DA:4,1\n\
             BRDA:4,0,0,1\n\
             BRDA:4,0,1,3\n\
             end_of_record\n\
             SF:/abs/other.rs\n\
             DA:10,0\n\
             end_of_record\n",
        );
        use LineCoverage::*;
        assert_eq!(
            rows(&files[Path::new("src/lib.rs")]),
            [(0, Covered), (1, Uncovered), (2, Partial), (3, Covered)]
        );
        assert_eq!(rows(&files[Path::new("/abs/other.rs")]), [(9, Uncovered)]);
    }

    #[test]
    fn test_parse_cobertura() {
        let files = parse_cobertura(
            r#"<?xml version="1.0" ?>
            <coverage>
              <sources><source>/project</source></sources>
              <packages><package name="app"><classes>
                <class name="a" filename="src/a.py">
                  <lines>
                    <line number="1" hits="3"/>
                    <line number="2" hits="0"/>
                    <line number="3" hits="1" branch="true" condition-coverage="50% (1/2)"/>
                  </lines>
                </class>
                <class name="b" filename="src/a.py">
                  <lines><line number="2" hits="1"/></lines>
                </class>
              </classes></package></packages>
            </coverage>"#,
        )
        .unwrap();
        use LineCoverage::*;
        assert_eq!(
            rows(&files[Path::new("/project/src/a.py")]),
            [(0, Covered), (1, Covered), (2, Partial)],
            "a line any class covers counts as covered"
        );
    }

    #[gpui::test]
    async fn test_resolve_report_path(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "src": { "a.rs": "" },
                "coverage": { "src": { "b.rs": "" } },
            }),
        )
        .await;
        let root = Path::new(path!("/project"));
        let report_dir = Path::new(path!("/project/coverage"));
        let resolve = async |path: &str| {
            resolve_report_path(fs.as_ref(), Path::new(path), root, report_dir)
                .await
                .map(|path| path.as_unix_str().to_string())
        };
        assert_eq!(
            resolve(path!("/project/src/a.rs")).await.as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(resolve("../src/a.rs").await.as_deref(), Some("src/a.rs"));
        assert_eq!(
            resolve("src/a.rs").await.as_deref(),
            Some("src/a.rs"),
            "relative to the worktree root"
        );
        assert_eq!(
            resolve("src/b.rs").await.as_deref(),
            Some("coverage/src/b.rs"),
            "relative to the report's directory"
        );
        assert_eq!(resolve("src/missing.rs").await, None);
        assert_eq!(resolve(path!("/elsewhere/a.rs")).await, None);
    }

    #[test]
    fn test_map_rows() {
        use LineCoverage::*;
        let lines: BTreeMap<u32, LineCoverage> =
            [(0, Covered), (1, Uncovered), (2, Covered), (5, Partial)]
                .into_iter()
                .collect();
        let edit = |old: (Point, Point), new: (Point, Point)| Edit {
            old: old.0..old.1,
            new: new.0..new.1,
        };
        let map = |edits: Vec<Edit<Point>>| {
            let mut mapped = map_rows(&lines, edits.into_iter())
                .into_iter()
                .collect::<Vec<_>>();
            mapped.sort_by_key(|(row, _)| *row);
            mapped
        };

        assert_eq!(
            map(vec![edit(
                (Point::new(1, 0), Point::new(1, 0)),
                (Point::new(1, 0), Point::new(3, 0)),
            )]),
            [(0, Covered), (3, Uncovered), (4, Covered), (7, Partial)],
            "inserting whole lines shifts the rows below"
        );
        assert_eq!(
            map(vec![edit(
                (Point::new(1, 0), Point::new(2, 0)),
                (Point::new(1, 0), Point::new(1, 0)),
            )]),
            [(0, Covered), (1, Covered), (4, Partial)],
            "deleting a line drops it and shifts the rows below"
        );
        assert_eq!(
            map(vec![edit(
                (Point::new(2, 3), Point::new(2, 5)),
                (Point::new(2, 3), Point::new(2, 9)),
            )]),
            [(0, Covered), (1, Uncovered), (5, Partial)],
            "editing within a line drops only that line"
        );
        assert_eq!(
            map(vec![
                edit(
                    (Point::new(0, 1), Point::new(0, 1)),
                    (Point::new(0, 1), Point::new(1, 0)),
                ),
                edit(
                    (Point::new(5, 0), Point::new(5, 2)),
                    (Point::new(6, 0), Point::new(6, 0)),
                ),
            ]),
            [(2, Uncovered), (3, Covered)],
            "splitting a line drops it, and later edits see the earlier shift"
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn test_buffer_coverage_follows_edits_and_reports(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({ "src": { "lib.rs": "one\ntwo\nthree\n" } }),
        )
        .await;
        // The report is written after the source it describes.
        cx.executor().advance_clock(Duration::from_secs(1));
        fs.insert_file(
            path!("/project/lcov.info"),
            "SF:src/lib.rs\nDA:1,1\nDA:2,0\nDA:3,1\nend_of_record\n".into(),
        )
        .await;

        let project = crate::Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let coverage_store = project.read_with(cx, |project, _| project.coverage_store());
        let coverage_store = coverage_store.expect("local projects have a coverage store");
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/src/lib.rs"), cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let coverage = |cx: &mut TestAppContext| {
            coverage_store.update(cx, |store, cx| {
                let mut rows = store
                    .buffer_coverage(&buffer, cx)
                    .map(|coverage| {
                        coverage
                            .iter()
                            .map(|(row, status)| (*row, *status))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                rows.sort_by_key(|(row, _)| *row);
                rows
            })
        };
        use LineCoverage::*;
        assert_eq!(coverage(cx), [(0, Covered), (1, Uncovered), (2, Covered)]);

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(0, 0)..Point::new(0, 0), "zero\n")], None, cx);
            buffer.edit([(Point::new(2, 0)..Point::new(2, 3), "TWO")], None, cx);
        });
        assert_eq!(
            coverage(cx),
            [(1, Covered), (3, Covered)],
            "the edited line loses its coverage and the rest shift down"
        );

        cx.executor().advance_clock(Duration::from_secs(1));
        fs.save(
            path!("/project/lcov.info").as_ref(),
            &"SF:src/lib.rs\nDA:1,0\nend_of_record\n".into(),
            Default::default(),
        )
        .await
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            coverage(cx),
            [(1, Uncovered)],
            "a new report is mapped through the buffer's unsaved edits"
        );

        fs.remove_file(path!("/project/lcov.info").as_ref(), Default::default())
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(coverage(cx), []);
    }

    #[gpui::test]
    async fn test_report_written_later_into_ignored_directory(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                ".gitignore": "coverage/\n",
                "src": { "lib.js": "one\n" },
            }),
        )
        .await;
        let project = crate::Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let coverage_store = project
            .read_with(cx, |project, _| project.coverage_store())
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/src/lib.js"), cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(
            coverage_store
                .update(cx, |store, cx| store.buffer_coverage(&buffer, cx))
                .is_none()
        );

        cx.executor().advance_clock(Duration::from_secs(1));
        fs.insert_tree(
            path!("/project/coverage"),
            json!({ "lcov.info": "SF:src/lib.js\nDA:1,0\nend_of_record\n" }),
        )
        .await;
        cx.run_until_parked();
        let coverage = coverage_store
            .update(cx, |store, cx| store.buffer_coverage(&buffer, cx))
            .expect("the report should load once written");
        assert_eq!(coverage.get(&0), Some(&LineCoverage::Uncovered));
    }

    #[gpui::test]
    async fn test_buffer_saved_after_report_has_no_coverage(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "lcov.info": "SF:src/lib.rs\nDA:1,1\nend_of_record\n",
                "src": {},
            }),
        )
        .await;
        cx.executor().advance_clock(Duration::from_secs(1));
        fs.insert_file(path!("/project/src/lib.rs"), "one\n".into())
            .await;

        let project = crate::Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let coverage_store = project
            .read_with(cx, |project, _| project.coverage_store())
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/src/lib.rs"), cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        assert!(
            coverage_store
                .update(cx, |store, cx| store.buffer_coverage(&buffer, cx))
                .is_none()
        );
    }
}
