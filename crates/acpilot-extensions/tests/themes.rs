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
