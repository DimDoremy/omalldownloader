//! Architecture guard and short-write / crash-consistency tests.
//!
//! The first test is the one that keeps the split honest: the engine and everything
//! under it must never grow a dependency on a user interface. When someone adds one,
//! this test says so while the compiler is still all too happy to help.

use std::process::Command;

#[test]
fn the_core_layers_carry_no_user_interface() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    // `omad-engine` depends on omad-core and, transitively, on nothing that draws.
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "omad-engine",
            "--prefix",
            "none",
            "--no-dedupe",
        ])
        .current_dir(manifest)
        .output()
        .expect("cargo tree should run");
    let tree = String::from_utf8_lossy(&output.stdout);

    let forbidden = [
        "gpui",
        "odk",
        "odk-ui",
        "odk-theme",
        "egui",
        "eframe",
        "tao",
        "wry",
        "winit",
        "iced",
        "slint",
        "cocoa",
        "objc",
        "windows-core",
        "flutter",
    ];

    let mut found = Vec::new();
    for name in forbidden {
        // A crate named exactly `name`, or `name-with-suffix`, at the start of a line.
        let pattern = name.to_string();
        for line in tree.lines() {
            let crate_name = line.split_whitespace().last().unwrap_or(line);
            let crate_name = crate_name.split(' ').next().unwrap_or(crate_name);
            if crate_name == pattern || crate_name.starts_with(&format!("{pattern}-")) {
                found.push(format!("{} ({})", pattern, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the engine must stay free of a user interface, found:\n{}",
        found.join("\n")
    );
}
