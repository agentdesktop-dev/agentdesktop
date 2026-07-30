use std::process::Command;

#[test]
fn help_exits_successfully() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-connector"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--upstream <UPSTREAM>")
    );
}
