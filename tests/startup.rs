//! Integration test for the binary's startup behaviour.

use std::process::Command;

/// A missing required variable aborts startup with the error's `Display`
/// message (not the `Debug` form) and a non-zero exit code.
#[test]
fn missing_api_key_reports_clear_message_and_exits_nonzero() {
    // Run from the temp dir so a local `.env` (loaded via `dotenvy` from the
    // cwd) can't supply the key and defeat this test.
    let output = Command::new(env!("CARGO_BIN_EXE_bx402"))
        .current_dir(std::env::temp_dir())
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

/// A bad `ENABLED_RAILS` value aborts startup with a message naming the
/// variable, instead of silently serving with a rail missing.
#[test]
fn bad_enabled_rails_reports_clear_message_and_exits_nonzero() {
    // The API key is set explicitly so the failure is the rails value, and the
    // temp-dir cwd keeps a local `.env` out of the picture.
    let output = Command::new(env!("CARGO_BIN_EXE_bx402"))
        .current_dir(std::env::temp_dir())
        .env("BRAVE_SEARCH_API_KEY", "test-key")
        .env("ENABLED_RAILS", "btc")
        .output()
        .expect("failed to run the bx402 binary");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid configuration: ENABLED_RAILS"),
        "stderr was: {stderr:?}"
    );
}
