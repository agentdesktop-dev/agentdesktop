// Adapted from Agentgateway's Apache-2.0 licensed yamlviajson module.
pub mod yamlviajson {
    use serde::{de, ser};

    /// Deserialize YAML using JSON's data model and type behavior.
    pub fn from_str<T>(contents: &str) -> anyhow::Result<T>
    where
        T: for<'de> de::Deserialize<'de>,
    {
        let yaml = serde_yaml::Deserializer::from_str(contents);
        let mut json = Vec::with_capacity(128);
        {
            let mut serializer = serde_json::Serializer::new(&mut json);
            serde_transcode::transcode(yaml, &mut serializer)?;
        }
        Ok(serde_json_path_to_error::from_slice(&json)?)
    }

    /// Serialize a value as YAML after passing it through JSON's data model.
    pub fn to_string<T>(value: &T) -> anyhow::Result<String>
    where
        T: ?Sized + ser::Serialize,
    {
        let json = serde_json::to_string(value)?;
        let mut yaml = Vec::with_capacity(128);
        let mut serializer = serde_yaml::Serializer::new(&mut yaml);
        let deserializer = serde_yaml::Deserializer::from_str(&json);
        serde_transcode::transcode(deserializer, &mut serializer)?;
        Ok(String::from_utf8(yaml)?)
    }
}
