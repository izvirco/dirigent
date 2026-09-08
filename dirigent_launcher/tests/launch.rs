//! Exercise the actual launcher, using this test executable as a small desktop stand-in.
use dirigent_launcher::{LAUNCHER_EXE, Selection};
use std::{
    env, fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn desktop_child() {
    let Some(output) = env::var_os("DIRIGENT_LAUNCHER_TEST_OUTPUT") else {
        return;
    };
    let output = std::path::PathBuf::from(output);
    let args: Vec<String> = env::args().collect();
    assert!(args.iter().any(|arg| arg == "a path with spaces/λ"));
    fs::write(&output, b"started").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output.with_extension("exit").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(output.with_extension("finished"), b"finished").unwrap();
}

#[test]
fn launcher_forwards_arguments_and_exits_without_waiting_for_desktop() {
    let root = tempfile::tempdir().unwrap();
    let selected = Selection::new("unstable", "20260908-102910").unwrap();
    fs::create_dir_all(selected.executable(root.path()).parent().unwrap()).unwrap();
    fs::copy(
        env::current_exe().unwrap(),
        selected.executable(root.path()),
    )
    .unwrap();
    selected.write(root.path()).unwrap();
    let launcher = root.path().join(LAUNCHER_EXE);
    fs::copy(env!("CARGO_BIN_EXE_dirigent"), &launcher).unwrap();
    let output = root.path().join("child-output");
    let mut child = Command::new(launcher)
        .args([
            "--exact",
            "desktop_child",
            "--skip",
            "a path with spaces/λ",
            "--nocapture",
        ])
        .env("DIRIGENT_LAUNCHER_TEST_OUTPUT", &output)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "launcher waited for desktop");
        thread::sleep(Duration::from_millis(10));
    }
    while !output.exists() {
        assert!(Instant::now() < deadline, "desktop was not launched");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!output.with_extension("finished").exists());
    // The old executable is still mapped here, including on Windows. Updating must not touch it.
    let stage = tempfile::tempdir_in(root.path().join("versions")).unwrap();
    fs::copy(
        env::current_exe().unwrap(),
        stage.path().join(dirigent_launcher::DESKTOP_EXE),
    )
    .unwrap();
    let next = Selection::new("unstable", "20260908-102911").unwrap();
    {
        let _lock = dirigent_launcher::lock(root.path(), true).unwrap();
        dirigent_launcher::publish(root.path(), stage.path(), next.clone()).unwrap();
    }
    assert_eq!(Selection::read(root.path()).unwrap().version, next.version);
    assert!(selected.executable(root.path()).exists());
    fs::write(output.with_extension("exit"), b"exit").unwrap();
    while !output.with_extension("finished").exists() {
        assert!(Instant::now() < deadline, "desktop did not finish");
        thread::sleep(Duration::from_millis(10));
    }
}
