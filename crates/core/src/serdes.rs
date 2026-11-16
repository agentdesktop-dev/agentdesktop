// Adapted from Agentgateway's Apache-2.0 licensed yamlviajson module.
pub mod yamlviajson {
    use serde::{de, ser};

    /// YAML 1.2 Core scalar resolution, matching the previous serde_yaml parser.
    fn options() -> serde_saphyr::Options {
        let mut options = serde_saphyr::Options::default();
        options.strict_booleans = true;
        options.leading_zero_numbers_as_strings = true;
        options.underscored_numbers_as_strings = true;
        options.reject_unsupported_tags = true;
        // Snippets would copy configuration text, including secrets, into errors.
        options.with_snippet = false;
        options
    }

    /// Deserialize YAML using JSON's data model and type behavior.
    pub fn from_str<T>(contents: &str) -> anyhow::Result<T>
    where
        T: for<'de> de::Deserialize<'de>,
    {
        let value: serde_json::Value = serde_saphyr::from_str_with_options(contents, options())?;
        let json = serde_json::to_vec(&value)?;
        Ok(serde_json_path_to_error::from_slice(&json)?)
    }

    /// Serialize a value as YAML after passing it through JSON's data model.
    pub fn to_string<T>(value: &T) -> anyhow::Result<String>
    where
        T: ?Sized + ser::Serialize,
    {
        let json = serde_json::to_string(value)?;
        let mut yaml = String::with_capacity(128);
        let mut serializer = serde_saphyr::Serializer::new(&mut yaml);
        let mut deserializer = serde_json::Deserializer::from_str(&json);
        serde_transcode::transcode(&mut deserializer, &mut serializer)?;
        Ok(yaml)
    }
}

#[cfg(test)]
mod tests {
    use super::yamlviajson;
    use serde_json::{Value, json};

    #[test]
    fn resolves_scalars_like_yaml_1_2_core() {
        let value: Value = yamlviajson::from_str(
            "answer: no\nswitch: on\nmode: 0755\nsize: 1_000\nenabled: true\nport: 8443",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "answer": "no",
                "switch": "on",
                "mode": "0755",
                "size": "1_000",
                "enabled": true,
                "port": 8443,
            })
        );
    }

    #[test]
    fn rejects_duplicate_keys_and_unknown_tags() {
        assert!(yamlviajson::from_str::<Value>("a: 1\na: 2").is_err());
        assert!(yamlviajson::from_str::<Value>("a: !custom value").is_err());
    }

    #[test]
    fn parse_errors_do_not_echo_the_document() {
        let error =
            yamlviajson::from_str::<Value>("databaseUrl: postgres://u:hunter2@db/x\nbad: [")
                .unwrap_err();
        assert!(!format!("{error:#}").contains("hunter2"), "{error:#}");
    }

    #[test]
    fn round_trips_ambiguous_strings() {
        let original = json!({"enabled": "yes", "count": "123", "pair": "a: b", "nested": {"list": [1, "two"]}});
        let yaml = yamlviajson::to_string(&original).unwrap();
        assert_eq!(yamlviajson::from_str::<Value>(&yaml).unwrap(), original);
    }
}
