use std::{fs, path::Path};

use serde::de::DeserializeOwned;

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

pub(crate) fn read_json5<T: DeserializeOwned>(path: &Path) -> Option<T> {
    json5::from_str(&fs::read_to_string(path).ok()?).ok()
}

pub(crate) fn read_toml<T: DeserializeOwned>(path: &Path) -> Option<T> {
    toml::from_str(&fs::read_to_string(path).ok()?).ok()
}
