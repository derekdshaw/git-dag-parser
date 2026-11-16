//! The `init` command, run as a process on a fixture repository

use std::process::Command;

use test_fixtures::Repo;

/// The stdout of `init` on `repo` with the extra arguments
fn init(repo: &Repo, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_git-dag-parser"))
        .arg("init")
        .arg("--repo")
        .arg(&repo.work)
        .args(extra)
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "init {extra:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn has_line(out: &str, f: impl Fn(&str) -> bool) -> bool {
    out.lines().any(f)
}

#[test]
fn init_prints_the_analysis_only_with_report() {
    let repo = Repo::with_history();

    let plain = init(&repo, &[]);
    assert!(has_line(&plain, |l| l == "=== DAG Summary ==="), "{plain}");
    assert!(!has_line(&plain, |l| l == "=== Analysis ==="), "{plain}");
    assert!(!has_line(&plain, |l| l.starts_with("Analysis:")), "{plain}");

    let report = init(&repo, &["--report"]);
    assert!(has_line(&report, |l| l == "=== Analysis ==="), "{report}");
    assert!(
        has_line(&report, |l| l.starts_with("Analysis:")),
        "{report}"
    );
    assert!(
        has_line(&report, |l| l.starts_with("Largest blobs")),
        "{report}"
    );
}
