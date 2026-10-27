//! Controller side of usage collection: display names from enrolled devices.

use agentdesktop_usage::Labels;
use anyhow::Context;

use crate::database::Database;

/// Labels from enrolled devices: hostnames, and the email (or another
/// readable IdP claim) of the user who enrolled each device.
pub async fn labels(database: &Database) -> anyhow::Result<Labels> {
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, hostname, enrolled_by_subject, idp_claims_json
         FROM devices ORDER BY created_at",
    )
    .fetch_all(database.pool())
    .await
    .context("load usage labels")?;
    let mut labels = Labels::default();
    for (device_id, hostname, subject, idp_claims) in rows {
        labels.insert_device(device_id, hostname);
        // A later enrollment by the same subject wins.
        if let Some(name) = idp_claims.as_deref().and_then(readable_name) {
            labels.insert_user(subject, name);
        }
    }
    Ok(labels)
}

fn readable_name(idp_claims: &str) -> Option<String> {
    let claims: serde_json::Value = serde_json::from_str(idp_claims).ok()?;
    ["email", "preferred_username", "name"]
        .into_iter()
        .find_map(|claim| {
            claims
                .get(claim)?
                .as_str()
                .filter(|value| !value.is_empty())
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::labels;
    use crate::database::Database;

    #[test]
    fn the_controller_creates_the_same_usage_tables() {
        assert_eq!(
            include_str!("../migrations/0004_usage.sql"),
            agentdesktop_usage::SCHEMA
        );
    }

    #[tokio::test]
    async fn labels_name_enrolled_devices_and_their_users() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-usage-labels-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("controller.db");
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect test database");
        let device_id = "7ca03414-bb20-4c80-98ef-7b0538b988ba";
        database
            .enroll_device(
                device_id,
                "build-mac",
                "issuer",
                "user-123",
                Some(
                    &serde_json::from_value(serde_json::json!({ "email": "john@example.com" }))
                        .unwrap(),
                ),
            )
            .await
            .unwrap();
        database
            .enroll_device(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "ci-runner",
                "issuer",
                "user-456",
                None,
            )
            .await
            .unwrap();

        let labels = labels(&database).await.unwrap();
        assert_eq!(labels.device(device_id), Some("build-mac"));
        assert_eq!(labels.user("user-123"), Some("john@example.com"));
        assert_eq!(
            labels.device("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            Some("ci-runner")
        );
        assert_eq!(labels.user("user-456"), None, "no readable claim");

        drop(database);
        let _ = std::fs::remove_dir_all(directory);
    }
}
