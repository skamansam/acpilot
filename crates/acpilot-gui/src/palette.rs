//! The command palette's entries, and the matching that makes it worth opening.
//!
//! This is deliberately data plus one pure function. The shell turns the result
//! into rows; nothing here knows about gpui, so the matching is tested without a
//! window and can be reused by the TUI's palette when P6 gets one.

use crate::profiles::ShellProfile;

/// What an entry does when it is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    /// Make this profile the current one.
    SelectProfile(usize),
    /// Leave the app.
    Quit,
}

/// One palette entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// What choosing it does.
    pub id: CommandId,
    /// The line the user reads.
    pub label: String,
    /// The line under it, so two profiles with similar names are distinguishable.
    pub detail: String,
}

/// The entries for a window opened on these profiles.
#[must_use]
pub fn commands(profiles: &[ShellProfile]) -> Vec<Command> {
    let mut commands: Vec<Command> = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| Command {
            id: CommandId::SelectProfile(index),
            label: format!("Switch to {}", profile.name()),
            detail: format!("{} · {}", profile.root().display(), profile.summary()),
        })
        .collect();
    commands.push(Command {
        id: CommandId::Quit,
        label: "Quit Acpilot".to_owned(),
        detail: "Close the window".to_owned(),
    });
    commands
}

/// The indices of the commands a query matches, best effort but predictable.
///
/// Every whitespace-separated term must appear somewhere in the label or the
/// detail, case-insensitively. That is deliberately dumber than a fuzzy score:
/// with five or six entries, a ranking nobody can explain is worse than a filter
/// that either shows an entry or does not.
#[must_use]
pub fn filter(commands: &[Command], query: &str) -> Vec<usize> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    if terms.is_empty() {
        return (0..commands.len()).collect();
    }
    commands
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            let haystack = format!("{} {}", command.label, command.detail).to_lowercase();
            terms.iter().all(|term| haystack.contains(term.as_str()))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Move a selection by `delta`, wrapping at both ends.
///
/// An empty list stays at 0 rather than panicking on a modulo of zero, which is
/// the state a query with no matches puts the palette in.
#[must_use]
pub fn move_selection(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    let moved = (current as isize + delta) % len;
    moved.rem_euclid(len) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real profile directory, because `ShellProfile` only exists for one that
    /// opens: the window never invents a profile, so neither does the test.
    fn profile(name: &str, rules: usize, agents: bool) -> (tempfile::TempDir, ShellProfile) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join(name);
        std::fs::create_dir(&root).expect("profile directory");
        let mut config = String::from("schema_version = 1\n\n[policy]\n");
        for index in 0..rules {
            config.push_str(&format!(
                "\n[[policy.rules]]\nid = \"rule-{index}\"\ndecision = \"allow\"\ntool = \"execute\"\n"
            ));
        }
        std::fs::write(root.join("config.toml"), config).expect("write config");
        if agents {
            std::fs::write(root.join("agents.toml"), "schema_version = 1\n").expect("write agents");
        }
        let profile = ShellProfile::open(&root).expect("the profile opens");
        (dir, profile)
    }

    /// The commands plus the directories that keep their profiles real.
    fn list() -> (Vec<tempfile::TempDir>, Vec<Command>) {
        let (first, work) = profile("work", 2, true);
        let (second, personal) = profile("personal", 0, false);
        (vec![first, second], commands(&[work, personal]))
    }

    #[test]
    fn there_is_one_entry_per_profile_and_one_to_leave() {
        let (_dirs, commands) = list();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].label, "Switch to work");
        assert_eq!(commands[0].id, CommandId::SelectProfile(0));
        assert!(commands[0].detail.contains("2 rules"), "{}", commands[0].detail);
        assert!(commands[0].detail.contains("own agents"));
        assert_eq!(commands[1].id, CommandId::SelectProfile(1));
        assert!(commands[1].detail.contains("shipped policy"));
        assert_eq!(commands[2].id, CommandId::Quit);
    }

    #[test]
    fn an_empty_query_shows_everything_in_order() {
        let (_dirs, commands) = list();
        assert_eq!(filter(&commands, ""), vec![0, 1, 2]);
        assert_eq!(filter(&commands, "   "), vec![0, 1, 2]);
    }

    #[test]
    fn a_query_narrows_by_name_case_insensitively() {
        let (_dirs, commands) = list();
        assert_eq!(filter(&commands, "work"), vec![0]);
        assert_eq!(filter(&commands, "WORK"), vec![0]);
        assert_eq!(filter(&commands, "quit"), vec![2]);
    }

    #[test]
    fn every_whitespace_term_has_to_match() {
        let (_dirs, commands) = list();
        assert_eq!(filter(&commands, "switch personal"), vec![1]);
        assert_eq!(filter(&commands, "switch nothing"), Vec::<usize>::new());
    }

    #[test]
    fn the_detail_is_searchable_too() {
        let (_dirs, commands) = list();
        // "shipped" is only in the detail of the profile with no rules.
        assert_eq!(filter(&commands, "shipped"), vec![1]);
    }

    #[test]
    fn a_query_that_matches_nothing_selects_nothing_rather_than_panicking() {
        let (_dirs, commands) = list();
        let matched = filter(&commands, "zzz");
        assert!(matched.is_empty());
        assert_eq!(move_selection(3, matched.len(), 1), 0);
        assert_eq!(move_selection(3, matched.len(), -1), 0);
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        assert_eq!(move_selection(0, 3, 1), 1);
        assert_eq!(move_selection(2, 3, 1), 0);
        assert_eq!(move_selection(0, 3, -1), 2);
        assert_eq!(move_selection(0, 1, 1), 0);
        assert_eq!(move_selection(1, 3, 3), 1);
    }

    #[test]
    fn the_quit_entry_is_last_so_it_never_shadows_a_profile() {
        let (_dirs, commands) = list();
        assert_eq!(
            commands.last().map(|command| command.id),
            Some(CommandId::Quit)
        );
    }

    #[test]
    fn the_detail_names_the_path_so_two_similar_profiles_are_distinguishable() {
        let (_dirs, commands) = list();
        assert!(
            commands[0].detail.contains("work"),
            "the detail says which directory: {}",
            commands[0].detail
        );
    }
}
