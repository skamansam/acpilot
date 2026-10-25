//! Serves the compiled web UI from the daemon binary.
//!
//! Today that is one page: the shell a browser lands on, which says which build is
//! answering and can open About or close itself. The web phase (P11) replaces the
//! body of `assets/index.html` with the built SPA (Svelte + CodeMirror) and keeps
//! this shape: the daemon embeds its own frontend, so "install the web UI" is "run
//! the daemon" and there is no asset directory to lose.
//!
//! Embedded rather than served from disk because a daemon started from `cargo run`
//! and one started from a release tarball must serve the same page, and the tarball
//! has no `web/` directory to serve from.

/// The page, with `{{version}}` still in it.
pub const PAGE: &str = include_str!("../assets/index.html");

/// The page with the version filled in.
///
/// Takes the version rather than reading a constant here: this crate has no opinion
/// about which build it is in, and the daemon already knows (`acpilot_core::VERSION`,
/// which is the release tag when one exists). Two sources of truth for "what
/// version is this" is how a bug report starts with "it says 0.1.0 but I installed
/// 0.4.2".
#[must_use]
pub fn page(version: &str) -> String {
    PAGE.replace("{{version}}", version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_names_the_version_it_was_served_with() {
        let html = page("9.9.9");
        assert!(html.contains("9.9.9"), "the version is on the page");
        assert!(
            !html.contains("{{version}}"),
            "no placeholder survives: a literal `{{version}}` in a browser is a bug report"
        );
    }

    #[test]
    fn the_page_has_an_about_dialog_and_a_quit_button() {
        let html = page("0.0.1");
        assert!(html.contains("id=\"about\""));
        assert!(html.contains("id=\"quit-button\""));
        assert!(html.contains("showModal"), "About opens as a modal");
        assert!(html.contains("window.close"), "Quit tries to close the tab");
        assert!(
            html.contains("MIT OR Apache-2.0"),
            "the licence is stated where a person looks for it"
        );
    }

    #[test]
    fn the_page_needs_no_network_to_render() {
        // A daemon on a machine with no route out still has to serve a page that
        // works: every style and script is inline, and only the two credits link
        // out. A CDN reference here would be a blank page behind a firewall.
        let html = page("0.0.1");
        assert!(!html.contains("<script src"), "{html}");
        assert!(!html.contains("rel=\"stylesheet\""), "{html}");
        assert_eq!(html.matches("https://").count(), 2, "{html}");
    }
}
