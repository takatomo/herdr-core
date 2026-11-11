//! hc as an app someone else named and signed (`hc.app/Contents/MacOS/hc`):
//! it installs, updates, serves and reaches machines as one whole app, like
//! upstream's own `Herdr Core.app`.

#![cfg(target_os = "macos")]

mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::process::Command;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const THIS: &str = env!("CARGO_PKG_VERSION");

/// This build as `<dir>/<app>/Contents/MacOS/<exe>`, ad-hoc signed under `identifier`.
fn bundle(dir: &Path, app: &str, exe: &str, identifier: &str) -> PathBuf {
    let app = dir.join(app);
    let bin = app.join("Contents/MacOS").join(exe);
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::copy(BIN, &bin).unwrap();
    let plist = std::fs::read_to_string(format!("{ROOT}/../../distribution/macos-Info.plist"))
        .unwrap()
        .replace("@VERSION@", THIS)
        .replace("<string>dev.herdr.core</string>", &format!("<string>{identifier}</string>"))
        .replace("<string>herdr-core</string>", &format!("<string>{exe}</string>"));
    std::fs::write(app.join("Contents/Info.plist"), plist).unwrap();
    let out = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--identifier", identifier])
        .arg(&app)
        .output()
        .unwrap();
    assert!(out.status.success(), "codesign: {out:?}");
    bin
}

/// A session whose hc is the custom app, installed the way an installer does it.
fn installed() -> Session {
    installed_as("hc.app", "hc", "test.custom.hc")
}

fn installed_as(app: &str, exe: &str, identifier: &str) -> Session {
    let mut s = Session::new();
    let source = bundle(&s.root.join("source"), app, exe, identifier);
    let entry = s.root.join(".local/bin/herdr-core");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    let out = s
        .program(&source)
        .arg("--hc-install-bundle")
        .arg(&source)
        .arg(&entry)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(out.status.success(), "custom bundle install: {out:?}");
    s.bin = entry;
    s
}

fn args_of(pid: &serde_json::Value) -> String {
    let out = Command::new("/bin/ps").args(["-o", "args=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_custom_named_bundle_installs_serves_and_updates_as_one_app() {
    let s = installed();
    let active = s.root.join(".local/share/herdr-core/active/hc.app/Contents/MacOS/hc").canonicalize().unwrap();
    assert_eq!(s.bin.canonicalize().unwrap(), active);
    assert_eq!(s.bin.with_file_name("hc").canonicalize().unwrap(), active, "both entry points are the app");

    let t = run(&s, &["sh", "-c", "while read l; do echo got-$l; done"]);
    let status = s.json(&["server", "status"]);
    // A holder must run from the app itself, not an entry-point link, to be found as that app.
    let holder = args_of(&status["holder"]["pid"]);
    assert!(holder.starts_with(&format!("{} hold ", active.display())), "{holder}");

    let newer = bundle(&s.root.join("newer"), "hc.app", "hc", "test.custom.hc");
    let app = newer.parent().unwrap().parent().unwrap().parent().unwrap();
    let updated = s.json(&["update", "--from", app.to_str().unwrap()]);
    assert_eq!(updated["now"], format!("hc {THIS}"));
    assert_ne!(s.json(&["server", "status"])["pid"], status["pid"], "the planned restart");
    assert_eq!(s.bin.canonicalize().unwrap(), active, "the app keeps its one fixed physical path");
    s.json(&["send", &t, "still"]);
    wait_until("the terminal kept running", || text(&s, &t).contains("got-still"));

    let (_, err) = s.err(&["update", "--from", BIN]);
    assert_eq!(err["error"]["kind"], "refused", "a bare executable never replaces the app: {err}");
}

#[test]
fn another_app_never_replaces_a_custom_named_bundle() {
    let s = installed();
    let active = s.bin.canonicalize().unwrap();
    let before = std::fs::read(&active).unwrap();
    // Upstream's own app, as a release or a local build would bring it.
    let official = bundle(&s.root.join("official"), "Herdr Core.app", "herdr-core", "dev.herdr.core");
    let (_, err) = s.err(&["update", "--from", official.to_str().unwrap()]);
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("hc.app") && message.contains("Herdr Core.app"), "{err}");
    assert_eq!(s.bin.canonicalize().unwrap(), active);
    assert_eq!(std::fs::read(&active).unwrap(), before);
    assert!(!s.root.join(".local/share/herdr-core/active/Herdr Core.app").exists());
}

#[test]
fn a_custom_named_bundle_reaches_a_machine_whole() {
    let mut remote = Session::new();
    remote.bin = remote.root.join(".local/bin/herdr-core");
    let mut local = installed();
    local.env.push(fake_ssh(&local, &[("bare", &remote)]));
    let added = local.json(&["machine", "add", "bare", "--install"]);
    assert_eq!(added["name"], "bare");
    let there = remote.root.join(".local/share/herdr-core/active/hc.app");
    assert!(there.join("Contents/Info.plist").is_file(), "the whole app, not its bare executable");
    let active = there.join("Contents/MacOS/hc").canonicalize().unwrap();
    assert_eq!(remote.bin.canonicalize().unwrap(), active);
    assert_eq!(remote.bin.with_file_name("hc").canonicalize().unwrap(), active);
    local.json(&["machine", "update", "bare"]);
    assert_eq!(remote.bin.canonicalize().unwrap(), active);
    local.json(&["machine", "rm", "bare"]);
}

#[test]
fn another_app_never_replaces_a_custom_named_bundle_on_a_machine() {
    let remote = installed();
    let before = remote.bin.canonicalize().unwrap();
    // The entry point there is a link, not the app itself: the app behind it still decides.
    assert!(remote.bin.is_symlink());
    let mut local = installed_as("Herdr Core.app", "herdr-core", "dev.herdr.core");
    local.env.push(fake_ssh(&local, &[("there", &remote)]));
    local.json(&["machine", "add", "there"]);
    let (_, err) = local.err(&["--json", "machine", "update", "there"]);
    assert_eq!(err["error"]["kind"], "install_failed", "{err}");
    assert!(err["error"]["message"].as_str().unwrap_or_default().contains("hc.app"), "{err}");
    assert_eq!(remote.bin.canonicalize().unwrap(), before);
    assert_eq!(remote.bin.with_file_name("hc").canonicalize().unwrap(), before);
    assert!(!remote.root.join(".local/share/herdr-core/active/Herdr Core.app").exists());
    local.json(&["machine", "rm", "there"]);
}

#[test]
fn a_package_managers_seal_on_the_source_app_stays_behind_where_hc_installs_it() {
    use std::os::unix::fs::PermissionsExt;
    let mut remote = Session::new();
    remote.bin = remote.root.join(".local/bin/herdr-core");
    let mut local = Session::new();
    let source = bundle(&local.root.join("source"), "hc.app", "hc", "test.custom.hc");
    // A package manager marks its app as not hc's to replace: the executable's folder is read-only.
    let sealed = source.parent().unwrap();
    let seal = |mode| std::fs::set_permissions(sealed, std::fs::Permissions::from_mode(mode)).unwrap();
    seal(0o555);
    let entry = local.root.join(".local/bin/herdr-core");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    let install = local
        .program(&source)
        .arg("--hc-install-bundle")
        .arg(&source)
        .arg(&entry)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    local.bin = entry;
    local.env.push(fake_ssh(&local, &[("bare", &remote)]));
    let app = sealed.parent().unwrap().parent().unwrap().to_str().unwrap();
    // What hc installed is hc's to update, here and on a machine.
    let update = local.hc(&["--json", "update", "--from", app]);
    // The sealed app itself goes to the machine, as from the package manager's own install.
    local.bin = source.clone();
    let add = local.hc(&["--json", "machine", "add", "bare", "--install"]);
    let again = local.hc(&["--json", "machine", "update", "bare"]);
    let still_sealed = std::fs::metadata(sealed).unwrap().permissions().mode() & 0o222 == 0;
    seal(0o755); // before any assert: a sealed folder would survive the session's cleanup
    for (what, out) in [("install", install), ("update", update), ("machine add", add), ("machine update", again)] {
        assert!(out.status.success(), "{what}: {out:?}");
    }
    assert!(still_sealed, "the source app keeps its seal");
}
