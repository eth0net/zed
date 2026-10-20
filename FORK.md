# eth0net/zed

A fork of [zed-industries/zed](https://github.com/zed-industries/zed) for
workspace and project persistence fixes. Each fix lives on its own branch off
upstream `main` so it can go upstream as one PR. `fork` is upstream plus all of
them, and is what gets built.

## Branches

| Branch                                 | Upstream PR | Status           |
| -------------------------------------- | ----------- | ---------------- |
| `worktree-order-everywhere`            | —           | not yet proposed |
| `restore-with-missing-folders`         | —           | not yet proposed |
| `fix-project-path-merging`             | #56414      | open             |
| `feature-worktree-drag-reorder`        | #60656      | open             |
| `project-panel-persist-collapse-state` | #56521      | closed, unmerged |
| `project-panel-auto-reveal-ignored`    | —           | not yet proposed |
| `sidebar-drag-reorder-projects`        | —           | not yet proposed |
| `git-diff-any-target`                  | —           | not yet proposed |
| `git-graph-refs`                       | —           | not yet proposed |
| `git-remotes`                          | —           | not yet proposed |
| `release-channel-fork`                 | —           | fork-only        |

`release-channel-fork` adds the Fork release channel ("Zed Fork",
`dev.zed.Zed-Fork`, its own database, no auto-update) and the build scripts.
It is never proposed upstream.

## Building

On `fork`, run `script/install-fork`, then restart Zed Fork. It builds a
stripped release bundle on the Fork channel, installs it over
`/Applications/Zed Fork.app` and deletes the release build afterwards. A build
from scratch takes about 20 minutes.

## Syncing with upstream

Merge `upstream/main` into each branch, then each branch into `fork`. Run the
tests for the crates the branches touch (`workspace`, `sidebar`,
`project_panel`, `project`, `recent_projects`, `settings_ui`, and
`cargo test -p zed --bin zed`) before pushing.

## Next

- Sidebar data loss:
  - The session id is written before restore finishes.
  - Window project lists are keyed by a window id macOS reuses.
  - Turning AI off clears every project group.
- Reopen Closed Window.
- Sidebar drag: accept drops on a group's threads, and scroll while dragging.
- Upstream `bundle-mac -i` tries to build a DMG from an app it has already
  installed; fixed on `release-channel-fork`, worth its own upstream PR.
- Later: clean up stale workspace rows and window state, a saved projects
  list in its own migration domain, startup and restore performance.
