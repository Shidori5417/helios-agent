//! `--db-path` must be the only database a run touches. HOME points at a regular
//! file, so any open of the default state database fails and the run would error.

use std::{fs, process::Command};

#[test]
fn exec_with_db_path_never_opens_the_default_database() {
    let root = std::env::temp_dir().join(format!("helios-db-isolation-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let home_file = root.join("home-is-a-file");
    fs::write(&home_file, "not a directory").expect("home file");
    let db = root.join("explicit.db");
    for format in ["text", "stream-json"] {
        let output = Command::new(env!("CARGO_BIN_EXE_helios"))
            .arg("--workspace")
            .arg(&root)
            .arg("--db-path")
            .arg(&db)
            .args([
                "exec",
                "--provider",
                "fake",
                "--output-format",
                format,
                "hello",
            ])
            .env("HOME", &home_file)
            .env("XDG_DATA_HOME", &home_file)
            .env("XDG_CONFIG_HOME", &home_file)
            .env("XDG_CACHE_HOME", &home_file)
            .output()
            .expect("run helios");
        assert!(
            output.status.success(),
            "{format}: exec touched the default database: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(db.is_file(), "the explicit database was not used");
    let _ = fs::remove_dir_all(root);
}
