use agentdesktop_core::config::{DaemonConfig, parse_daemon};
use serde::Deserialize;

#[derive(Deserialize)]
struct BuilderFixture {
    input: DaemonConfig,
    yaml: String,
}

#[test]
fn frontend_yaml_preserves_native_values_and_authentication_in_the_daemon() {
    // The frontend test asserts that its actual generator produces these exact bytes.
    let fixture: BuilderFixture =
        serde_json::from_str(include_str!("fixtures/configuration-builder.json")).unwrap();
    let expected = parse_daemon(&serde_yaml::to_string(&fixture.input).unwrap()).unwrap();
    let actual = parse_daemon(&fixture.yaml).unwrap();

    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}
