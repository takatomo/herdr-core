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
    let mut s = Session::new();
    let source = bundle(&s.root.join("source"), "hc.app", "hc", "test.custom.hc");
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
