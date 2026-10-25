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
//! `bg`, `bg_panel`, and `border` are deliberately excluded. The first two are the
//! paper, not the ink; comparing a surface with itself always reports 1:1. WCAG
//! 1.4.11 covers contrast for parts of a component needed to identify it, and a
//! hairline separator carries no information — forcing 3:1 hairlines would make
//! every theme look like a wireframe.

use crate::theme::{ResolvedTheme, SyntaxToken, UiToken};

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
}

impl Audit {
    /// The worst ratio in the theme.
    #[must_use]
    pub fn minimum(&self) -> Option<f64> {
        self.pairs.iter().map(|pair| pair.ratio).reduce(f64::min)
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
            return out;
        }
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

    Audit { pairs }
}
