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
    let audit = Command::new(executable)
        .arg("--config").arg(&config_path).args(["audit", "--json"])
        .output().unwrap();
    assert!(audit.status.success(), "audit: {}", String::from_utf8_lossy(&audit.stderr));
    let coverage: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(coverage["sources"][0]["listed_files"], 1);
    assert_eq!(coverage["sources"][0]["present_size_matched"], 1);
    assert_eq!(coverage["sources"][0]["missing_files"], 0);
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
    assert!(
        !another.status.success(),
        "a formerly listed item disappearing must raise a coverage warning"
    );
    assert!(
        String::from_utf8_lossy(&another.stderr).contains("COVERAGE WARNING"),
        "must report the disappearing item"
    );
    assert_eq!(fs::read(dest.join("large.bin")).unwrap(), payload);
    let audit = Command::new(executable)
        .arg("--config").arg(&config_path).args(["audit", "--json"])
        .output().unwrap();
    assert!(!audit.status.success());
    let data: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(data["sources"][0]["historical_absent"], 1);
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


#[test]
fn drive_recovery_cli_plans_immutable_ids_without_local_writes() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("archive");
    let dest = root.join("gdrive");
    fs::create_dir_all(&dest).unwrap();

    let config = tmp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "google-drive"
kind = "rclone"
enabled = false
remote = "gdrive:"
destination = "{}"
"#, root.display(), dest.display()
    )).unwrap();

    let fixture = tmp.path().join("drive-objects.json");
    fs::write(&fixture, serde_json::to_vec(&serde_json::json!([
        {"id":"object-A","name":"private-photo.jpg","mimeType":"image/jpeg","parents":["root"],"size":"123"},
        {"id":"object-B","name":"private-photo.jpg","mimeType":"image/jpeg","parents":["root"],"size":123},
        {"id":"unrelated","name":"different.txt","mimeType":"text/plain","parents":["root"],"size":30}
    ])).unwrap()).unwrap();

    let cli = env!("CARGO_BIN_EXE_syncarium");
    let invoke = || Command::new(cli)
        .arg("--config").arg(&config)
        .args(["drive-recovery-plan", "--source", "google-drive", "--fixture"])
        .arg(&fixture).arg("--json").output().unwrap();

    let first = invoke();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(json["items"].as_array().unwrap().len(), 2);
    assert_eq!(json["known_size_bytes"], 246);
    let path_a = json["items"][0]["relative_destination"].as_str().unwrap();
    let path_b = json["items"][1]["relative_destination"].as_str().unwrap();
    assert_ne!(path_a, path_b);
    assert!(path_a.starts_with(".rclone-duplicates/objects/"));
    let output = String::from_utf8_lossy(&first.stdout);
    assert!(!output.contains("object-A"));
    assert!(!output.contains("private-photo"));
    assert!(!dest.join(".rclone-duplicates").exists(),
        "read-only planning must not create output folders");
    assert_eq!(invoke().stdout, first.stdout, "plans must be deterministic");
}


#[test]
fn provider_listing_below_minimum_is_rejected_without_publishing_bytes() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("archive");
    let dest = root.join("photos");
    let source = temp.path().join("remote");
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("only-one.heic"), b"image").unwrap();
    let config = temp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "photos"
enabled = true
kind = "rclone"
remote = ":local:{}"
destination = "{}"
min_items = 2
"#, root.display(), source.display(), dest.display()
    )).unwrap();
    let bin = env!("CARGO_BIN_EXE_syncarium");
    let check = Command::new(bin)
        .arg("--config").arg(&config).args(["run", "--execute"])
        .output().unwrap();
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stderr).contains("min_items=2"));
    assert!(!dest.join("only-one.heic").exists());
    assert!(!root.join(".syncarium/inventories/photos.json").exists(),
        "rejected inventory must never become a new baseline");
}


#[test]
fn published_receipts_detect_same_size_local_corruption_without_cloud_calls() {
    let tmp = tempdir().unwrap();
    let source = tmp.path().join("remote");
    let root = tmp.path().join("archive");
    let dest = root.join("fixture");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&dest).unwrap();
    fs::write(source.join("file.bin"), b"correct-content").unwrap();
    let config = tmp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "fixture"
kind = "rclone"
enabled = true
remote = ":local:{}"
destination = "{}"
"#, root.display(), source.display(), dest.display())).unwrap();
    let bin = env!("CARGO_BIN_EXE_syncarium");
    let run = Command::new(bin).arg("--config").arg(&config)
        .args(["run", "--execute"]).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let check = || Command::new(bin).arg("--config").arg(&config)
        .args(["verify", "--source", "fixture", "--json"]).output().unwrap();
    let verified = check();
    assert!(verified.status.success(), "{}", String::from_utf8_lossy(&verified.stderr));
    let counts: serde_json::Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(counts["sources"][0]["receipts"], 1);
    assert_eq!(counts["sources"][0]["verified"], 1);

    // This must not require source access: neither receipt nor local archive
    // is affected by remote deletion after the successful transfer.
    fs::remove_file(source.join("file.bin")).unwrap();
    assert!(check().status.success());

    fs::write(dest.join("file.bin"), b"changed-content").unwrap();
    let corrupted = check();
    assert!(!corrupted.status.success());
    let counts: serde_json::Value = serde_json::from_slice(&corrupted.stdout).unwrap();
    assert_eq!(counts["sources"][0]["mismatched"], 1);
    assert_eq!(fs::read(dest.join("file.bin")).unwrap(), b"changed-content");
}


#[test]
fn workspace_export_plan_is_private_read_only_and_reports_unsupported_types() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("archive");
    let dest = root.join("drive");
    fs::create_dir_all(&dest).unwrap();
    let config = tmp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "google-drive"
kind = "rclone"
enabled = false
remote = "unused:"
destination = "{}"
"#, root.display(), dest.display()
    )).unwrap();
    let fixture = tmp.path().join("private-google-objects.json");
    fs::write(&fixture, serde_json::to_vec(&serde_json::json!([
        {"id":"private-id-document", "name":"private-tax-return", "mimeType":"application/vnd.google-apps.document"},
        {"id":"private-id-sheet", "name":"private-budget", "mimeType":"application/vnd.google-apps.spreadsheet"},
        {"id":"private-id-forms", "name":"survey", "mimeType":"application/vnd.google-apps.form"}
    ])).unwrap()).unwrap();
    let command = || Command::new(env!("CARGO_BIN_EXE_syncarium"))
        .arg("--config").arg(&config)
        .args(["drive-workspace-plan", "--source", "google-drive", "--fixture"])
        .arg(&fixture).arg("--json").output().unwrap();
    let first = command();
    assert!(!first.status.success(), "unexportable types cannot be silently treated as complete");
    let parsed: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(parsed["supported"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["unsupported_google_types"], 1);
    assert_eq!(parsed["supported"][0]["size_known_before_export"], false);
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(!stdout.contains("private-id-document"));
    assert!(!stdout.contains("private-tax-return"));
    assert!(!stdout.contains("private-budget"));
    assert!(parsed["supported"].as_array().unwrap().iter().all(|item|
        item["relative_destination"].as_str().unwrap().starts_with("workspace-exports/")));
    assert!(!dest.join("workspace-exports").exists(),
        "read-only plan must not create archive directories");
    assert_eq!(first.stdout, command().stdout, "planning must be deterministic");
}


#[cfg(unix)]
#[test]
fn workspace_source_offline_listing_reports_incomplete_coverage_without_transfers() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("archive");
    let destination = root.join("workspace");
    fs::create_dir_all(&destination).unwrap();

    let fixture = tmp.path().join("google-objects.json");
    fs::write(&fixture, serde_json::to_vec(&serde_json::json!([
        {"id":"valid-doc","name":"Budget",
         "mimeType":"application/vnd.google-apps.spreadsheet",
         "modifiedTime":"2026-10-19T12:00:00Z"},
        {"id":"no-version","name":"Notes",
         "mimeType":"application/vnd.google-apps.document"},
        {"id":"unexportable-form","name":"Survey",
         "mimeType":"application/vnd.google-apps.form"}
    ])).unwrap()).unwrap();

    // The binary invokes "rclone backend query" as its metadata adapter.
    // Keep the integration entirely offline by providing a fixture-backed
    // subprocess in a temp PATH, without accessing any actual user remote.
    let fake_bin = tmp.path().join("bin");
    fs::create_dir(&fake_bin).unwrap();
    let rclone = fake_bin.join("rclone");
    fs::write(&rclone,
        "#!/bin/sh\n[ \"$1\" = \"backend\" ] && [ \"$2\" = \"query\" ] || exit 9\ncat \"$SYNCARIUM_FAKE_DRIVE_JSON\"\n"
    ).unwrap();
    fs::set_permissions(&rclone, fs::Permissions::from_mode(0o755)).unwrap();

    let config = tmp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "workspace"
kind = "google_workspace"
enabled = true
remote = "fake:"
token_file = "{}"
destination = "{}"
"#, root.display(),
        tmp.path().join("token-should-not-exist").display(),
        destination.display()
    )).unwrap();

    let run = |action: &[&str]| Command::new(env!("CARGO_BIN_EXE_syncarium"))
        .arg("--config").arg(&config).args(action)
        .env("PATH", format!("{}:{}", fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()))
        .env("SYNCARIUM_FAKE_DRIVE_JSON", &fixture)
        .output().unwrap();

    let preview = run(&["run"]);
    assert!(!preview.status.success(),
        "unsupported or unversioned Google objects must fail the coverage check");
    let shown = String::from_utf8_lossy(&preview.stdout);
    assert!(shown.contains("1 pending Workspace export(s)"), "{shown}");
    assert!(!destination.join("workspace-exports").exists());
    assert!(!root.join(".syncarium/receipts").exists());

    let audit = run(&["audit", "--json"]);
    assert!(!audit.status.success());
    let report: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    let row = &report["sources"][0];
    assert_eq!(row["workspace_export_candidates"], 1);
    assert_eq!(row["unresolved_provider_items"], 2);
    assert_eq!(row["missing_files"], 1);
    assert_eq!(row["historical_absent"], 0);
    assert!(!destination.join("workspace-exports").exists());
    assert!(!root.join(".syncarium/receipts").exists());
}


#[test]
fn strict_cloud_history_holds_changed_listing_and_keeps_other_source_healthy() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("archive");
    let source = tmp.path().join("remote-photos");
    let healthy = tmp.path().join("remote-other");
    let photos = root.join("photos");
    let other = root.join("other");
    for folder in [&source, &healthy, &photos, &other] {
        fs::create_dir_all(folder).unwrap();
    }
    fs::write(source.join("first.heic"), b"first").unwrap();
    fs::write(source.join("second.heic"), b"second").unwrap();
    let config = tmp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 2
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "photos"
kind = "rclone"
enabled = true
remote = ":local:{}"
destination = "{}"
require_historical_coverage = true
min_items = 1
min_total_bytes = 1

[[sources]]
name = "other"
kind = "rclone"
enabled = true
remote = ":local:{}"
destination = "{}"
"#,
        root.display(), source.display(), photos.display(),
        healthy.display(), other.display()
    )).unwrap();
    let execute = || Command::new(env!("CARGO_BIN_EXE_syncarium"))
        .arg("--config").arg(&config).args(["run", "--execute"])
        .output().unwrap();

    let first = execute();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert_eq!(fs::read(photos.join("first.heic")).unwrap(), b"first");
    assert_eq!(fs::read(photos.join("second.heic")).unwrap(), b"second");
    let ledger = root.join(".syncarium/inventories/photos.json");
    let initial = fs::read(&ledger).unwrap();
    let initial_json: serde_json::Value = serde_json::from_slice(&initial).unwrap();
    assert_eq!(initial_json["hashes"].as_array().unwrap().len(), 2);

    // The provider returns a perfectly valid but incomplete and changed
    // inventory: one old item missing, one previously unseen one added.
    fs::remove_file(source.join("second.heic")).unwrap();
    fs::write(source.join("third.heic"), b"third").unwrap();
    fs::write(healthy.join("healthy.bin"), b"healthy").unwrap();

    let second = execute();
    assert!(!second.status.success(), "strict historical gap must report failure");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("SOURCE HELD"), "{stderr}");
    assert!(stderr.contains("previously observed cloud identities missing"), "{stderr}");
    assert!(!photos.join("third.heic").exists(),
        "strict source must not download from an incomplete listing");
    assert_eq!(fs::read(photos.join("second.heic")).unwrap(), b"second",
        "cloud disappearance must never remove local archives");
    assert_eq!(fs::read(&ledger).unwrap(), initial,
        "rejected listing must not persist newly seen identities");
    assert_eq!(fs::read(other.join("healthy.bin")).unwrap(), b"healthy",
        "healthy providers should continue even if strict cloud listing was held");
}

#[test]
fn minimum_provider_byte_total_rejects_suspiciously_small_listing() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("archive");
    let remote = tmp.path().join("remote");
    let dest = root.join("photos");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&dest).unwrap();
    fs::write(remote.join("small.bin"), b"tiny").unwrap();
    fs::write(remote.join("another.bin"), b"small").unwrap();
    let config = tmp.path().join("config.toml");
    fs::write(&config, format!(
        r#"archive_root = "{}"

[scheduler]
budget_mib = 1
chunk_mib = 1
bandwidth_kib = 10240
run_seconds = 120

[[sources]]
name = "photos"
kind = "rclone"
enabled = true
remote = ":local:{}"
destination = "{}"
min_items = 2
min_total_bytes = 1000
"#, root.display(), remote.display(), dest.display()
    )).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_syncarium"))
        .arg("--config").arg(&config).args(["run", "--execute"])
        .output().unwrap();
    assert!(!result.status.success(), "the byte-size floor must prevent transfers");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("min_total_bytes=1000"), "{stderr}");
    assert!(!dest.join("small.bin").exists());
    assert!(!dest.join("another.bin").exists());
    assert!(!root.join(".syncarium/inventories/photos.json").exists(),
        "no observations should persist for rejected size inventories");
}
