use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const INERT_PROFILE: &str = r#"
schema_version = 1
profile_name = "stdio-lifecycle-test"

[permissions]
arbitrary_text = false
arbitrary_local_audio = false

[playback]
minimum_gain = 0.0
maximum_gain = 0.7
default_gain = 0.4
default_concurrency = "enqueue"
allowed_concurrency = ["enqueue", "interrupt"]
maximum_queue_items = 2
maximum_audio_seconds = 0

[outputs]
default_target = "system"

[[outputs.targets]]
id = "system"
description = "Current system default audio device"
kind = "system_default"
allow = ["audio", "speech"]

[tts]
enabled = false
backend = "system"
voice_id = ""
maximum_characters = 1

[logging]
level = "error"
history_enabled = false
history_include_spoken_text = false
"#;

#[test]
fn stdio_eof_stops_the_server_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("inert.toml");
    fs::write(&config, INERT_PROFILE).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-speak"))
        .args(["serve", "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut host_input = child.stdin.take().unwrap();
    writeln!(
        host_input,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"stdio-test","version":"1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        host_input,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    drop(host_input);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "server exited unsuccessfully: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server did not exit after stdio EOF");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn forced_host_termination_reaps_the_server_process() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("inert.toml");
    fs::write(&config, INERT_PROFILE).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-speak"))
        .args(["serve", "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _open_host_input = child.stdin.take().unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(
        child.try_wait().unwrap().is_none(),
        "server exited before the host terminated it"
    );

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
}
