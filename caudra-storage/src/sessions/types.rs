use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Ask,
    Auto,
    Yolo,
}

impl From<bool> for PermissionMode {
    fn from(yolo: bool) -> Self {
        if yolo { Self::Yolo } else { Self::Ask }
    }
}
