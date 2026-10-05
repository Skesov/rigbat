//! The deb and rpm metadata in `Cargo.toml` must apply the udev rule on install.

const MANIFEST: &str = include_str!("../Cargo.toml");
const POSTINST: &str = include_str!("../packaging/scripts/postinst");
const POSTRM: &str = include_str!("../packaging/scripts/postrm");

#[test]
fn packages_run_the_udev_scripts() {
    for line in [
        r#"maintainer-scripts = "packaging/scripts/""#,
        r#"post_install_script = "packaging/scripts/postinst""#,
        r#"post_uninstall_script = "packaging/scripts/postrm""#,
    ] {
        assert!(
            MANIFEST.lines().any(|l| l == line),
            "Cargo.toml lacks {line}"
        );
    }
}

#[test]
fn install_reloads_the_rules_and_retriggers_hidraw() {
    assert!(POSTINST.contains("udevadm control --reload-rules"));
    assert!(POSTINST.contains("udevadm trigger --subsystem-match=hidraw --action=change"));
    assert!(POSTRM.contains("udevadm control --reload-rules"));
}
