use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
provider = "system"
maximum_characters = 1

[tts.provider_options]
voice_id = ""

[logging]
level = "error"
history_enabled = false
history_include_spoken_text = false
"#;

#[test]
fn stdio_eof_stops_the_server_and_removes_its_control_descriptor() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("inert.toml");
    let control = directory.path().join("control.json");
    fs::write(&config, INERT_PROFILE).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-speak"))
        .args(["serve", "--config"])
        .arg(config)
        .args(["--control-file"])
        .arg(&control)
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

    let descriptor_deadline = Instant::now() + Duration::from_secs(5);
    while !control.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited before creating its control descriptor"
        );
        assert!(
            Instant::now() < descriptor_deadline,
            "server did not create its control descriptor"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let descriptor: serde_json::Value =
        serde_json::from_slice(&fs::read(&control).unwrap()).unwrap();
    assert_eq!(descriptor["schema_version"], 1);
    assert_eq!(descriptor["host"], "127.0.0.1");
    assert_eq!(descriptor["token"].as_str().unwrap().len(), 64);
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&control).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(host_input);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "server exited unsuccessfully: {status}");
            assert!(!control.exists(), "control descriptor survived shutdown");
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
