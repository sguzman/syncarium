use std::fs;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn local_backend_resumes_large_file_across_bounded_runs() {
    let temp = tempdir().expect("temporary test root");
    let source = temp.path().join("remote");
    let archive = temp.path().join("archive");
    let dest = archive.join("fixture");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&dest).unwrap();

    let payload: Vec<u8> = (0..2_500_000).map(|n| (n % 251) as u8).collect();
    fs::write(source.join("large.bin"), &payload).unwrap();

    let config_path = temp.path().join("config.toml");
    // rclone's on-the-fly local backend does not require any account credentials.
    let content = format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "fixture"
enabled = true
kind = "rclone"
remote = ":local:{}"
destination = "{}"
"#,
        archive.display(), source.display(), dest.display()
    );
    fs::write(&config_path, content).unwrap();
    let executable = env!("CARGO_BIN_EXE_syncarium");

    let doctor = Command::new(executable)
        .arg("--config").arg(&config_path).arg("doctor")
        .output().expect("doctor starts");
    assert!(doctor.status.success(), "doctor: {}", String::from_utf8_lossy(&doctor.stderr));

    let preview = Command::new(executable)
        .arg("--config").arg(&config_path).arg("run")
        .output().expect("dry run starts");
    assert!(preview.status.success(), "preview: {}", String::from_utf8_lossy(&preview.stderr));
    assert!(!dest.join("large.bin").exists(), "dry run must not download anything");

    for attempt in 0..3 {
        let result = Command::new(executable)
            .arg("--config").arg(&config_path).args(["run", "--execute"])
            .output().expect("execute starts");
        assert!(result.status.success(),
            "run {} failed:\nstdout={}\nstderr={}", attempt + 1,
            String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        if attempt < 2 {
            assert!(!dest.join("large.bin").exists(), "partial must not be published");
        }
    }
    assert_eq!(fs::read(dest.join("large.bin")).unwrap(), payload);

    // The source may disappear, but a completed archived file must remain untouched.
    fs::remove_file(source.join("large.bin")).unwrap();
    let another = Command::new(executable)
        .arg("--config").arg(&config_path).args(["run", "--execute"])
        .output().unwrap();
    assert!(another.status.success());
    assert_eq!(fs::read(dest.join("large.bin")).unwrap(), payload);
}
