//! Serves the compiled web UI from the daemon binary.
//!
//! That is one page, built by `web/` (Svelte 5 + Vite) into
//! `assets/index.html`: a single file with its script and stylesheet inlined, so
//! the daemon embeds its own frontend and "install the web UI" is "run the
//! daemon". There is no asset directory to lose and no second file to route.
//!
//! Embedded rather than served from disk because a daemon started from `cargo run`
//! and one started from a release tarball must serve the same page, and the tarball
//! has no `web/` directory to serve from. `npm run build` in `web/` is what
//! regenerates the file; it is committed so building the daemon needs no Node.
//!
//! What the page does today: asks for the daemon's token, shows the health answer
//! and the session list once it has one, and offers About and Quit. The editor,
//! transcript and terminal are the rest of the web phase (P11-3 – P11-5).

use acpilot_extensions::theme::{Palette, UiToken};

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

/// The page, themed with a resolved palette.
///
/// The browser is the third surface reading the same theme extension, so the
/// variables its stylesheet is written against come from the token file rather
/// than from the colours someone typed into the page. The variables it does not
/// have — a warning colour, a brighter accent — are mapped onto the ones it does,
/// because a page that grew a second palette would be a fourth place a colour is
/// written down.
///
/// A palette with holes leaves the page's own colours alone: a browser with no
/// `--text` is a page nobody can read, and the daemon has no business shipping that.
#[must_use]
pub fn themed_page(version: &str, palette: &Palette) -> String {
    let Some(vars) = variables(palette) else {
        return page(version);
    };
    page(version).replace(DEFAULT_VARS, &vars)
}

/// The variables the stylesheet is written against, in the page's order.
///
/// Kept as a string rather than a struct because it is a stylesheet fragment: the
/// names are the contract with the CSS, and a test reads them back out.
fn variables(palette: &Palette) -> Option<String> {
    let token = |token: UiToken| palette.ui(token).map(|color| color.to_string());
    let bg = token(UiToken::Bg)?;
    let vars = format!(
        ":root{{color-scheme:{};--bg: {};--panel: {};--border: {};--text: {};--muted: {};\
         --accent: {};--on-accent: {};--danger: {};--ok: {}}}",
        if palette
            .ui(UiToken::Bg)
            .is_some_and(|bg| bg.relative_luminance() < 0.5)
        {
            "dark"
        } else {
            "light"
        },
        bg,
        token(UiToken::BgPanel)?,
        token(UiToken::Border)?,
        token(UiToken::Text)?,
        token(UiToken::TextMuted)?,
        token(UiToken::Accent)?,
        token(UiToken::Bg)?,
        token(UiToken::Danger)?,
        token(UiToken::Success)?,
    );
    Some(vars)
}

/// The page's own `:root` block, which a theme replaces.
///
/// Looked up rather than copied: the file is the truth, and this constant is how
/// the replacement finds it.
const DEFAULT_VARS: &str = ":root{color-scheme:dark;--bg: #0b1220;--panel: #131c2e;--border: #223049;--text: #e6ecf7;--muted: #93a4c0;--accent: #5aa9ff;--on-accent: #08101d;--danger: #ff8a80;--ok: #7ee0a3}";

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
        assert!(html.contains("id=\"about-title\""), "About has a heading");
        assert!(html.contains("showModal"), "About opens as a modal");
        assert!(html.contains("window.close"), "Quit tries to close the tab");
        assert!(
            html.contains("MIT OR Apache-2.0"),
            "the licence is stated where a person looks for it"
        );
    }

    #[test]
    fn the_page_asks_for_the_token_before_it_asks_the_daemon_anything() {
        // The shell is unauthenticated (see `acpilotd::auth::OPEN`), so the page
        // must not carry data — and the only way in is the token field.
        let html = page("0.0.1");
        assert!(html.contains("id=\"token\""), "{html}");
        assert!(html.contains("type=\"password\""), "{html}");
        assert!(
            html.contains("Authorization"),
            "the token travels in a header, never a query string"
        );
    }

    fn night_flight() -> acpilot_extensions::Palette {
        let extensions = acpilot_extensions::builtin::load_all().expect("the built-ins load");
        acpilot_extensions::ThemeRegistry::new(&extensions)
            .complete("acpilot.night-flight")
            .expect("night flight resolves")
            .palette
    }

    #[test]
    fn a_themed_page_carries_the_token_values_not_its_own_palette() {
        let html = themed_page("0.0.1", &night_flight());
        assert!(html.contains("--bg: #070d18"), "the theme's background");
        assert!(html.contains("--accent: #f7c65c"), "and its accent");
        assert!(
            !html.contains("--bg: #0b1220"),
            "the page's hardcoded palette is gone, not painted over"
        );
        assert_eq!(
            html.matches("color-scheme:").count(),
            1,
            "one declaration, so the browser has nothing to arbitrate"
        );
    }

    #[test]
    fn a_palette_with_a_hole_leaves_the_page_readable() {
        let mut palette = night_flight();
        palette.ui.remove(&acpilot_extensions::UiToken::Text);
        let html = themed_page("0.0.1", &palette);
        assert_eq!(
            html,
            page("0.0.1"),
            "the page's own colours are the fallback"
        );
    }

    #[test]
    fn the_page_needs_no_network_to_render() {
        // A daemon on a machine with no route out still has to serve a page that
        // works: every style and script is inline, and the only links out are the
        // two credits. A CDN reference here would be a blank page behind a
        // firewall. Svelte's production bundle mentions its error-message URLs as
        // strings; those are not requests, which is why this counts `href=`.
        let html = page("0.0.1");
        assert!(!html.contains("<script src"), "{html}");
        assert!(!html.contains("<link "), "no external stylesheet");
        assert_eq!(
            html.matches("href=\"https://").count(),
            2,
            "the manual and the credit, and nothing else"
        );
    }
}
