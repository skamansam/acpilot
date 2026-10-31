//! The built-ins are the first real test of the schema, so they get the strict
//! assertions: exact counts, a passing contrast audit, and docs data that has
//! not drifted from the files.

use std::fs;
use std::path::Path;

use acpilot_extensions::contrast::{self, AA_NORMAL_TEXT, Surface};
use acpilot_extensions::docs;
use acpilot_extensions::{
    Color, Palette, Registry, ResolvedTheme, SyntaxToken, ThemeAppearance, UiToken,
};

#[test]
fn the_built_ins_load_through_the_public_loader() {
    let registry = Registry::built_in().expect("the embedded extensions load");

    assert_eq!(registry.extensions().len(), 4, "four built-in extensions");
    assert_eq!(registry.themes().len(), 2, "night-flight and paper");
    assert_eq!(
        registry.language_servers().len(),
        10,
        "ten language-server presets"
    );
    assert_eq!(registry.agents().len(), 4, "four agent presets");

    for extension in registry.extensions() {
        assert!(
            extension.built_in,
            "{} should be marked built_in",
            extension.manifest.id
        );
        assert!(
            extension.warnings.is_empty(),
            "built-ins should not defer anything: {:?}",
            extension.warnings
        );
    }

    for preset in registry.language_servers() {
        assert_eq!(
            preset.extension_id, "acpilot.language-servers",
            "{} lost its provenance",
            preset.id
        );
        assert!(
            !preset.languages.is_empty(),
            "{} has no languages",
            preset.id
        );
        assert!(!preset.command.is_empty(), "{} has no command", preset.id);
    }
    for preset in registry.agents() {
        assert_eq!(preset.extension_id, "acpilot.agents");
        assert!(preset.command().is_some(), "{} has no command", preset.id());
    }
}

#[test]
fn built_in_themes_have_the_documented_identities() {
    let registry = Registry::built_in().expect("built-ins load");

    let dark = registry
        .theme("acpilot.night-flight")
        .expect("night flight is built in");
    assert_eq!(dark.appearance, ThemeAppearance::Dark);
    let light = registry.theme("acpilot.paper").expect("paper is built in");
    assert_eq!(light.appearance, ThemeAppearance::Light);

    assert_eq!(
        registry
            .default_theme(ThemeAppearance::Dark)
            .map(|theme| theme.id.as_str()),
        Some("acpilot.night-flight")
    );
    assert_eq!(
        registry
            .default_theme(ThemeAppearance::Light)
            .map(|theme| theme.id.as_str()),
        Some("acpilot.paper")
    );
}

#[test]
fn built_in_themes_meet_wcag_aa() {
    let registry = Registry::built_in().expect("built-ins load");
    let mut report = String::new();
    let mut failed = false;

    for theme in registry.themes() {
        let audit = contrast::audit(theme);
        assert_eq!(
            audit.pairs.len(),
            34,
            "{} should audit 17 foreground tokens against 2 surfaces",
            theme.id
        );
        report.push_str(&audit.report(&theme.id, AA_NORMAL_TEXT));
        failed |= !audit.passes(AA_NORMAL_TEXT);
    }

    assert!(!failed, "a theme is below WCAG AA:\n{report}");
    // Printed with `--nocapture`, so a change in the weakest pair is visible in
    // the CI log without opening the build artifacts.
    println!("{report}");
}

#[test]
fn the_audit_catches_an_unreadable_theme() {
    // A gate that cannot fail is decoration. This theme is deliberately
    // near-black on near-black and must be refused.
    let mut palette = Palette::default();
    for token in UiToken::ALL {
        palette.ui.insert(token, Color::from_rgb(0x14, 0x14, 0x14));
    }
    for token in SyntaxToken::ALL {
        palette
            .syntax
            .insert(token, Color::from_rgb(0x16, 0x16, 0x16));
    }
    let theme = ResolvedTheme {
        id: "test.unreadable".to_owned(),
        name: "Unreadable".to_owned(),
        appearance: ThemeAppearance::Dark,
        built_in: false,
        extension_id: "test.unreadable".to_owned(),
        palette,
    };

    let audit = contrast::audit(&theme);
    assert!(!audit.passes(AA_NORMAL_TEXT));
    let failures = audit.failures(AA_NORMAL_TEXT);
    assert!(!failures.is_empty());
    assert!(
        failures
            .iter()
            .all(|pair| pair.surface == Surface::Bg || pair.surface == Surface::BgPanel)
    );
    let report = audit.report("test.unreadable", AA_NORMAL_TEXT);
    assert!(report.contains("below 4.50:1"), "{report}");
    assert!(report.contains("on bg"), "{report}");
}

#[test]
fn built_in_panels_sit_inside_the_window() {
    let registry = Registry::built_in().expect("built-ins load");

    for theme in registry.themes() {
        let ratio = contrast::panel_ratio(theme).expect("both surfaces are set");
        assert!(
            (contrast::PANEL_MIN..=contrast::PANEL_MAX).contains(&ratio),
            "{}: bg_panel is {ratio:.2}:1 from bg, outside {:.2}–{:.2}:1",
            theme.id,
            contrast::PANEL_MIN,
            contrast::PANEL_MAX
        );
        assert!(
            contrast::panel_violation(theme).is_none(),
            "{} reports a violation it should not have",
            theme.id
        );
    }
}

#[test]
fn a_panel_outside_the_window_is_reported_rather_than_refused() {
    // The rule is a warning at load, not a parse error: a theme whose panel is a
    // shade too close still draws, so it loads and says what is wrong with it.
    let violation = contrast::PanelViolation::of(1.04).expect("below the floor");
    assert!(
        violation.to_string().contains("outside 1.10–1.35:1"),
        "{violation}"
    );
    assert!(
        violation.to_string().contains("cannot be seen"),
        "{violation}"
    );
    assert!(contrast::PanelViolation::of(1.18).is_none());
    assert!(
        contrast::PanelViolation::of(1.9)
            .expect("above the ceiling")
            .to_string()
            .contains("spend the contrast")
    );
}

#[test]
fn a_built_in_theme_loads_with_nothing_to_report() {
    let extensions = acpilot_extensions::builtin::load_all().expect("built-ins load");
    let registry = acpilot_extensions::ThemeRegistry::new(&extensions);

    let (theme, warnings) = registry
        .complete_with_warnings("acpilot.night-flight")
        .expect("night flight resolves");
    assert_eq!(theme.id, "acpilot.night-flight");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn the_gallery_shows_a_followed_palette_for_each_end_of_the_ramp() {
    // The gallery's second list is a claim about `theme_follow_terminal`: what a
    // black terminal and a cream one actually get. It is generated by the solver,
    // so the claim is checked here — the published ratios are the gate's, and the
    // two cards are one of each appearance rather than two dark palettes.
    let registry = Registry::built_in().expect("built-ins load");
    let document: serde_json::Value =
        serde_json::from_str(&docs::themes_json(&registry)).expect("generated JSON");
    let followed = document["followed"]
        .as_array()
        .expect("a followed list beside the built-ins");
    assert_eq!(followed.len(), 2, "a black terminal and a cream one");
    assert_eq!(followed[0]["appearance"], "dark", "black ink reads dark");
    assert_eq!(followed[1]["appearance"], "light", "cream reads light");

    for theme in followed {
        assert_eq!(theme["id"], acpilot_extensions::follow::ID);
        assert!(
            theme["terminal"]["label"].is_string(),
            "a card names the terminal it was solved for"
        );

        let pairs = theme["pairs"].as_array().expect("pairs");
        assert_eq!(pairs.len(), 34, "the same 17 by 2 the gate measures");
        for pair in pairs {
            let ratio = pair["ratio"].as_f64().expect("a ratio");
            assert!(
                ratio >= AA_NORMAL_TEXT,
                "{} on {} is {ratio:.2}:1, below the AA floor the page claims",
                pair["foreground"],
                pair["surface"]
            );
        }

        let panel = theme["panel"].as_f64().expect("a panel ratio");
        assert!(
            (contrast::PANEL_MIN..=contrast::PANEL_MAX).contains(&panel),
            "a solved panel is outside the window: {panel:.2}:1"
        );
    }
}

#[test]
fn docs_theme_data_is_current() {
    let registry = Registry::built_in().expect("built-ins load");
    let generated: serde_json::Value =
        serde_json::from_str(&docs::themes_json(&registry)).expect("generated JSON");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/src/data/themes.json");
    let committed = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}; run `cargo xtask docs-data`", path.display()));
    let committed: serde_json::Value = serde_json::from_str(&committed).expect("valid JSON");

    // Compare the document, not its key order: `serde_json`'s `preserve_order`
    // feature is decided by feature unification, so byte comparison makes this
    // test pass or fail depending on which packages are built alongside it.
    assert_eq!(
        committed, generated,
        "site/src/data/themes.json is stale; run `cargo xtask docs-data`"
    );
    assert_eq!(committed["aa_threshold"], serde_json::json!(AA_NORMAL_TEXT));
}
