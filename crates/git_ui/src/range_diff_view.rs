use anyhow::Context as _;
use collections::HashMap;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer,
    SplittableEditor,
};
use git::status::FileStatus;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use language::Capability;
use project::{
    Project, ProjectPath,
    git_store::{Repository, RepositoryEvent},
};
use settings::Settings;
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::{DiffStat, Divider, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::{ItemEvent, TabTooltipContent},
    notifications::NotifyTaskExt as _,
    searchable::SearchableItemHandle,
};

use crate::branch_diff::BranchDiff;
use crate::branch_picker;
use crate::commit_view::{LoadedDiffFiles, load_diff_files};

/// A read-only diff of the changes on `target` since it diverged from `base`,
/// without checking `target` out.
pub struct RangeDiffView {
    base: SharedString,
    target: SharedString,
    editor: Entity<SplittableEditor>,
    multibuffer: Entity<MultiBuffer>,
    repository: Entity<Repository>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    loaded_tips: BranchTips,
    load_state: LoadState,
    _load_diff_task: Task<()>,
    _repository_subscription: Subscription,
}

#[derive(Clone, Default, PartialEq)]
struct BranchTips {
    base: Option<SharedString>,
    target: Option<SharedString>,
}

#[derive(Clone, PartialEq)]
enum LoadState {
    Loading,
    Loaded,
    Failed(SharedString),
}

struct RangeDiffAddon {
    file_statuses: HashMap<language::BufferId, FileStatus>,
}

impl Addon for RangeDiffAddon {
    fn to_any(&self) -> &dyn Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        _cx: &App,
    ) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }
}

impl RangeDiffView {
    /// Opens the diff in the active pane, replacing a view of the same range.
    pub fn open(
        base: SharedString,
        target: SharedString,
        repository: Entity<Repository>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let workspace_entity = cx.entity();
        let workspace_handle = cx.weak_entity();
        let view = cx.new(|cx| {
            Self::new(
                base.clone(),
                target.clone(),
                repository,
                project,
                workspace_entity,
                workspace_handle,
                window,
                cx,
            )
        });

        workspace.active_pane().update(cx, |pane, cx| {
            let existing = pane.items().enumerate().find_map(|(ix, item)| {
                let existing = item.downcast::<Self>()?;
                let existing_ref = existing.read(cx);
                (existing_ref.base == base && existing_ref.target == target)
                    .then(|| (ix, existing.item_id()))
            });
            let index = existing.map(|(ix, item_id)| {
                pane.remove_item(item_id, false, false, window, cx);
                ix
            });
            pane.add_item(Box::new(view), true, true, index, window, cx);
        });
    }

    /// Opens the diff of `target` against the repository's default branch.
    pub(crate) fn open_against_default_branch(
        target: SharedString,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let default_branch = repository.update(cx, |repository, _| repository.default_branch(true));
        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let base = default_branch
                        .await??
                        .context("the repository has no default branch")?;
                    workspace.update_in(cx, |workspace, window, cx| {
                        Self::open(base, target, repository, workspace, window, cx);
                    })
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    fn new(
        base: SharedString,
        target: SharedString,
        repository: Entity<Repository>,
        project: Entity<Project>,
        workspace_entity: Entity<Workspace>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (multibuffer, editor) = Self::build_editor(&project, workspace_entity, window, cx);
        let repository_subscription =
            cx.subscribe_in(&repository, window, |this, _, event, window, cx| {
                if matches!(event, RepositoryEvent::BranchListChanged)
                    && this.branch_tips(cx) != this.loaded_tips
                {
                    this.reload(window, cx);
                }
            });
        let mut this = Self {
            base,
            target,
            editor,
            multibuffer,
            repository,
            project,
            workspace,
            loaded_tips: Default::default(),
            load_state: LoadState::Loading,
            _load_diff_task: Task::ready(()),
            _repository_subscription: repository_subscription,
        };
        this.load(window, cx);
        this
    }

    fn build_editor(
        project: &Entity<Project>,
        workspace_entity: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<MultiBuffer>, Entity<SplittableEditor>) {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace_entity,
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
            });
            editor
        });
        (multibuffer, editor)
    }

    /// The commits `base` and `target` point at, when they name branches.
    fn branch_tips(&self, cx: &App) -> BranchTips {
        let branch_list = &self.repository.read(cx).snapshot().branch_list;
        let tip = |name: &SharedString| {
            branch_list
                .iter()
                .find(|branch| branch.name() == name.as_ref())
                .and_then(|branch| branch.most_recent_commit.as_ref())
                .map(|commit| commit.sha.clone())
        };
        BranchTips {
            base: tip(&self.base),
            target: tip(&self.target),
        }
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return;
        };
        let was_focused = self.editor.focus_handle(cx).contains_focused(window, cx);
        let (multibuffer, editor) = Self::build_editor(&self.project, workspace_entity, window, cx);
        self.multibuffer = multibuffer;
        self.editor = editor;
        if was_focused {
            self.editor.focus_handle(cx).focus(window, cx);
        }
        self.load(window, cx);
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loaded_tips = self.branch_tips(cx);
        self.load_state = LoadState::Loading;
        let range_diff = self.repository.update(cx, |repository, cx| {
            repository.load_range_diff(self.base.to_string(), self.target.to_string(), cx)
        });
        let language_registry = self.project.read(cx).languages().clone();
        let editor = self.editor.downgrade();
        let repository = self.repository.clone();
        let project = self.project.clone();
        let target = self.target.clone();
        self._load_diff_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let range_diff = range_diff.await?;
                load_diff_files(
                    range_diff.files,
                    &target,
                    editor.clone(),
                    repository,
                    project,
                    language_registry,
                    cx,
                )
                .await
            }
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(LoadedDiffFiles {
                        file_statuses,
                        binary_buffer_ids,
                    }) => {
                        this.editor.update(cx, |editor, cx| {
                            editor.rhs_editor().update(cx, |editor, cx| {
                                editor.register_addon(RangeDiffAddon { file_statuses });
                                if !binary_buffer_ids.is_empty() {
                                    editor.fold_buffers(binary_buffer_ids, cx);
                                }
                            });
                        });
                        this.load_state = LoadState::Loaded;
                    }
                    Err(error) => {
                        this.load_state = LoadState::Failed(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    fn render_placeholder(&self, cx: &App) -> Option<AnyElement> {
        let message: SharedString = match &self.load_state {
            LoadState::Loading => return None,
            LoadState::Failed(error) => error.clone(),
            LoadState::Loaded => {
                if !self.multibuffer.read(cx).is_empty() {
                    return None;
                }
                format!(
                    "No changes on {} since it diverged from {}",
                    self.target, self.base
                )
                .into()
            }
        };
        Some(
            h_flex()
                .size_full()
                .justify_center()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element(),
        )
    }
}

impl EventEmitter<EditorEvent> for RangeDiffView {}

impl Focusable for RangeDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for RangeDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("{}...{}", self.base, self.target).into()
    }

    fn tab_tooltip_content(&self, _: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Text(
            format!(
                "Changes on {} since it diverged from {}",
                self.target, self.base
            )
            .into(),
        ))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Range Diff Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for RangeDiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .map(|this| match self.render_placeholder(cx) {
                Some(placeholder) => this.child(placeholder),
                None => this.child(div().flex_grow(1.).child(self.editor.clone())),
            })
    }
}

/// The target label for a diff against the working tree rather than a ref.
pub(crate) const WORKING_TREE_TARGET: &str = "Working Tree";

pub struct RangeDiffToolbar {
    range_diff: Option<WeakEntity<RangeDiffView>>,
}

impl RangeDiffToolbar {
    pub fn new() -> Self {
        Self { range_diff: None }
    }
}

impl EventEmitter<ToolbarItemEvent> for RangeDiffToolbar {}

impl ToolbarItemView for RangeDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.range_diff = active_pane_item
            .and_then(|item| item.act_as::<RangeDiffView>(cx))
            .map(|entity| entity.downgrade());
        if self.range_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

/// Which side of the range a picker changes.
#[derive(Clone, Copy)]
pub(crate) enum RangeSide {
    Base,
    Target,
}

pub(crate) fn range_picker(
    side: RangeSide,
    base: SharedString,
    target: SharedString,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
) -> impl IntoElement {
    let (id, label, tooltip, selected) = match side {
        RangeSide::Base => (
            "range-diff-base-picker",
            format!("Base: {base}"),
            "Select Base",
            base.clone(),
        ),
        RangeSide::Target => (
            "range-diff-target-picker",
            format!("Target: {target}"),
            "Select Target",
            target.clone(),
        ),
    };
    PopoverMenu::new(id)
        .menu(move |window, cx| {
            let base = base.clone();
            let target = target.clone();
            let repository_for_select = repository.clone();
            let workspace_for_select = workspace.clone();
            let on_select = Arc::new(
                move |branch: git::repository::Branch, window: &mut Window, cx: &mut App| {
                    let picked: SharedString = branch.name().to_owned().into();
                    let (base, target) = match side {
                        RangeSide::Base => (picked, target.clone()),
                        RangeSide::Target => (base.clone(), picked),
                    };
                    let repository = repository_for_select.clone();
                    workspace_for_select
                        .update(cx, |workspace, cx| {
                            RangeDiffView::open(base, target, repository, workspace, window, cx);
                        })
                        .log_err();
                },
            );
            Some(branch_picker::select_popover(
                workspace.clone(),
                Some(repository.clone()),
                Some(selected.clone()),
                on_select,
                window,
                cx,
            ))
        })
        .trigger_with_tooltip(
            Button::new(id, label).end_icon(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            ),
            Tooltip::text(tooltip),
        )
}

impl Render for RangeDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(range_diff) = self.range_diff.as_ref().and_then(|view| view.upgrade()) else {
            return div();
        };
        let range_diff = range_diff.read(cx);
        let (additions, deletions) = range_diff.changed_lines(cx);
        let base = range_diff.base.clone();
        let target = range_diff.target.clone();
        let repository = range_diff.repository.clone();
        let project = range_diff.project.clone();
        let workspace = range_diff.workspace.clone();

        h_flex()
            .my_neg_1()
            .py_1()
            .gap_1p5()
            .flex_wrap()
            .when(additions > 0 || deletions > 0, |this| {
                this.child(DiffStat::new(
                    "range-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
                .child(Divider::vertical())
            })
            .child(range_picker(
                RangeSide::Base,
                base.clone(),
                target.clone(),
                repository.clone(),
                workspace.clone(),
            ))
            .child(range_picker(
                RangeSide::Target,
                base.clone(),
                target,
                repository.clone(),
                workspace.clone(),
            ))
            .child(
                IconButton::new("range-diff-working-tree", IconName::FileDiff)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Compare the Working Tree with Base"))
                    .on_click(move |_, window, cx| {
                        let base = base.clone();
                        let repository = repository.clone();
                        let project = project.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                BranchDiff::deploy_branch_diff_with_base_ref(
                                    workspace, project, repository, base, None, window, cx,
                                );
                            })
                            .log_err();
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use git::repository::{Branch, CommitDiff, CommitFile, CommitSummary, repo_path};
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            crate::init(cx);
        });
    }

    fn set_range_diff(fs: &FakeFs, new_content: &str) {
        let diff = CommitDiff {
            files: vec![CommitFile {
                path: repo_path("feature.txt"),
                old_content: None,
                new_content: Some(new_content.as_bytes().to_vec()),
                is_binary: false,
            }],
            is_shallow_boundary: false,
        };
        fs.with_git_state(Path::new("/project/.git"), false, |state| {
            state
                .range_diffs
                .insert(("main".into(), "feature".into()), diff);
        })
        .expect("fake repository should exist");
    }

    fn feature_branch(sha: &str) -> Branch {
        Branch {
            is_head: false,
            ref_name: "refs/heads/feature".into(),
            upstream: None,
            most_recent_commit: Some(CommitSummary {
                sha: sha.to_owned().into(),
                subject: "feature".into(),
                commit_timestamp: 0,
                author_name: "Author".into(),
                has_parent: true,
            }),
        }
    }

    fn range_diff_text(view: &Entity<RangeDiffView>, cx: &mut VisualTestContext) -> String {
        let editor = view.read_with(cx, |view, cx| view.editor.read(cx).rhs_editor().clone());
        editor.update(cx, |editor, cx| editor.text(cx))
    }

    #[gpui::test]
    async fn test_range_diff_reloads_when_target_moves(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/project"), json!({ ".git": {}, "file.txt": "" }))
            .await;
        set_range_diff(&fs, "first\n");

        let project = project::Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(vec![feature_branch("aaa")], cx);
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            RangeDiffView::open(
                "main".into(),
                "feature".into(),
                repository.clone(),
                workspace,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let view = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<RangeDiffView>(cx)
                .expect("range diff should be active")
        });
        assert_eq!(
            view.read_with(cx, |view, cx| view.tab_content_text(0, cx)),
            "main...feature"
        );
        assert_eq!(range_diff_text(&view, cx), "first\n");

        set_range_diff(&fs, "second\n");
        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(vec![feature_branch("aaa")], cx);
        });
        cx.run_until_parked();
        assert_eq!(
            range_diff_text(&view, cx),
            "first\n",
            "an unchanged tip should not reload"
        );

        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(vec![feature_branch("bbb")], cx);
        });
        cx.run_until_parked();
        assert_eq!(range_diff_text(&view, cx), "second\n");
    }
}
