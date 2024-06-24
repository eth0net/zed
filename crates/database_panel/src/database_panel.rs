mod database_panel_settings;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use database_panel_settings::{DatabasePanelDockPosition, DatabasePanelSettings};
use db::kvp::KEY_VALUE_STORE;
// use editor::Editor;
use gpui::{
    actions, AppContext, AsyncWindowContext, EventEmitter, FocusHandle, FocusableView,
    InteractiveElement, ParentElement, Pixels, Render, Styled, StyledText, Task, View, ViewContext,
    WeakView, WindowContext,
};
use project::Fs;
use settings::{Settings, SettingsStore};
use ui::{prelude::*, v_flex};
use util::{ResultExt, TryFutureExt};
use workspace::{
    dock::{DockPosition, Panel, PanelEvent},
    Workspace,
};

const DATABASE_PANEL_KEY: &str = "DatabasePanel";

pub struct DatabasePanel {
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,
    pending_serialization: Task<Option<()>>,
}

actions!(database_panel, [ToggleFocus]);

pub fn init_settings(cx: &mut AppContext) {
    DatabasePanelSettings::register(cx);
}

pub fn init(cx: &mut AppContext) {
    init_settings(cx);

    cx.observe_new_views(|workspace: &mut Workspace, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(cx);
        });
    })
    .detach();
}

#[derive(Debug)]
pub enum Event {
    Focus,
}

#[derive(Serialize, Deserialize)]
struct SerializedDatabasePanel {
    width: Option<Pixels>,
}

impl DatabasePanel {
    fn new(workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) -> View<Self> {
        // todo: load database list from the workspace
        // let databases = workspace.databases.clone();
        let database_panel = cx.new_view(|cx: &mut ViewContext<Self>| {
            let focus_handle = cx.focus_handle();
            cx.on_focus(&focus_handle, Self::focus_in).detach();

            // todo: subscribe to workspace database list events, like project in project_panel

            let mut database_panel_settings = *DatabasePanelSettings::get_global(cx);
            cx.observe_global::<SettingsStore>(move |_, cx| {
                let new_settings = *DatabasePanelSettings::get_global(cx);
                if database_panel_settings != new_settings {
                    database_panel_settings = new_settings;
                    cx.notify();
                }
            })
            .detach();

            // let database_name_editor = cx.new_view(|cx| Editor::single_line(cx));

            // cx.subscribe(&database_name_editor, |this, _, event, cx| match event {
            //     _ => {}
            // });

            let this = Self {
                fs: workspace.app_state().fs.clone(),
                focus_handle,
                width: None,
                pending_serialization: Task::ready(None),
            };
            // this.update_visible_entries(None, cx);

            this
        });

        // todo: handle the events needed here as the panel is implemented further
        // cx.subscribe(&database_panel, {
        //     let database_panel = database_panel.downgrade();
        //     move |workspace, _, event, cx| match event {
        //         _ => {}
        //     }
        // })
        // .detach();

        database_panel
    }

    pub async fn load(
        workspace: WeakView<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<View<Self>> {
        let serialized_panel = cx
            .background_executor()
            .spawn(async move { KEY_VALUE_STORE.read_kvp(DATABASE_PANEL_KEY) })
            .await
            .map_err(|e| anyhow!("Failed to load project panel: {}", e))
            .log_err()
            .flatten()
            .map(|panel| serde_json::from_str::<SerializedDatabasePanel>(&panel))
            .transpose()
            .log_err()
            .flatten();

        workspace.update(&mut cx, |workspace, cx| {
            let panel = DatabasePanel::new(workspace, cx);
            if let Some(serialized_panel) = serialized_panel {
                panel.update(cx, |panel, cx| {
                    panel.width = serialized_panel.width;
                    cx.notify();
                });
            }
            panel
        })
    }

    fn serialize(&mut self, cx: &mut ViewContext<Self>) {
        let width = self.width;
        self.pending_serialization = cx.background_executor().spawn(
            async move {
                KEY_VALUE_STORE
                    .write_kvp(
                        DATABASE_PANEL_KEY.into(),
                        serde_json::to_string(&SerializedDatabasePanel { width })?,
                    )
                    .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    fn focus_in(&mut self, cx: &mut ViewContext<Self>) {
        if !self.focus_handle.contains_focused(cx) {
            cx.emit(Event::Focus);
        }
    }
}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "Database Panel"
    }

    fn position(&self, cx: &WindowContext) -> DockPosition {
        match DatabasePanelSettings::get_global(cx).dock {
            DatabasePanelDockPosition::Left => DockPosition::Left,
            DatabasePanelDockPosition::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, cx: &mut ViewContext<Self>) {
        settings::update_settings_file::<DatabasePanelSettings>(
            self.fs.clone(),
            cx,
            move |settings| {
                let dock = match position {
                    DockPosition::Left | DockPosition::Bottom => DatabasePanelDockPosition::Left,
                    DockPosition::Right => DatabasePanelDockPosition::Right,
                };
                settings.dock = Some(dock);
            },
        );
    }

    fn size(&self, cx: &WindowContext) -> Pixels {
        self.width
            .unwrap_or_else(|| DatabasePanelSettings::get_global(cx).default_width)
    }

    fn set_size(&mut self, size: Option<Pixels>, cx: &mut ViewContext<Self>) {
        self.width = size;
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, cx: &WindowContext) -> Option<IconName> {
        DatabasePanelSettings::get_global(cx)
            .button
            .then(|| IconName::Database)
    }

    fn icon_tooltip(&self, _cx: &WindowContext) -> Option<&'static str> {
        Some("Database Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }
}

impl EventEmitter<Event> for DatabasePanel {}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl FocusableView for DatabasePanel {
    fn focus_handle(&self, _cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DatabasePanel {
    fn render(&mut self, _cx: &mut ViewContext<Self>) -> impl gpui::IntoElement {
        // let has_connections = ...
        // if has_connections {
        //    ...
        // } else {

        v_flex()
            .id("empty-database_panel")
            .size_full()
            .p_4()
            .track_focus(&self.focus_handle)
            .child(StyledText::new("No database connections"))

        // }
    }
}
