use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Open {
        path: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<usize>,
    },
    Show,
    Hide,
    Toggle,
    Status,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ResponseData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            success: true,
            data: None,
            error: None,
        }
    }

    pub fn ok_with_data(data: ResponseData) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseData {
    Status {
        visible: bool,
        open_documents: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        focused_document: Option<PathBuf>,
    },
}

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("garedit.sock")
}
