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
            let status = Command::new(executable)
                .arg("--config").arg(&config_path).args(["status", "--json"])
                .output().expect("local-only status starts");
            assert!(status.status.success(),
                "status: {}", String::from_utf8_lossy(&status.stderr));
            let progress: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
            assert_eq!(progress["partial_files"], 1);
            assert_eq!(progress["sources"][0]["committed_bytes"],
                (attempt as u64 + 1) * 1024 * 1024);
        }
    }
    assert_eq!(fs::read(dest.join("large.bin")).unwrap(), payload);
    let status = Command::new(executable)
        .arg("--config").arg(&config_path).args(["status", "--json"])
        .output().unwrap();
    assert!(status.status.success());
    let progress: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(progress["partial_files"], 0);

    // The source may disappear, but a completed archived file must remain untouched.
    fs::remove_file(source.join("large.bin")).unwrap();
    let another = Command::new(executable)
        .arg("--config").arg(&config_path).args(["run", "--execute"])
        .output().unwrap();
    assert!(another.status.success());
    assert_eq!(fs::read(dest.join("large.bin")).unwrap(), payload);
}


#[test]
fn failed_cloud_source_does_not_starve_a_healthy_source() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("healthy-remote");
    let root = temp.path().join("archive");
    let healthy_destination = root.join("healthy");
    let broken_destination = root.join("broken");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&healthy_destination).unwrap();
    fs::create_dir_all(&broken_destination).unwrap();
    fs::write(source.join("survives.txt"), b"independent source").unwrap();

    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        format!(
            r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "expired-icloud"
kind = "rclone"
enabled = true
remote = "syncarium-missing-unconfigured-remote:"
destination = "{}"

[[sources]]
name = "healthy-google"
kind = "rclone"
enabled = true
remote = ":local:{}"
destination = "{}"
"#,
            root.display(),
            broken_destination.display(),
            source.display(),
            healthy_destination.display()
        ),
    )
    .unwrap();

    let executable = env!("CARGO_BIN_EXE_syncarium");
    let output = Command::new(executable)
        .arg("--config")
        .arg(&config)
        .args(["run", "--execute"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "source failures must not be reported as total success"
    );
    assert_eq!(
        fs::read(healthy_destination.join("survives.txt")).unwrap(),
        b"independent source"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("listing failed"),
        "the failed provider should be identified"
    );
}
