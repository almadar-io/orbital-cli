//! `orb update` · `orb add` · `orb remove` · `orb list` (and that a read-only
//! command never sets the store up). bun is stubbed (`ORB_BUN_PATH`): `add` links each named package to a
//! real installed copy or a fixture, `remove` deletes it — so every check runs
//! against real data. The default programs (the TypeScript shell, orb-desktop)
//! are fixture packages whose binaries answer the real protocol.

use orbital_core::desktop_protocol::DesktopReply;
use orbital_core::shell_protocol::{platform, ShellDescription, IR_VERSION};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn orb_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orb"))
}

fn start() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn installed(name: &str) -> PathBuf {
    orbital_core::behavior_packages::find_npm_package(name, start(), &orbital_core::behavior_packages::user_store_dir().unwrap_or_default())
        .unwrap_or_else(|| panic!("the workspace {name}"))
        .root
}

fn real_core() -> PathBuf {
    orbital_test_support::installed_core_language(start()).root.clone()
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn executable(path: &Path, script: &str) {
    write(path, script);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// An `orbProgram` package (`meta`) and its platform package carrying `binary`,
/// a script that prints `reply` whatever it is asked.
fn program(dir: &Path, meta: &str, program: serde_json::Value, binary: &str, reply: &str) -> (PathBuf, PathBuf) {
    let root = dir.join(format!("fixture-{}", meta.replace(['@', '/'], "_")));
    write(&root.join("package.json"), &serde_json::json!({ "name": meta, "version": "18.0.0", "orbProgram": program }).to_string());
    let platform_name = format!("{meta}-{}", platform());
    let platform_root = dir.join(format!("fixture-{}", platform_name.replace(['@', '/'], "_")));
    write(
        &platform_root.join("package.json"),
        &serde_json::json!({ "name": platform_name, "version": "18.0.0", "bin": { binary: format!("bin/{binary}") } }).to_string(),
    );
    executable(&platform_root.join("bin").join(binary), &format!("#!/bin/sh\ncat <<'JSON'\n{reply}\nJSON\n"));
    (root, platform_root)
}

fn shell_reply(min: u32, max: u32) -> String {
    let d = ShellDescription {
        name: "typescript".into(),
        version: "18.0.0".into(),
        ir_versions: orbital_core::shell_protocol::IrVersions { min, max },
        options: vec![],
        needs_resolved: true,
        needs_served: false,
        web_client: None,
    };
    serde_json::to_string(&d).unwrap()
}

fn desktop_reply() -> String {
    serde_json::to_string(&DesktopReply::Coverage { supported: vec!["typography".into()], stubbed: vec![] }).unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    bun: PathBuf,
}

/// A project, its bun stand-in and the packages bun can "install".
fn fixture(core: Option<PathBuf>, shell_ir: (u32, u32)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    write(&p.join("package.json"), r#"{"name":"pkg-test","private":true}"#);
    let core = core.unwrap_or_else(real_core);
    let pack = p.join("fixture-lolo-xx");
    write(&pack.join("package.json"), r#"{"name":"lolo-xx","version":"1.0.0","orbVocabulary":{"compiler":">=0.0.0","vocabulary":"vocabulary.json"}}"#);
    write(&pack.join("vocabulary.json"), r#"{"meta":{"code":"xx","name":"Xx","rtl":false},"keywords":{"orbital":"xx-orbital"}}"#);
    let plain = p.join("fixture-not-orb");
    write(&plain.join("package.json"), r#"{"name":"not-orb","version":"1.0.0"}"#);
    let (shell, shell_platform) = program(p, "@almadar/orb-shell-typescript", serde_json::json!({ "kind": "shell", "name": "typescript" }), "orb-shell-typescript", &shell_reply(shell_ir.0, shell_ir.1));
    let (desktop, desktop_platform) = program(p, "@almadar/orb-desktop", serde_json::json!({ "kind": "desktop" }), "orb-desktop", &desktop_reply());
    let links = [
        ("@almadar/core".to_string(), core),
        ("@almadar/std".to_string(), installed("@almadar/std")),
        ("@almadar/lolo-ar".to_string(), installed("@almadar/lolo-ar")),
        ("@almadar/lolo-sl".to_string(), installed("@almadar/lolo-sl")),
        ("@almadar/orb-shell-typescript".to_string(), shell),
        (format!("@almadar/orb-shell-typescript-{}", platform()), shell_platform),
        ("@almadar/orb-desktop".to_string(), desktop),
        (format!("@almadar/orb-desktop-{}", platform()), desktop_platform),
        ("lolo-xx".to_string(), pack),
        ("not-orb".to_string(), plain),
    ];
    // A spec `name@range` links `name`; an `orbProgram` package brings its platform package along.
    let mut cases = String::new();
    for (name, target) in &links {
        if name.ends_with(platform()) {
            continue;
        }
        let mut lines = format!("ln -sfn \"{}\" \"node_modules/{name}\"", target.display());
        if let Some((pname, ptarget)) = links.iter().find(|(n, _)| *n == format!("{name}-{}", platform())) {
            lines.push_str(&format!("; ln -sfn \"{}\" \"node_modules/{pname}\"", ptarget.display()));
        }
        cases.push_str(&format!("        \"{name}\"|\"{name}\"@*) {lines} ;;\n"));
    }
    let script = format!(
        "#!/bin/sh\nset -e\nverb=\"$1\"; shift\nmkdir -p node_modules/@almadar\nfor spec in \"$@\"; do\n  case \"$verb\" in\n    add)\n      case \"$spec\" in\n{cases}      esac ;;\n    remove) rm -rf \"node_modules/$spec\" \"node_modules/$spec-{platform}\" ;;\n  esac\ndone\n",
        platform = platform()
    );
    let bun = p.join("fake-bun.sh");
    executable(&bun, &script);
    Fixture { dir, bun }
}

fn orb(f: &Fixture, args: &[&str]) -> Output {
    Command::new(orb_bin())
        .args(args)
        .current_dir(f.dir.path())
        .env("ORB_BUN_PATH", &f.bun)
        .env("HOME", f.dir.path())
        .env("ORB_NO_UPDATE_CHECK", "1")
        .env_remove("ORB_NO_AUTO_SETUP")
        .output()
        .expect("spawn orb")
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

fn list(f: &Fixture) -> serde_json::Value {
    let out = orb(f, &["list", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn row<'a>(rows: &'a serde_json::Value, package: &str) -> &'a serde_json::Value {
    rows.as_array().unwrap().iter().find(|r| r["package"] == package).unwrap_or_else(|| panic!("{package} listed: {rows}"))
}

#[test]
fn update_installs_everything_orb_needs_and_each_loads() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    let out = orb(&f, &["update"]);
    assert!(out.status.success(), "{}", text(&out));
    let rows = list(&f);
    for (package, kind) in [
        ("@almadar/core", "language"),
        ("@almadar/std", "behaviors"),
        ("@almadar/lolo-ar", "language-pack"),
        ("@almadar/orb-shell-typescript", "shell"),
        ("@almadar/orb-desktop", "desktop"),
    ] {
        let r = row(&rows, package);
        assert!(r["kinds"].as_array().unwrap().iter().any(|k| k == kind), "{package}: {r}");
        assert!(r["problem"].is_null(), "{package}: {r}");
    }
}

#[test]
fn control_a_core_whose_data_does_not_load_is_refused_and_removed() {
    let broken = tempfile::tempdir().unwrap();
    write(&broken.path().join("package.json"), r#"{"name":"@almadar/core","version":"1.0.0","orbLanguage":{}}"#);
    let f = fixture(Some(broken.path().to_path_buf()), (IR_VERSION, IR_VERSION));
    let out = orb(&f, &["update"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("ORB_LANGUAGE_DATA_"), "{}", text(&out));
    assert!(!f.dir.path().join("node_modules/@almadar/core").exists(), "what this call added is removed");
}

#[test]
fn edge_a_shell_that_does_not_accept_this_orbs_ir_is_refused() {
    let f = fixture(None, (IR_VERSION + 1, IR_VERSION + 1));
    let out = orb(&f, &["update"]);
    assert!(!out.status.success());
    assert!(text(&out).contains(&format!("this orb emits IR {IR_VERSION}")), "{}", text(&out));
}

#[test]
fn add_extends_the_language_with_a_community_pack() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    assert!(orb(&f, &["update"]).status.success());
    let out = orb(&f, &["add", "lolo-xx@1.0.0"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("added lolo-xx@1.0.0 (language pack)"), "{}", text(&out));
    assert!(row(&list(&f), "lolo-xx")["problem"].is_null());
}

#[test]
fn control_a_package_that_is_no_orb_package_is_refused_and_removed() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    let out = orb(&f, &["add", "not-orb"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("not an orb package"), "{}", text(&out));
    assert!(!f.dir.path().join("node_modules/not-orb").exists());
}

#[test]
fn remove_uninstalls_a_package() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    assert!(orb(&f, &["update"]).status.success());
    assert!(orb(&f, &["remove", "@almadar/orb-desktop"]).status.success());
    assert!(list(&f).as_array().unwrap().iter().all(|r| r["package"] != "@almadar/orb-desktop"));
}

fn write_app(dir: &Path) {
    write(&dir.join("app.orb"), r#"{"name":"a","version":"1.0.0","orbitals":[]}"#);
}

fn bun_calls(f: &Fixture) -> bool {
    f.dir.path().join("node_modules").exists()
}

#[test]
fn a_read_only_command_never_sets_the_user_store_up() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    write_app(f.dir.path());
    let out = orb(&f, &["validate", "app.orb"]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("ORB_LANGUAGE_DATA_NOT_INSTALLED"), "{t}");
    assert!(t.contains("orb update --global"), "{t}");
    assert!(!t.contains("running `orb update"), "{t}");
    assert!(!f.dir.path().join(".orb").exists(), "the store is untouched: {t}");
    assert!(!bun_calls(&f), "bun was never run: {t}");
}

#[test]
fn an_invalid_user_store_fails_with_the_typed_error_and_its_fix() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    write_app(f.dir.path());
    let core = f.dir.path().join(".orb/node_modules/@almadar/core/package.json");
    write(&core, r#"{"name":"@almadar/core","version":"1.0.0","orbLanguage":{}}"#);
    let before = std::fs::read(&core).unwrap();
    let out = orb(&f, &["validate", "app.orb"]);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("ORB_LANGUAGE_DATA_"), "{t}");
    assert!(t.contains("orb update --global"), "{t}");
    assert!(!t.contains("running `orb update"), "{t}");
    assert_eq!(std::fs::read(&core).unwrap(), before, "{t}");
    assert!(!bun_calls(&f), "no install ran: {t}");
}

#[test]
fn control_an_explicit_global_update_makes_the_same_command_work() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    write_app(f.dir.path());
    let up = orb(&f, &["update", "--global"]);
    assert!(up.status.success(), "{}", text(&up));
    assert!(f.dir.path().join(".orb/node_modules/@almadar/core").exists());
    let out = orb(&f, &["validate", "app.orb"]);
    assert!(!text(&out).contains("ORB_LANGUAGE_DATA_"), "{}", text(&out));
}

#[test]
fn edge_a_projects_own_core_is_never_set_up_over() {
    let f = fixture(None, (IR_VERSION, IR_VERSION));
    let broken = f.dir.path().join("broken-core");
    write(&broken.join("package.json"), r#"{"name":"@almadar/core","version":"1.0.0"}"#);
    std::fs::create_dir_all(f.dir.path().join("node_modules/@almadar")).unwrap();
    std::os::unix::fs::symlink(&broken, f.dir.path().join("node_modules/@almadar/core")).unwrap();
    write_app(f.dir.path());
    let t = text(&orb(&f, &["validate", "app.orb"]));
    assert!(t.contains("ORB_LANGUAGE_DATA_INVALID"), "{t}");
    assert!(!t.contains("running `orb update"), "{t}");
    assert!(!f.dir.path().join(".orb").exists(), "{t}");
}
