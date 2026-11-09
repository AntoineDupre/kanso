//! Skill installation runs locally, without an agent runtime or Kanso server.
use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        for _ in 0..100 {
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kanso-skill-cli-{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create {}: {error}", path.display()),
            }
        }
        panic!("could not create skill test directory");
    }

    fn run(&self, home: Option<&Path>, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanso"));
        // Remove any real credentials, configuration and executable lookup.
        // The installer must use only the binary and the supplied directory.
        command.env_clear().current_dir(&self.0).args(args);
        if let Some(home) = home {
            command.env("HOME", home);
        }
        command.output().expect("run kanso")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installs_each_target_and_preserves_the_legacy_default_without_configuration() {
    for (target, parent) in [
        (None, ".config/opencode/skills"),
        (Some("opencode"), ".config/opencode/skills"),
        (Some("codex"), ".agents/skills"),
    ] {
        let fixture = Fixture::new();
        let home = fixture.0.join("home");
        let mut args = vec!["skill", "--install"];
        if let Some(target) = target {
            args.extend(["--target", target]);
        }
        let output = fixture.run(Some(&home), &args);
        assert_exit(&output, 0);
        assert!(output.stdout.starts_with(b"installed:"));
        assert_eq!(
            fs::read(home.join(parent).join("kanso/SKILL.md")).expect("installed skill"),
            kanso::skill::SKILL.as_bytes()
        );
        let other = if target == Some("codex") {
            ".config/opencode/skills"
        } else {
            ".agents/skills"
        };
        assert!(!home.join(other).exists());
    }
}

#[test]
fn explicit_directory_overrides_either_target_without_home() {
    for target in ["opencode", "codex"] {
        let fixture = Fixture::new();
        let output = fixture.run(
            None,
            &[
                "skill",
                "--install",
                "--target",
                target,
                "--dir",
                "project/.agents/skills",
            ],
        );
        assert_exit(&output, 0);
        assert_eq!(
            fs::read(fixture.0.join("project/.agents/skills/kanso/SKILL.md"))
                .expect("custom skill"),
            kanso::skill::SKILL.as_bytes()
        );
        assert!(!fixture.0.join(".agents").exists());
        assert!(!fixture.0.join(".config").exists());
    }
}

#[test]
fn explicit_directory_without_target_keeps_working_without_home() {
    let fixture = Fixture::new();
    let output = fixture.run(None, &["skill", "--install", "--dir", "custom"]);
    assert_exit(&output, 0);
    assert_eq!(
        fs::read(fixture.0.join("custom/kanso/SKILL.md")).expect("custom skill"),
        kanso::skill::SKILL.as_bytes()
    );
}

#[test]
fn installation_updates_changed_content_and_does_not_rewrite_identical_content() {
    let fixture = Fixture::new();
    let home = fixture.0.join("home");
    let path = home.join(".agents/skills/kanso/SKILL.md");
    fs::create_dir_all(path.parent().expect("parent")).expect("create parents");
    fs::write(&path, "old skill").expect("old skill");
    let args = ["skill", "--install", "--target", "codex"];
    let updated = fixture.run(Some(&home), &args);
    assert_exit(&updated, 0);
    assert!(updated.stdout.starts_with(b"updated:"));
    assert_eq!(
        fs::read(&path).expect("updated skill"),
        kanso::skill::SKILL.as_bytes()
    );

    // Give the file a known old mtime, avoiding sleeps and filesystem clock
    // resolution assumptions when detecting an unnecessary atomic replacement.
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000);
    File::options()
        .write(true)
        .open(&path)
        .expect("open skill")
        .set_times(FileTimes::new().set_modified(modified))
        .expect("set mtime");
    let before = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let repeated = fixture.run(Some(&home), &args);
    assert_exit(&repeated, 0);
    assert!(repeated.stdout.starts_with(b"up to date:"));
    assert_eq!(
        fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        before
    );
    assert_eq!(
        fs::read(&path).expect("unchanged skill"),
        kanso::skill::SKILL.as_bytes()
    );
}

#[test]
fn printing_needs_no_home_and_creates_no_files() {
    let fixture = Fixture::new();
    for args in [vec!["skill"], vec!["skill", "--print"]] {
        let output = fixture.run(None, &args);
        assert_exit(&output, 0);
        assert_eq!(output.stdout, kanso::skill::SKILL.as_bytes());
    }
    assert_eq!(fs::read_dir(&fixture.0).expect("directory").count(), 0);
}

#[test]
fn invalid_options_fail_before_writing_files() {
    let fixture = Fixture::new();
    for args in [
        vec!["skill", "--target", "codex"],
        vec!["skill", "--target", "opencode"],
        vec!["skill", "--print", "--target", "codex"],
        vec![
            "skill",
            "--install",
            "--target",
            "unknown",
            "--dir",
            "custom",
        ],
        vec!["skill", "--dir", "custom"],
        vec!["skill", "--print", "--install"],
    ] {
        let output = fixture.run(Some(&fixture.0), &args);
        assert_exit(&output, 1);
        assert!(output.stdout.is_empty());
    }
    assert_eq!(fs::read_dir(&fixture.0).expect("directory").count(), 0);
}

#[test]
fn target_default_requires_a_nonempty_home_and_does_not_install_relatively() {
    let fixture = Fixture::new();
    for args in [
        vec!["skill", "--install"],
        vec!["skill", "--install", "--target", "opencode"],
        vec!["skill", "--install", "--target", "codex"],
    ] {
        for home in [None, Some(Path::new(""))] {
            let output = fixture.run(home, &args);
            assert_exit(&output, 1);
            assert!(String::from_utf8_lossy(&output.stderr).contains("--dir"));
        }
    }
    assert_eq!(fs::read_dir(&fixture.0).expect("directory").count(), 0);
}
