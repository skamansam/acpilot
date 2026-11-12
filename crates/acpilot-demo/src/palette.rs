//! Every command the interface offers, and what would build it.
//!
//! The palette is the design's index: it is where a feature is named, keyed and
//! answered, so a command that does nothing yet is not a hole in the interface —
//! it is a row that says which task fills it. That is the whole point of the
//! exercise: the screen lists the work.

/// Something the interface can do today, as the frontend's own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// Open a new session with the first available agent.
    NewSession,
    /// Choose which declared agent the assistant runs.
    SelectAgent,
    /// Open, or leave, the session's terminal pane.
    Terminal,
    /// Read the open terminal backwards and copy from it.
    CopyMode,
    /// Re-read sessions, agents and pending permissions.
    Refresh,
    /// Show the keys the focused pane answers to.
    WhichKey,
    /// Open this list.
    Palette,
    /// Show the permission card for whatever is waiting.
    Permission,
    /// Open, or leave, the review of the agent's diff.
    ReviewDiff,
    /// Answer the pending write with the diff as it stands.
    AcceptDiff,
    /// Decline the pending write.
    RejectDiff,
    /// Search the open buffer, from the editor it belongs to.
    Find,
    /// Find a file in the workspace, from the tree that opens one.
    FindFile,
    /// Find a line in the workspace's files, in a jump list.
    SearchContents,
    /// Go to a line and column of the open buffer.
    GoToLine,
    /// Report what the daemon's language servers are doing.
    LspStatus,
    /// Leave.
    Quit,
}

/// What activating a command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    /// It works now.
    Run(Run),
    /// It is designed, and named after the task that would build it.
    Planned(&'static str),
}

/// One row of the palette.
#[derive(Debug, Clone, Copy)]
pub struct Command {
    /// What it does, in the words the row shows.
    pub title: &'static str,
    /// The binding, as the row advertises it. Empty when it is only here.
    pub keys: &'static str,
    /// What activating it does.
    pub action: CommandAction,
}

impl Command {
    /// The task that would build this command, when it is not built.
    #[must_use]
    pub fn planned(&self) -> Option<&'static str> {
        match self.action {
            CommandAction::Run(_) => None,
            CommandAction::Planned(task) => Some(task),
        }
    }
}

const fn run(title: &'static str, keys: &'static str, run: Run) -> Command {
    Command {
        title,
        keys,
        action: CommandAction::Run(run),
    }
}

const fn planned(title: &'static str, keys: &'static str, task: &'static str) -> Command {
    Command {
        title,
        keys,
        action: CommandAction::Planned(task),
    }
}

/// The catalogue, in the order the palette lists it: the working commands first,
/// because a list that opens on what does not work is a list that reads as a
/// wiring diagram rather than as a tool.
#[must_use]
pub fn commands() -> Vec<Command> {
    vec![
        run("New session", "n", Run::NewSession),
        run("Select agent…", "", Run::SelectAgent),
        run("Terminal pane", "ctrl-t", Run::Terminal),
        run("Copy from the terminal", "f2", Run::CopyMode),
        run("Refresh", "ctrl-r", Run::Refresh),
        run("Show keys", "?", Run::WhichKey),
        run("Command palette", "ctrl-p", Run::Palette),
        run("Answer a permission", "y", Run::Permission),
        // The editor's two boxes are the editor's: the row moves the keyboard
        // there and opens the one it names, so the palette cannot drift from the
        // keys that do the same thing.
        run("Find in file", "ctrl-f", Run::Find),
        run("Go to line…", "ctrl-g", Run::GoToLine),
        // The diff's three rows answer the same question the card asks, so they
        // moved out of the planned list when P6-9 landed rather than staying a
        // second way to reach a decision that already exists. They sit last of the
        // working rows because the list is read top down and the first rows are
        // the ones a person reaches for without thinking.
        run("Review diff", "ctrl-d", Run::ReviewDiff),
        run("Accept this diff", "ctrl-enter", Run::AcceptDiff),
        run("Reject this diff", "ctrl-backspace", Run::RejectDiff),
        run("Quit", "ctrl-q", Run::Quit),
        // `Find a file…` was `planned("Open file…", "P6-6")` until the tree could
        // open one. It is the tree's own find now, because a file is opened by
        // picking it out of a list rather than by typing a name into nothing.
        run("Find a file…", "ctrl-o", Run::FindFile),
        // The other half of a find: contents are a walk that reads the files rather
        // than a listing of their names, and the answer is a jump list.
        run("Search file contents", "ctrl-shift-f", Run::SearchContents),
        // The daemon owns the servers, so the report is a request and not a
        // setting: what is running, how it counts columns, and how long its last
        // answer took. The row is here rather than behind a `:` line because this
        // frontend has no command line yet — the palette's query is the one field.
        run("LSP status", "", Run::LspStatus),
        planned("Go to definition", "f12", "P7-4"),
        planned("Rename symbol", "f2", "P7-5"),
        planned("Split editor", "ctrl-\\", "P6-3"),
        planned("Switch profile…", "ctrl-k p", "P4-6"),
        planned("Model lab", "ctrl-k m", "P4.5"),
        planned("Contribute a theme…", "ctrl-k t", "P4.6"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_shows_what_works_and_what_would_build_the_rest() {
        let commands = commands();
        let working = commands
            .iter()
            .filter(|command| command.planned().is_none())
            .count();
        assert!(working >= 5, "the interface does something today");
        assert!(
            commands.iter().all(|command| !command.title.is_empty()),
            "a row with no title is a row nobody can read"
        );
        assert!(
            commands
                .iter()
                .filter_map(Command::planned)
                .all(|task| task.starts_with('P')),
            "every planned row names a phase task"
        );
        assert!(
            commands
                .iter()
                .any(|command| "ctrl-o" == command.keys && command.planned().is_none()),
            "finding a file is a command: the tree does it"
        );
    }
}
