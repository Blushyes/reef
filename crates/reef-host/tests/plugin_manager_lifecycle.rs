//! Lifecycle tests for `PluginManager` driving a real echo-plugin subprocess.
//!
//! Covers: loading manifests from a directory, registering panels + help
//! entries, routing render/command requests to the correct plugin, shutdown
//! notification, and graceful behavior when a plugin binary is missing.

use reef_host::plugin::manager::PluginManager;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

const ECHO_PLUGIN: &str = env!("CARGO_BIN_EXE_echo-plugin");

/// Build a `<tmp>/<name>/reef.json` manifest pointing at the real echo-plugin
/// binary. Declares `panels` and keybindings per the caller's request.
fn write_manifest(
    root: &std::path::Path,
    plugin_name: &str,
    panels: &[&str],
    keybindings: &[(&str, &str, &str)],
) -> std::path::PathBuf {
    let dir = root.join(plugin_name);
    fs::create_dir_all(&dir).unwrap();
    let panels_json: Vec<serde_json::Value> = panels
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "title": id,
                "slot": "sidebar"
            })
        })
        .collect();
    let kbs_json: Vec<serde_json::Value> = keybindings
        .iter()
        .map(|(key, cmd, desc)| {
            serde_json::json!({
                "key": key,
                "command": cmd,
                "description": desc
            })
        })
        .collect();
    let manifest = serde_json::json!({
        "name": plugin_name,
        "version": "0.0.1",
        "main": ECHO_PLUGIN,
        "activation_events": [],
        "contributes": {
            "panels": panels_json,
            "keybindings": kbs_json,
            "commands": []
        }
    });
    fs::write(
        dir.join("reef.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
    dir
}

/// Poll `tick()` until at least `n` responses accumulated on `panels[*].last_render`.
fn tick_until_renders(mgr: &mut PluginManager, at_least: usize, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        mgr.tick();
        let rendered = mgr
            .panels
            .iter()
            .filter(|p| p.last_render.is_some())
            .count();
        if rendered >= at_least || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn load_from_dir_registers_panels_and_help_entries() {
    let tmp = TempDir::new().unwrap();
    write_manifest(
        tmp.path(),
        "alpha",
        &["alpha.main", "alpha.side"],
        &[("a", "alpha.do", "do the thing")],
    );
    write_manifest(
        tmp.path(),
        "beta",
        &["beta.panel"],
        &[("b", "beta.do", "beta does stuff")],
    );

    let mut mgr = PluginManager::new();
    mgr.load_from_dir(tmp.path());

    assert_eq!(mgr.panels.len(), 3, "3 panels across 2 plugins");
    assert_eq!(mgr.help_entries.len(), 2, "2 documented keybindings");
    let plugin_names: Vec<&str> = mgr.panels.iter().map(|p| p.plugin_name.as_str()).collect();
    assert!(plugin_names.contains(&"alpha"));
    assert!(plugin_names.contains(&"beta"));

    mgr.shutdown();
}

#[test]
fn request_render_routed_to_owning_plugin() {
    let tmp = TempDir::new().unwrap();
    write_manifest(tmp.path(), "alpha", &["alpha.main"], &[]);
    write_manifest(tmp.path(), "beta", &["beta.main"], &[]);

    let mut mgr = PluginManager::new();
    mgr.load_from_dir(tmp.path());

    // Request render on alpha.main; echo plugin responds with empty RenderResult.
    mgr.request_render("alpha.main", 80, 20, true, 0);
    tick_until_renders(&mut mgr, 1, Duration::from_secs(3));

    let alpha = mgr
        .panels
        .iter()
        .find(|p| p.decl.id == "alpha.main")
        .expect("alpha panel registered");
    assert!(alpha.last_render.is_some(), "alpha should have rendered");
    let beta = mgr
        .panels
        .iter()
        .find(|p| p.decl.id == "beta.main")
        .expect("beta panel registered");
    assert!(
        beta.last_render.is_none(),
        "beta must not receive alpha's render request"
    );

    mgr.shutdown();
}

#[test]
fn load_plugin_fails_on_missing_binary() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("broken");
    fs::create_dir(&dir).unwrap();
    let manifest = serde_json::json!({
        "name": "broken",
        "version": "0.0.1",
        "main": "/this/path/does/not/exist",
        "activation_events": [],
        "contributes": { "panels": [], "keybindings": [], "commands": [] }
    });
    fs::write(
        dir.join("reef.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();

    let mut mgr = PluginManager::new();
    // load_from_dir must not panic on spawn failure; it prints to stderr and moves on.
    mgr.load_from_dir(tmp.path());
    // No panels registered because load_plugin returned Err before reaching them.
    assert!(mgr.panels.is_empty());
}

#[test]
fn invalidate_panels_sets_flag_on_all() {
    let tmp = TempDir::new().unwrap();
    write_manifest(tmp.path(), "alpha", &["alpha.main"], &[]);
    write_manifest(tmp.path(), "beta", &["beta.main"], &[]);

    let mut mgr = PluginManager::new();
    mgr.load_from_dir(tmp.path());

    // Clear existing needs_render flags (set to true at registration)
    for p in &mut mgr.panels {
        p.needs_render = false;
    }
    mgr.invalidate_panels();
    assert!(mgr.panels.iter().all(|p| p.needs_render));

    mgr.shutdown();
}

#[test]
fn shutdown_completes_without_panic_for_multiple_plugins() {
    let tmp = TempDir::new().unwrap();
    write_manifest(tmp.path(), "alpha", &["a"], &[]);
    write_manifest(tmp.path(), "beta", &["b"], &[]);
    write_manifest(tmp.path(), "gamma", &["g"], &[]);

    let mut mgr = PluginManager::new();
    mgr.load_from_dir(tmp.path());
    assert_eq!(mgr.panels.len(), 3);

    // Just verify shutdown doesn't panic when multiple subprocesses are live.
    mgr.shutdown();
}
