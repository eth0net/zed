use gpui::{AppContext, Pixels};
use schemars::JsonSchema;
use serde_derive::{Deserialize, Serialize};
use settings::{Settings, SettingsSources};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DatabasePanelDockPosition {
    Left,
    Right,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct DatabasePanelSettings {
    pub button: bool,
    pub default_width: Pixels,
    pub dock: DatabasePanelDockPosition,
    pub indent_size: f32,
    pub auto_reveal_entries: bool,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema, Debug)]
pub struct DatabasePanelSettingsContent {
    /// Whether to show the database panel button in the status bar.
    ///
    /// Default: true
    pub button: Option<bool>,
    /// Customise default width (in pixels) taken by database panel
    ///
    /// Default: 240
    pub default_width: Option<f32>,
    /// The position of database panel
    ///
    /// Default: left
    pub dock: Option<DatabasePanelDockPosition>,
    /// Amount of indentation (in pixels) for nested items.
    ///
    /// Default: 20
    pub indent_size: Option<f32>,
    /// Whether to reveal it in the database panel automatically,
    /// when a corresponding database entry becomes active.
    ///
    /// Default: true
    pub auto_reveal_entries: Option<bool>,
}

impl Settings for DatabasePanelSettings {
    const KEY: Option<&'static str> = Some("database_panel");

    type FileContent = DatabasePanelSettingsContent;

    fn load(
        sources: SettingsSources<Self::FileContent>,
        _: &mut AppContext,
    ) -> anyhow::Result<Self> {
        sources.json_merge()
    }
}
