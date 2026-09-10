use std::path::PathBuf;

use agentdesktop_core::model::Discovery;

pub(crate) mod metadata {
    pub(crate) use crate::provider::metadata::{home_dir, home_dir_for_uid, user_home_dirs};
}

pub async fn discover() -> Discovery {
    crate::reconcile::Reconciler::new(
        false,
        crate::reconcile::default_claude_code_managed_settings_dir().join("settings.json"),
        crate::reconcile::default_claude_desktop_managed_settings_path(),
        crate::reconcile::default_claude_desktop_credential_helper_path(),
        crate::reconcile::default_codex_managed_config_path(),
        crate::reconcile::default_open_code_managed_config_path(),
        crate::reconcile::default_open_code_plugin_path(),
        PathBuf::new(),
        PathBuf::new(),
    )
    .discover()
    .await
}
