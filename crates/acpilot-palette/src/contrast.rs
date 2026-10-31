//! WCAG contrast, measured over every pair a theme can put on screen.
//!
//! The rule exists because of a real bug in this project's own manual: the
//! sidebar's selected item painted near-black text on a dark panel and was
//! unreadable until somebody measured it. "It looked fine on my monitor" is how
//! a theme ships broken, so the build measures instead.
//!
//! Every foreground token is checked against **both** surfaces, not just the one
//! it was designed for: a status colour that only works on `bg` will eventually
//! be painted on a panel. That is 7 UI tokens and 10 syntax tokens against two
//! surfaces, so 34 measurements per theme.
//!
//! `bg`, `bg_panel`, and `border` are deliberately excluded from that count. The
//! first two are the paper, not the ink; comparing a surface with itself always
//! reports 1:1. WCAG 1.4.11 covers contrast for parts of a component needed to
//! identify it, and a hairline separator carries no information — forcing 3:1
//! hairlines would make every theme look like a wireframe.
//!
//! The paper still needs a rule, because "which of these two is the panel" is not
//! something text contrast can answer, and a panel that is 1.04:1 from the
//! background is a panel nobody can see. That is the [`PANEL_MIN`]–[`PANEL_MAX`]
//! window, measured between `bg` and `bg_panel` and reported separately.

use std::fmt;

use crate::theme::{ResolvedTheme, SyntaxToken, UiToken};

/// The closest a panel may sit to the background it is lifted off.
///
/// A panel is paper, not ink, so the text rule cannot see it: text on a panel can
/// pass while the panel itself is indistinguishable from the screen behind it.
/// Night flight's original `#0c1424` on `#070d18` measured 1.06:1 — a difference
/// you can find with a colour picker and not with your eyes, which is why the
/// sidebar needed a border to say where it ended.
///
/// Below this floor a panel reads as the background; above [`PANEL_MAX`] it reads
/// as a slab. The upper bound is not taste either: the AA rule measures every token
/// against the panel, so a panel that has travelled further from the background has
/// spent contrast the text drawn on it needs. Between 1.10 and 1.35 there is room
/// for a layer to be visible and for the ink above it to stay readable, which is
/// the same window ratcn calls its well ladder.
pub const PANEL_MIN: f64 = 1.10;

/// The furthest a panel may sit from the background. See [`PANEL_MIN`].
pub const PANEL_MAX: f64 = 1.35;

/// The measured distance between the two surfaces, when a theme sets both.
#[must_use]
pub fn panel_ratio(theme: &ResolvedTheme) -> Option<f64> {
    let background = theme.palette.ui(UiToken::Bg)?;
    let panel = theme.palette.ui(UiToken::BgPanel)?;
    Some(background.contrast_ratio(panel))
}

/// A panel that is outside the window, and by how much.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelViolation {
    /// The measured ratio, 1.0–21.0.
    pub ratio: f64,
}

impl PanelViolation {
    /// The violation in a measured ratio, or [`None`] when it is inside the window.
    #[must_use]
    pub fn of(ratio: f64) -> Option<Self> {
        if (PANEL_MIN..=PANEL_MAX).contains(&ratio) {
            None
        } else {
            Some(Self { ratio })
        }
    }

    /// Which end of the window was missed, in the words a theme author needs.
    #[must_use]
    pub const fn edge(&self) -> &'static str {
        if self.ratio < PANEL_MIN {
            "cannot be seen against the background"
        } else {
            "is far enough from the background to spend the contrast its text needs"
        }
    }
}

impl fmt::Display for PanelViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bg_panel is {:.2}:1 from bg, outside {PANEL_MIN:.2}–{PANEL_MAX:.2}:1: a panel that {}",
            self.ratio,
            self.edge()
        )
    }
}

/// Why a theme's panel breaks the window, or [`None`] when it does not.
#[must_use]
pub fn panel_violation(theme: &ResolvedTheme) -> Option<PanelViolation> {
    panel_ratio(theme).and_then(PanelViolation::of)
}

/// WCAG AA for normal-size text.
pub const AA_NORMAL_TEXT: f64 = 4.5;

/// A surface a token can be painted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Surface {
    /// The main background.
    Bg,
    /// Panels, cards, sidebars.
    BgPanel,
}

impl Surface {
    /// Both surfaces, in report order.
    pub const ALL: [Self; 2] = [Self::Bg, Self::BgPanel];

    /// The token whose colour is the surface.
    #[must_use]
    pub const fn token(self) -> UiToken {
        match self {
            Self::Bg => UiToken::Bg,
            Self::BgPanel => UiToken::BgPanel,
        }
    }

    /// The token name, for reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.token().as_str()
    }
}

/// The colour being tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Foreground {
    /// A UI token such as `text`.
    Ui(UiToken),
    /// A syntax token such as `keyword`.
    Syntax(SyntaxToken),
}

impl Foreground {
    /// The token name, for reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ui(token) => token.as_str(),
            Self::Syntax(token) => token.as_str(),
        }
    }
}

/// One measured pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pair {
    /// The ink.
    pub foreground: Foreground,
    /// The paper.
    pub surface: Surface,
    /// Measured ratio, 1.0–21.0.
    pub ratio: f64,
}

/// The result of auditing one theme.
#[derive(Debug, Clone, PartialEq)]
pub struct Audit {
    /// Every pair, in a stable order.
    pub pairs: Vec<Pair>,
    /// `bg_panel` measured against `bg`, when the theme sets both.
    pub panel: Option<f64>,
}

impl Audit {
    /// The worst ratio in the theme.
    #[must_use]
    pub fn minimum(&self) -> Option<f64> {
        self.pairs.iter().map(|pair| pair.ratio).reduce(f64::min)
    }

    /// The panel's distance from the background, when the theme sets both.
    #[must_use]
    pub fn panel(&self) -> Option<f64> {
        self.panel
    }

    /// Whether the panel, if the theme has one, sits inside the window.
    #[must_use]
    pub fn panel_is_inside_window(&self) -> bool {
        self.panel.and_then(PanelViolation::of).is_none()
    }

    /// The panel violation, when there is one.
    #[must_use]
    pub fn panel_violation(&self) -> Option<PanelViolation> {
        self.panel.and_then(PanelViolation::of)
    }

    /// Pairs below the threshold.
    #[must_use]
    pub fn failures(&self, threshold: f64) -> Vec<&Pair> {
        self.pairs
            .iter()
            .filter(|pair| pair.ratio < threshold)
            .collect()
    }

    /// Whether every pair clears the threshold.
    #[must_use]
    pub fn passes(&self, threshold: f64) -> bool {
        self.failures(threshold).is_empty()
    }

    /// A report a failing test can print.
    ///
    /// Names the offending pair, both tokens, and the ratio needed, because a
    /// failure that says only "contrast" sends someone hunting through tokens.
    #[must_use]
    pub fn report(&self, theme: &str, threshold: f64) -> String {
        let failures = self.failures(threshold);
        let mut out = String::new();
        if failures.is_empty() {
            out.push_str(&format!(
                "{theme}: {} pairs, lowest {:.2}:1 (AA needs {threshold:.2}:1)\n",
                self.pairs.len(),
                self.minimum().unwrap_or(0.0)
            ));
        } else {
            out.push_str(&format!(
                "{theme}: {} of {} pairs below {threshold:.2}:1\n",
                failures.len(),
                self.pairs.len()
            ));
            for pair in failures {
                out.push_str(&format!(
                    "  {} on {}: {:.2}:1\n",
                    pair.foreground.as_str(),
                    pair.surface.as_str(),
                    pair.ratio
                ));
            }
        }
        // The panel is the one measurement the text rule cannot make, so it is
        // printed even when it passes: a ratio that drifts towards an edge is the
        // next theme's regression if nobody sees the number.
        if let Some(ratio) = self.panel {
            match self.panel_violation() {
                Some(violation) => out.push_str(&format!("  {violation}\n")),
                None => out.push_str(&format!(
                    "  bg_panel {ratio:.2}:1 from bg (window {PANEL_MIN:.2}–{PANEL_MAX:.2}:1)\n"
                )),
            }
        }
        out
    }
}

/// Measure every foreground token against both surfaces.
///
/// Missing tokens are skipped rather than defaulted: an incomplete theme is a
/// loading error, and inventing a colour here would hide it.
#[must_use]
pub fn audit(theme: &ResolvedTheme) -> Audit {
    // 10 UI tokens minus the two surfaces and the border, plus 10 syntax tokens.
    let mut pairs = Vec::with_capacity(
        (UiToken::ALL.len() - 3) * Surface::ALL.len() + SyntaxToken::ALL.len() * Surface::ALL.len(),
    );

    for surface in Surface::ALL {
        let Some(paper) = theme.palette.ui(surface.token()) else {
            continue;
        };
        for token in UiToken::ALL {
            if matches!(token, UiToken::Bg | UiToken::BgPanel | UiToken::Border) {
                continue;
            }
            if let Some(ink) = theme.palette.ui(token) {
                pairs.push(Pair {
                    foreground: Foreground::Ui(token),
                    surface,
                    ratio: ink.contrast_ratio(paper),
                });
            }
        }
        for token in SyntaxToken::ALL {
            if let Some(ink) = theme.palette.syntax(token) {
                pairs.push(Pair {
                    foreground: Foreground::Syntax(token),
                    surface,
                    ratio: ink.contrast_ratio(paper),
                });
            }
        }
    }

    Audit {
        pairs,
        panel: panel_ratio(theme),
    }
}
