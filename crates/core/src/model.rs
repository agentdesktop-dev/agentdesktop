use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Discovery {
    pub agents: Vec<Agent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub kind: String,
    pub executable: PathBuf,
    pub version: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Health {
    pub status: String,
}
