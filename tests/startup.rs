//! Integration test for the binary's startup behaviour.

use std::process::Command;

/// A missing required variable aborts startup with the error's `Display`
/// message (not the `Debug` form) and a non-zero exit code.
#[test]
fn missing_api_key_reports_clear_message_and_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_bx402"))
        .env_remove("BRAVE_SEARCH_API_KEY")
        .output()
        .expect("failed to run the bx402 binary");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing required configuration: BRAVE_SEARCH_API_KEY"),
        "stderr was: {stderr:?}"
    );
}
