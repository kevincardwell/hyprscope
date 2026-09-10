use serde::Deserialize;

/// One entry of `hyprctl -j clients`.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Client {
    pub address: String,
    pub mapped: bool,
    pub hidden: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: WorkspaceRef,
    pub floating: bool,
    pub monitor: i64,
    pub class: String,
    pub title: String,
    pub initial_class: String,
    pub initial_title: String,
    pub pid: i64,
    pub xwayland: bool,
    pub pinned: bool,
    pub fullscreen: i32,
    pub fullscreen_client: i32,
    pub grouped: Vec<String>,
    pub tags: Vec<String>,
    pub focus_history_i_d: i32,
    pub xdg_tag: String,
    pub content_type: String,
    pub stable_id: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct WorkspaceRef {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    pub monitor: String,
    #[serde(rename = "monitorID")]
    pub monitor_id: i64,
    pub windows: i64,
    pub hasfullscreen: bool,
    pub ispersistent: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub focused: bool,
}

impl Client {
    /// Short label for lists: class, falling back to initial class or the address.
    pub fn label(&self) -> &str {
        if !self.class.is_empty() {
            &self.class
        } else if !self.initial_class.is_empty() {
            &self.initial_class
        } else {
            &self.address
        }
    }

    /// Whether the compositor treats the window as fullscreen (any internal mode).
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen != 0
    }

    pub fn is_grouped(&self) -> bool {
        !self.grouped.is_empty()
    }
}
