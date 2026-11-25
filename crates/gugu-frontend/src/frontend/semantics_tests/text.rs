use super::accepts;

#[test]
fn unicode_version_exposes_major_minor_patch() {
    assert!(accepts(
        "use std.text.{unicode_version}\nfn main() { let version = unicode_version()\n let major: int = version.major\n let minor: int = version.minor\n let patch: int = version.patch\n _ = major\n _ = minor\n _ = patch }"
    ));
}
