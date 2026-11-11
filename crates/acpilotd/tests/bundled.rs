//! What the daemon carries of the helper.
//!
//! The answer depends on how the daemon was built, so the test states both laws rather
//! than assuming one: a build given `ACPILOT_HELPER_BIN` carries those exact bytes, and a
//! build given nothing carries nothing. A daemon that "carried" a zero-byte program would
//! upload it and then fail every call on the host, which is why the empty case is `None`
//! and not an empty slice somebody could write out.

#[test]
fn the_daemon_carries_exactly_the_helper_it_was_built_with() {
    match std::env::var_os("ACPILOT_HELPER_BIN") {
        None => assert!(
            acpilotd::helper::bundled().is_none(),
            "a build without an artifact must not claim to carry one"
        ),
        Some(path) => {
            let expected = std::fs::read(&path).expect("the artifact the build was given");
            let carried = acpilotd::helper::bundled().expect("the build was given a helper");
            assert_eq!(
                carried,
                expected.as_slice(),
                "byte for byte, not a copy of a copy"
            );
        }
    }
}
