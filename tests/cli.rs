use std::process::Command;

#[test]
fn help_lists_desktop_and_permanent_cli_interfaces() {
    let output = Command::new(env!("CARGO_BIN_EXE_a6-image-studio"))
        .arg("--help")
        .output()
        .expect("CLI help should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gui"));
    assert!(stdout.contains("check"));
    assert!(stdout.contains("smoke-generate"));
}

#[test]
fn smoke_generation_requires_confirmation_before_loading_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_a6-image-studio"))
        .arg("smoke-generate")
        .env_remove("A6API_KEY")
        .output()
        .expect("CLI should run");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warning = stderr
        .find("billable image-generation request")
        .expect("billable warning should be printed");
    let refusal = stderr
        .find("confirmation is required")
        .expect("confirmation error should be printed");
    assert!(warning < refusal);
    assert!(!stderr.contains("A6API_KEY"));
}

#[test]
fn check_reports_missing_api_key_without_network_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_a6-image-studio"))
        .arg("check")
        .env_remove("A6API_KEY")
        .output()
        .expect("CLI should run");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("A6API_KEY is required"));
}
