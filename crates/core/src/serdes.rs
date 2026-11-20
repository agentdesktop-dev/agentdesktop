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

/// Serde helpers for `std::time::Duration` in a humantime-compatible format,
/// such as `90s`, `1h 30m` or `30days`. Days and weeks count as 24 hours and
/// 7 days; months and years are rejected because their length varies.
pub mod duration {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use std::time::Duration;

    pub fn parse(value: &str) -> Result<Duration, jiff::Error> {
        let span: jiff::Span = value.parse()?;
        let duration = span.to_duration(jiff::SpanRelativeTo::days_are_24_hours())?;
        Duration::try_from(duration)
    }

    pub fn format(duration: &Duration) -> String {
        // humantime designators keep the output readable by older daemons.
        jiff::fmt::friendly::SpanPrinter::new()
            .designator(jiff::fmt::friendly::Designator::HumanTime)
            .unsigned_duration_to_string(duration)
    }

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(duration))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let value = String::deserialize(deserializer)?;
        parse(&value).map_err(de::Error::custom)
    }

    pub mod option {
        use serde::{Deserialize, Deserializer, Serializer};
        use std::time::Duration;

        pub fn serialize<S: Serializer>(
            duration: &Option<Duration>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match duration {
                Some(duration) => super::serialize(duration, serializer),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Duration>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|value| super::parse(&value).map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{duration, yamlviajson};
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

    #[test]
    fn durations_accept_humantime_units_and_reject_calendar_units() {
        use std::time::Duration;
        assert_eq!(duration::parse("50ms").unwrap(), Duration::from_millis(50));
        assert_eq!(
            duration::parse("1h 30m").unwrap(),
            Duration::from_secs(5400)
        );
        assert_eq!(
            duration::parse("30days").unwrap(),
            Duration::from_secs(30 * 86400)
        );
        assert_eq!(
            duration::parse("1w").unwrap(),
            Duration::from_secs(7 * 86400)
        );
        assert!(duration::parse("1month").is_err());
        assert!(duration::parse("1year").is_err());
        assert!(duration::parse("-5s").is_err());
    }

    #[test]
    fn durations_format_in_humantime_style() {
        use std::time::Duration;
        assert_eq!(duration::format(&Duration::from_secs(90)), "1m 30s");
        assert_eq!(duration::format(&Duration::from_millis(50)), "50ms");
        assert_eq!(duration::format(&Duration::ZERO), "0s");
    }
}
