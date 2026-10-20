use crate::{commit_view::CommitView, create_tag_at_commit, range_diff_view::RangeDiffView};
use git::Oid;
use gpui::{
    Action, ClipboardItem, Entity, FocusHandle, PromptLevel, SharedString, WeakEntity, Window,
    actions,
};
use project::{GIT_COMMAND_TASK_TAG, git_store::Repository};

use task::{TaskContext, TaskVariables, VariableName};
use ui::{Color, ContextMenu, ContextMenuEntry, IconName, IconPosition, prelude::*};
use util::ResultExt as _;
use workspace::{Workspace, notifications::DetachAndPromptErr as _};

actions!(
    git_graph,
    [
        /// Copies the SHA of the selected commit to the clipboard.
        CopyCommitSha,
        /// Copies a tag from the selected commit to the clipboard.
        CopyCommitTag,
        /// Opens the commit view for the selected commit.
        OpenCommitView,
    ]
);

const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
pub(crate) const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitGraph,
    GitPanel,
}

pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    ref_name: Option<SharedString>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let sha = commit.sha;
    let sha_short = sha.display_short();
    let git_tasks = git_context_menu_tasks(
        git_task_context(&repository, sha, ref_name.as_deref(), cx),
        &workspace,
        cx,
    );
    let header = match &ref_name {
        Some(ref_name) => format!("Ref {ref_name}"),
        None => format!("Commit {sha_short}"),
    };

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        context_menu
            .context(focus_handle)
            .header(header)
            .entry("View Diff", Some(OpenCommitView.boxed_clone()), {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.clone() else {
                        return;
                    };
                    CommitView::open(
                        sha.to_string(),
                        repository,
                        workspace.clone(),
                        None,
                        None,
                        window,
                        cx,
                    );
                }
            })
            .entry("Compare with Default Branch", None, {
                let repository = repository.clone();
                let workspace = workspace.clone();
                let target: SharedString =
                    ref_name.clone().unwrap_or_else(|| sha.to_string().into());
                move |window, cx| {
                    let Some(repository) = repository.as_ref().and_then(WeakEntity::upgrade) else {
                        return;
                    };
                    RangeDiffView::open_against_default_branch(
                        target.clone(),
                        repository,
                        workspace.clone(),
                        window,
                        cx,
                    );
                }
            })
            .entry(
                "Copy SHA",
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry("Copy Ref Name", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = "Copy Tag";

                    match tag_names.as_slice() {
                        [] => menu.item(
                            ContextMenuEntry::new(copy_tag_label)
                                .action(CopyCommitTag.boxed_clone())
                                .disabled(true),
                        ),
                        [tag_name] => {
                            let tag_name = tag_name.clone();
                            let label = format!("{copy_tag_label}: {tag_name}");
                            menu.entry(
                                label,
                                Some(CopyCommitTag.boxed_clone()),
                                move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name.to_string(),
                                    ));
                                },
                            )
                        }
                        _ => menu.submenu(copy_tag_label, move |menu, _window, _cx| {
                            let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());

                            for tag_name in tag_names.clone() {
                                let tag_name_to_copy = tag_name.clone();
                                menu = menu.entry(tag_name, None, move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name_to_copy.to_string(),
                                    ));
                                });
                            }
                            menu
                        }),
                    }
                })
            })
            .entry("Create Tag…", None, {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.as_ref().and_then(WeakEntity::upgrade) else {
                        return;
                    };
                    workspace
                        .update(cx, |workspace, cx| {
                            create_tag_at_commit(sha, false, repository, workspace, window, cx);
                        })
                        .log_err();
                }
            })
            .entry("Checkout Detached", None, {
                let repository = repository.clone();
                move |window, cx| {
                    let Some(repository) = repository.as_ref().and_then(WeakEntity::upgrade) else {
                        return;
                    };
                    check_out_detached(sha, repository, window, cx);
                }
            })
            .when(source == CommitContextMenuSource::GitPanel, |menu| {
                menu.entry("Show in Git Graph", None, move |window, cx| {
                    window.dispatch_action(
                        Box::new(crate::git_graph::OpenAtCommit {
                            sha: sha.to_string(),
                        }),
                        cx,
                    );
                })
            })
            .map(|mut menu| {
                menu = menu.separator().header("Custom Commands");

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new("Learn More")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .icon_position(IconPosition::End)
                            .handler(|_window, cx| {
                                let docs_url =
                                    release_channel::docs_url(CUSTOM_GIT_COMMANDS_DOCS_SLUG, cx);
                                cx.open_url(&docs_url);
                            }),
                    );
                }

                for (task_source_kind, resolved_task) in git_tasks {
                    let label = resolved_task.display_label().to_string();
                    let workspace = workspace.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.schedule_resolved_task(
                                    task_source_kind.clone(),
                                    resolved_task.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    });
                }

                menu
            })
    })
}

pub(crate) fn git_task_context(
    repository: &Option<WeakEntity<Repository>>,
    commit_sha: git::Oid,
    ref_name: Option<&str>,
    cx: &App,
) -> Option<TaskContext> {
    let repository_path = repository
        .as_ref()?
        .upgrade()?
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string);
    let mut task_variables = TaskVariables::from_iter([
        (VariableName::GitSha, commit_sha.to_string()),
        (VariableName::GitShaShort, commit_sha.display_short()),
        (
            VariableName::GitRepositoryPath,
            repository_path.to_string_lossy().into_owned(),
        ),
    ]);

    if let Some(repository_name) = repository_name {
        task_variables.insert(VariableName::GitRepositoryName, repository_name);
    }
    if let Some(ref_name) = ref_name {
        task_variables.insert(VariableName::GitRef, ref_name.to_string());
    }

    Some(TaskContext {
        cwd: Some(repository_path),
        task_variables,
        ..TaskContext::default()
    })
}

pub(crate) fn git_context_menu_tasks(
    task_context: Option<TaskContext>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
    let Some(task_context) = task_context else {
        return Vec::new();
    };
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let project = workspace.read(cx).project().clone();
    let task_inventory = project.read_with(cx, |project, cx| {
        project.task_store().read(cx).task_inventory().cloned()
    });
    let Some(task_inventory) = task_inventory else {
        return Vec::new();
    };

    task_inventory
        .read(cx)
        .resolve_global_tasks_with_tag(GIT_COMMAND_TASK_TAG, &task_context)
}

/// The local branch that checking out `ref_name` should switch to, if it can
/// be done without repointing another branch's upstream.
pub(crate) fn branch_to_check_out(
    ref_name: &str,
    branch_list: &[git::repository::Branch],
) -> Option<SharedString> {
    if branch_list
        .iter()
        .any(|branch| !branch.is_remote() && branch.name() == ref_name)
    {
        return Some(ref_name.to_owned().into());
    }
    let remote_ref_name = format!("refs/remotes/{ref_name}");
    if !branch_list
        .iter()
        .any(|branch| branch.ref_name.as_ref() == remote_ref_name)
    {
        return None;
    }
    if let Some(tracking) = branch_list.iter().find(|branch| {
        !branch.is_remote()
            && branch
                .upstream
                .as_ref()
                .is_some_and(|upstream| upstream.ref_name.as_ref() == remote_ref_name)
    }) {
        return Some(tracking.name().to_owned().into());
    }
    let (_, short_name) = ref_name.split_once('/')?;
    let short_name_is_free = !branch_list
        .iter()
        .any(|branch| !branch.is_remote() && branch.name() == short_name);
    short_name_is_free.then(|| ref_name.to_owned().into())
}

pub(crate) fn check_out_branch(
    branch_name: SharedString,
    repository: Entity<Repository>,
    window: &mut Window,
    cx: &mut App,
) {
    let change_branch = repository.update(cx, |repository, _| {
        repository.change_branch(branch_name.to_string())
    });
    window
        .spawn(cx, async move |_| change_branch.await?)
        .detach_and_prompt_err("Failed to check out branch", window, cx, |_, _, _| None);
}

pub(crate) fn check_out_detached(
    sha: Oid,
    repository: Entity<Repository>,
    window: &mut Window,
    cx: &mut App,
) {
    let has_changes = repository.read(cx).cached_status().next().is_some();
    let answer = has_changes.then(|| {
        window.prompt(
            PromptLevel::Warning,
            "You have uncommitted changes",
            Some("Git carries them over to the checkout, and stops if any would conflict."),
            &["Checkout Detached", "Cancel"],
            cx,
        )
    });
    window
        .spawn(cx, async move |cx| {
            if let Some(answer) = answer
                && answer.await != Ok(0)
            {
                return Ok(());
            }
            repository
                .update(cx, |repository, _| {
                    repository.checkout_detached(sha.to_string())
                })
                .await?
        })
        .detach_and_prompt_err("Failed to check out commit", window, cx, |_, _, _| None);
}
