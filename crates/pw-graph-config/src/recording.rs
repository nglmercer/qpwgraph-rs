//! TOML configuration compatible with the state surface described by qpwgraph.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RecordingSaveMode {
    #[default]
    AskOnStop,
    AutoSave,
}
impl RecordingSaveMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AskOnStop => "ask",
            Self::AutoSave => "auto",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "autosave" | "auto-save" => Self::AutoSave,
            _ => Self::AskOnStop,
        }
    }
}
