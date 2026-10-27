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

use std::collections::BTreeMap;

use acpilot_extensions::theme::{AppliedTheme, UiToken};

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

/// The page, themed with a palette the surfaces share.
///
/// The browser is the third surface reading the same theme extension, so the
/// variables its stylesheet is written against come from the token file rather
/// than from the colours someone typed into the page.
///
/// Every token is a variable, and the name is the token's (`bg_panel` is
/// `--bg-panel`) rather than a mapping kept here. That is the point: a token added
/// to [`UiToken::ALL`] is a colour the page can use with no edit to this crate, so
/// the browser cannot be the surface that has not heard of it.
#[must_use]
pub fn themed_page(version: &str, theme: &AppliedTheme) -> String {
    page(version).replace(DEFAULT_VARS, &stylesheet(theme))
}

/// The palette as the variables the page's stylesheet is written against.
///
/// The same map the served page is built from, as data, so a page that is already
/// running can be repainted without being fetched again.
#[must_use]
pub fn variables(theme: &AppliedTheme) -> BTreeMap<String, String> {
    UiToken::ALL
        .iter()
        .map(|token| (css_name(*token), theme.color(*token).to_string()))
        .collect()
}

/// The palette as the JSON object a live page repaints from.
///
/// `{"color_scheme": "dark", "vars": {"--bg": "#070d18", …}}` — the variables by
/// name plus the one declaration a stylesheet cannot express as a variable, so the
/// browser's own scrollbars and form controls follow the theme too.
#[must_use]
pub fn palette_json(theme: &AppliedTheme) -> String {
    serde_json::json!({
        "color_scheme": color_scheme(theme),
        "vars": variables(theme),
    })
    .to_string()
}

/// The page's `:root` block for a palette.
///
/// One declaration of each variable and one `color-scheme`: a second declaration of
/// anything would leave the browser arbitrating between two of the page's own
/// values.
fn stylesheet(theme: &AppliedTheme) -> String {
    let mut css = format!(":root{{color-scheme:{};", color_scheme(theme));
    // In token order, which is the order `app.css` writes them in, so the block
    // reads like the stylesheet it replaces.
    for token in UiToken::ALL {
        css.push_str(&format!("{}: {};", css_name(token), theme.color(token)));
    }
    css.pop();
    css.push('}');
    css
}

/// The CSS custom property a token becomes: `bg_panel` is `--bg-panel`.
///
/// Derived, not listed. A hand-kept table here would be the second place a token is
/// handled — one table per surface, one of them forgotten on the next token.
#[must_use]
pub fn css_name(token: UiToken) -> String {
    format!("--{}", token.as_str().replace('_', "-"))
}

/// Which half of the light/dark divide the palette belongs to.
///
/// [`AppliedTheme::is_dark`]'s answer, which is read from the background's
/// luminance rather than from the theme file's own `appearance`: the two can
/// disagree, and the page has to match the colour it is actually painted with.
fn color_scheme(theme: &AppliedTheme) -> &'static str {
    if theme.is_dark() { "dark" } else { "light" }
}

/// The page's own `:root` block, which a theme replaces.
///
/// Looked up rather than copied: the file is the truth, and this constant is how
/// the replacement finds it. It is the same block `web/src/app.css` declares, and
/// the test below fails if the two drift — a page whose `:root` kept yesterday's
/// colours would be the palette a reload showed.
const DEFAULT_VARS: &str = ":root{color-scheme:dark;--bg: #0b1220;--bg-panel: #131c2e;--text: #e6ecf7;--text-muted: #93a4c0;--accent: #5aa9ff;--accent-high: #8fc4ff;--border: #223049;--success: #7ee0a3;--warning: #f7c65c;--danger: #ff8a80}";

#[cfg(test)]
mod tests {
    use super::*;

    fn night_flight() -> AppliedTheme {
        let extensions = acpilot_extensions::builtin::load_all().expect("the built-ins load");
        AppliedTheme::from_extensions(&extensions, "acpilot.night-flight")
            .expect("night flight resolves")
    }

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

    #[test]
    fn the_served_page_still_has_the_block_a_theme_replaces() {
        // `themed_page` finds the page's palette by this literal. If `app.css`
        // changes and this constant does not, the replacement silently does
        // nothing and every themed page is the page's own colours.
        assert!(
            PAGE.contains(DEFAULT_VARS),
            "the built page's `:root` is not DEFAULT_VARS; copy the block from app.css"
        );
    }

    #[test]
    fn every_token_the_page_has_is_a_variable_it_declares() {
        // The derivation and the stylesheet are the same list: a token the page
        // never declares is a colour a theme sets to no effect, and a variable the
        // page declares with no token is a colour that can never be themed.
        for token in UiToken::ALL {
            let name = css_name(token);
            assert!(
                DEFAULT_VARS.contains(&format!("{name}:")),
                "`{name}` is a token but the page declares no such variable"
            );
        }
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
    fn the_live_payload_is_the_same_variables_the_page_was_built_from() {
        let theme = night_flight();
        let payload = palette_json(&theme);
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");

        assert_eq!(parsed["color_scheme"], "dark");
        assert_eq!(parsed["vars"]["--bg"], "#070d18");
        assert_eq!(parsed["vars"]["--accent"], "#f7c65c");
        assert_eq!(
            parsed["vars"].as_object().expect("an object").len(),
            UiToken::ALL.len(),
            "every token, so a repaint leaves no colour from the old theme"
        );
        // The served page and the stream must agree: a live page that repainted
        // to something the initial HTML did not say would be a flash of a third
        // palette on every reload.
        let html = themed_page("0.0.1", &theme);
        for (name, value) in parsed["vars"].as_object().expect("an object") {
            let value = value.as_str().expect("a colour");
            assert!(
                html.contains(&format!("{name}: {value}")),
                "the page says `{name}: {value}` too"
            );
        }
    }

    #[test]
    fn the_light_palette_says_so_in_the_live_payload() {
        let extensions = acpilot_extensions::builtin::load_all().expect("the built-ins load");
        let theme =
            AppliedTheme::from_extensions(&extensions, "acpilot.paper").expect("paper resolves");

        let payload = palette_json(&theme);
        assert!(payload.contains("\"color_scheme\":\"light\""), "{payload}");
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
