//! The half of the TUI that talks to the daemon.
//!
//! The UI thread never awaits. A worker thread owns a small Tokio runtime, makes
//! the calls, and turns them into [`Update`]s; the UI turns key presses into
//! [`Request`]s. That split is what keeps a slow daemon from freezing the
//! drawing loop, and it is the same shape a GUI or web client would use, because
//! all of them are clients of the same API.
//!
//! Nothing here decides policy. A prompt goes to the daemon because the daemon
//! runs the turn; a permission answer goes to the daemon because the broker owns
//! the pending list and the audit row. The TUI is a view with a keyboard.

use std::time::Duration;

use acpilot_client::terminal::{Terminal as Attached, TerminalEvent};
use acpilot_client::{Client, ClientError, ProfileStream, SessionStream};
use acpilot_proto::agent::AgentSummary;
use acpilot_proto::event::SessionEvent;
use acpilot_proto::permission::{PermissionOption, PermissionRequest};
use acpilot_proto::session::{CreateSessionRequest, SessionSummary};
use acpilot_proto::stats::IntegrationStats;
use acpilot_proto::terminal::{TerminalSize, TerminalSummary};
use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How long the session list waits before asking again.
///
/// Status is not on the event stream — a turn ending changes a summary, not the
/// transcript — so a poll is how a finished turn stops looking busy. One second
/// is faster than a person reads the status line, and the request is tiny.
const POLL: Duration = Duration::from_secs(1);

/// How long to wait before reopening a stream that failed.
const RECONNECT: Duration = Duration::from_secs(2);

/// How long the stats bar waits before asking again.
///
/// Slower than the session poll on purpose: a context window and a balance move
/// over a turn, not over a keystroke, and a row that arrives already knowing how
/// old it is (`updated_at`) needs no faster clock to be honest.
const STATS_POLL: Duration = Duration::from_secs(15);

/// What the UI asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Re-read sessions, agents and pending permissions.
    Refresh {
        /// Whether the tree is showing the paths an ignore rule hides. The root is
        /// listed again on a refresh, and a refresh that quietly dropped those rows
        /// would leave the toggle claiming they are on screen.
        include_ignored: bool,
    },
    /// Open a session, replaying its transcript from the beginning.
    Select(String),
    /// Send a prompt to the open session.
    Prompt(String),
    /// Cancel the turn in flight.
    Cancel,
    /// Start a session with this agent and open it.
    New(String),
    /// End a session.
    Delete(String),
    /// List a directory of the workspace, by workspace-relative path.
    ///
    /// Empty is the root. The tree asks for a directory when it is opened rather
    /// than reading the whole workspace, because a walk of a large repository is a
    /// request whose cost is the repository's.
    ListDir {
        /// The directory, relative to the workspace root; empty is the root.
        path: String,
        /// Whether to return the rows an ignore rule hides, marked as hidden.
        include_ignored: bool,
    },
    /// Walk the workspace for paths containing a query, for the files pane's find.
    SearchFiles(String),
    /// Search the workspace's file contents for a query, for the jump list.
    Grep(String),
    /// Find the code uses of an identifier, for the references panel.
    ///
    /// The name *and* the caret: the name is what a parse-tree search needs, and the
    /// file and character offset are what a language server needs to resolve that name
    /// to a symbol. The pane sends both, and the daemon uses whichever it can.
    References {
        /// The identifier under the caret.
        name: String,
        /// The file the caret is in, workspace-relative.
        path: String,
        /// The caret's offset in that file, counted in characters.
        offset: usize,
        /// The buffer as the pane holds it, because an edit that has not been saved is
        /// still the text the list is about: a use typed a moment ago is missing from
        /// the file on disk, and a use deleted a moment ago is still in it.
        text: String,
    },
    /// What the daemon's language server says is wrong with a buffer.
    ///
    /// The text travels for the same reason it does with references: an edit that has
    /// not been saved is the file the person is reading, and problems about the saved
    /// version are problems about a line that has moved.
    Diagnostics {
        /// The file, workspace-relative.
        path: String,
        /// The buffer as the pane holds it.
        text: String,
    },
    /// Read what the daemon's language servers are doing.
    ///
    /// A request rather than a local answer: the servers are the daemon's processes,
    /// and a window that reported its own guess would be a second opinion about which
    /// servers exist.
    LspStatus,
    /// Read the project map the agent wrote under `.acpilot/`.
    ProjectMap,
    /// Read a file, by workspace-relative path.
    OpenFile(String),
    /// Write a buffer back, with the token the read came with.
    ///
    /// The text and the token travel together because they are one write: a save
    /// without the token would be a save that could overwrite a change nobody saw.
    SaveFile {
        /// The file, relative to the workspace root.
        path: String,
        /// The whole buffer, as it reads now.
        text: String,
        /// The `etag` the read answered with.
        etag: String,
    },
    /// Attach to one of the open session's terminals, by id.
    OpenTerminal {
        /// The terminal to attach to, as the daemon listed it.
        id: String,
    },
    /// Start a shell for the open session and attach to it.
    ///
    /// `workspace` is where the shell runs, and it is the session's: a remote
    /// workspace's terminal runs on that workspace's host through its helper, which
    /// is the machine the files are on. The daemon decides that from the id, so this
    /// window does not have to know which hosts are remote.
    NewTerminal {
        /// Workspace id from the session; absent for the daemon's own workspace.
        workspace: Option<String>,
        /// The size to start it at, before the pane's first resize.
        size: TerminalSize,
    },
    /// Bytes for the attached terminal, as if typed.
    TerminalWrite(Vec<u8>),
    /// The pane changed shape; tell the program.
    TerminalResize(TerminalSize),
    /// Leave the terminal. The program keeps running — a pane is a view.
    CloseTerminal,
    /// Answer a pending request.
    Resolve {
        /// Request id, as listed.
        id: String,
        /// What to answer.
        option: PermissionOption,
    },
    /// Stop the worker.
    Quit,
}

/// What the worker tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The daemon answered, and says which version it is.
    Health {
        /// Version string.
        version: String,
    },
    /// The profile's agents.
    Agents(Vec<AgentSummary>),
    /// The workspace root the daemon serves, for the tree's title.
    Workspace(String),
    /// One directory, as the daemon listed it.
    Listing {
        /// The workspace-relative path that was listed; empty is the root.
        path: String,
        /// Its entries, already sorted by the daemon.
        entries: Vec<acpilot_proto::file::FileEntry>,
    },
    /// A workspace search's answer.
    ///
    /// The query comes back with the rows because a person types faster than a walk
    /// finishes: an answer to the query before this one is an answer to a question
    /// nobody is asking any more, and the app can only tell by the words.
    Found {
        /// The query that was asked.
        query: String,
        /// The files whose paths contain it, sorted by the daemon.
        entries: Vec<acpilot_proto::file::FileEntry>,
    },
    /// A content search's answer, tagged with the query that asked for the same
    /// reason [`Update::Found`] is: the words on screen are the newest question.
    Matches {
        /// The query that was asked.
        query: String,
        /// The lines that contain it, sorted by path and line.
        matches: Vec<acpilot_proto::file::GrepMatch>,
    },
    /// A references answer, tagged with the name that asked for the same reason the
    /// searches are tagged with their queries.
    References {
        /// The identifier that was asked about.
        name: String,
        /// Its uses in code, sorted by path, line and column.
        hits: Vec<acpilot_proto::file::Reference>,
    },
    /// What a language server said about a buffer.
    Diagnostics {
        /// The file the answer is about.
        path: String,
        /// The problems, and which server found them — absent when none was asked.
        problems: acpilot_proto::file::FileDiagnostics,
    },
    /// Problems a language server pushed about a file without being asked.
    ///
    /// Different from [`Update::Diagnostics`] because it is not an answer: a server
    /// that finishes an index speaks again on its own, and a pane that only knew about
    /// the problems it had asked for would keep a stale gutter until somebody typed.
    /// The app applies it to the open buffer when the path matches and leaves the
    /// question state alone, because the two are about different versions of the text.
    DiagnosticsPushed(acpilot_proto::file::DiagnosticsNotice),
    /// A question about a buffer was asked and nothing answered it.
    ///
    /// Not the same as [`Update::Diagnostics`] naming no server: a server that said
    /// nothing is an answer, and this is the client never getting one. It exists so a
    /// question stops reading as still in flight — a client that never hears back has
    /// to be told that it will not, or the status line would say `checking` for ever.
    DiagnosticsUnanswered {
        /// The file the question was about.
        path: String,
        /// What went wrong, worded the way the status line should say it.
        message: String,
    },
    /// The project map, as the daemon read it.
    ///
    /// The map rather than its text: the daemon parsed it, so a client cannot
    /// disagree with the other one about what the file says.
    ProjectMap(acpilot_proto::project::ProjectMap),
    /// A file's text, for the editor.
    File {
        /// The path the daemon answered with, which is the one to show.
        path: String,
        /// The UTF-8 contents.
        text: String,
        /// The token a save sends back as `if_match`.
        etag: String,
        /// The lines that differ from the last commit, for the gutter.
        marks: Vec<acpilot_proto::file::LineMark>,
    },
    /// A buffer was written back.
    Saved {
        /// The path the daemon answered with.
        path: String,
        /// The token the next save must send, which is the new file's.
        etag: String,
    },
    /// The integrations the bar draws, as the daemon reported them.
    Stats(Vec<IntegrationStats>),
    /// The session list.
    Sessions(Vec<SessionSummary>),
    /// A session was opened; its transcript starts empty.
    Opened {
        /// Session id.
        id: String,
    },
    /// One event on the open session.
    Event(SessionEvent),
    /// The pending permission list, as the daemon has it now.
    Permissions(Vec<PermissionRequest>),
    /// The panes a person may attach to: the open session's, and the ones no
    /// session owns, as the daemon lists them.
    Terminals(Vec<TerminalSummary>),
    /// The attach succeeded. The screen replay follows as [`Update::TerminalData`].
    TerminalOpened {
        /// The terminal, as the daemon sees it.
        terminal: TerminalSummary,
        /// How many bytes of screen replay follow.
        replay_bytes: usize,
    },
    /// Screen bytes: the replay, then every chunk as it is produced.
    TerminalData(Vec<u8>),
    /// The program ended.
    TerminalExited {
        /// Exit code, when the host reported one.
        code: Option<i32>,
    },
    /// The attach is over — after an exit, or because the socket went away.
    TerminalClosed,
    /// The daemon's preferences, resolved into the theme to draw with.
    ///
    /// Separate from [`Update::Theme`], which is a settings file this client can
    /// read itself: the daemon's answer is the authoritative one, so the first of
    /// these takes over from the file watcher rather than racing it.
    DaemonTheme(crate::Theme),
    /// A turn was accepted.
    Prompted {
        /// Session id.
        session_id: String,
        /// Sequence the turn's output starts at.
        first_seq: u64,
        /// Identifier for the turn.
        turn_id: String,
    },
    /// Something the user should read, but which is not a transcript event.
    Status(String),
    /// The settings named a different theme, and it resolved.
    ///
    /// Not from the daemon: the watcher in [`crate::watch_theme`] sends this, and
    /// it arrives through the same channel as everything else so a theme change is
    /// one more thing the loop applies rather than a second path into the state.
    Theme(crate::theme::Theme),
}

/// The worker's handle: where requests go, and the thread running it.
pub struct Worker {
    requests: UnboundedSender<Request>,
}

impl Worker {
    /// Send a request. A closed channel means the worker is gone, which the UI
    /// treats as a failed daemon and shows in the status line.
    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }
}

/// Start the worker for the daemon at `daemon`, with `token` when it has one.
pub fn spawn(
    daemon: String,
    token: Option<String>,
    terminal: Option<crate::TerminalColours>,
    updates: UnboundedSender<Update>,
) -> Worker {
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("acpilot-tui-client".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    let client = match &token {
                        Some(token) => Client::with_token(daemon, token),
                        None => Client::new(daemon),
                    };
                    // The attach needs the token too, and the client keeps it
                    // private: the worker is the one that resolves it, so it is
                    // passed along rather than looked up again.
                    runtime.block_on(run(client, token, terminal, receiver, updates))
                }
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "no runtime for the daemon client: {error}"
                    )));
                }
            }
        })
        .expect("the TUI client thread starts");
    Worker { requests }
}

/// What the worker sends the attach task.
#[derive(Debug)]
enum TerminalCommand {
    /// Bytes to type.
    Write(Vec<u8>),
    /// A new shape for the screen.
    Resize(TerminalSize),
    /// Leave, without ending the program.
    Close,
}

/// The task holding one attach, and the way to talk to it.
struct TerminalLink {
    commands: UnboundedSender<TerminalCommand>,
    task: tokio::task::JoinHandle<()>,
}

/// Start following one terminal, replacing nothing: the caller drops the old link.
fn start_attach(
    endpoint: &str,
    token: Option<String>,
    id: String,
    updates: &UnboundedSender<Update>,
) -> TerminalLink {
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(follow_terminal(
        endpoint.to_owned(),
        token,
        id,
        receiver,
        updates.clone(),
    ));
    TerminalLink { commands, task }
}

async fn run(
    client: Client,
    token: Option<String>,
    terminal: Option<crate::TerminalColours>,
    mut requests: UnboundedReceiver<Request>,
    updates: UnboundedSender<Update>,
) {
    match client.health().await {
        Ok(health) => {
            if let Some(workspace) = &health.workspace {
                let _ = updates.send(Update::Workspace(workspace.clone()));
            }
            let _ = updates.send(Update::Health {
                version: health.version,
            });
        }
        Err(error) => {
            let _ = updates.send(Update::Status(format!(
                "cannot reach {}: {error}",
                client.base()
            )));
        }
    }
    refresh(&client, false, &updates).await;

    // The profile-wide stream is what raises a permission card without the user
    // pressing anything, and what says the settings changed when this client
    // cannot see the daemon's disk itself. It runs for the life of the TUI.
    {
        let client = client.clone();
        let updates = updates.clone();
        tokio::spawn(async move { follow_permissions(client, terminal, updates).await });
    }

    let mut open: Option<(String, tokio::task::JoinHandle<()>)> = None;
    // One attach at a time, like the pane. The task owns the socket; this is how
    // the loop types into it and how it is stopped.
    let mut terminal: Option<TerminalLink> = None;
    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats = tokio::time::interval(STATS_POLL);
    stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll.tick() => {
                match client.sessions().await {
                    Ok(sessions) => { let _ = updates.send(Update::Sessions(sessions)); }
                    // A daemon that went away is reported once per poll; the
                    // status line is a line, not a log.
                    Err(error) => { let _ = updates.send(Update::Status(describe(&error))); }
                }
                // The terminal list changes when an agent opens one, which is
                // exactly when a person wants to attach, so it is polled too.
                if let Some((id, _)) = &open {
                    refresh_terminals(&client, id, &updates).await;
                }
            }
            _ = stats.tick() => refresh_stats(&client, &updates).await,
            request = requests.recv() => match request {
                None | Some(Request::Quit) => {
                    if let Some((_, task)) = open.take() {
                        task.abort();
                    }
                    if let Some(link) = terminal.take() {
                        link.task.abort();
                    }
                    return;
                }
                // Which terminal is the app's question, not the worker's: it is
                // the app that has the list on screen and the picker in front of
                // it. A dead terminal still has a screen worth looking at, so a
                // closed one attaches the same way.
                Some(Request::OpenTerminal { id }) => {
                    if let Some(link) = terminal.take() {
                        link.task.abort();
                    }
                    terminal = Some(start_attach(client.base(), token.clone(), id, &updates));
                }
                // The daemon starts the shell and answers with it running, so the
                // attach below cannot arrive before there is a screen to attach to.
                Some(Request::NewTerminal { workspace, size }) => {
                    let Some((session, _)) = &open else {
                        let _ = updates.send(Update::Status("no session is open".to_owned()));
                        continue;
                    };
                    let request = acpilot_proto::terminal::CreateTerminalRequest {
                        session_id: Some(session.clone()),
                        workspace,
                        command: None,
                        args: Vec::new(),
                        cwd: None,
                        size,
                    };
                    match client.create_terminal(&request).await {
                        Ok(summary) => {
                            let _ = updates.send(Update::Status(format!(
                                "started {} · attaching",
                                summary.command
                            )));
                            refresh_terminals(&client, session, &updates).await;
                            if let Some(link) = terminal.take() {
                                link.task.abort();
                            }
                            let id = summary.id;
                            terminal = Some(start_attach(client.base(), token.clone(), id, &updates));
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::TerminalWrite(bytes)) => {
                    if let Some(link) = &terminal {
                        let _ = link.commands.send(TerminalCommand::Write(bytes));
                    }
                }
                Some(Request::TerminalResize(size)) => {
                    if let Some(link) = &terminal {
                        let _ = link.commands.send(TerminalCommand::Resize(size));
                    }
                }
                Some(Request::CloseTerminal) => {
                    if let Some(link) = terminal.take() {
                        let _ = link.commands.send(TerminalCommand::Close);
                    }
                }
                Some(Request::Refresh { include_ignored }) => {
                    refresh(&client, include_ignored, &updates).await;
                }
                Some(Request::ListDir {
                    path,
                    include_ignored,
                }) => {
                    match client.files(&path, include_ignored).await {
                        Ok(entries) => {
                            let _ = updates.send(Update::Listing { path, entries });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::SearchFiles(query)) => {
                    match client.search_files(&query).await {
                        Ok(entries) => {
                            let _ = updates.send(Update::Found { query, entries });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::SaveFile { path, text, etag }) => {
                    let body = acpilot_proto::file::FileWriteRequest {
                        workspace: None,
                        path: path.clone(),
                        text,
                        if_match: Some(etag),
                    };
                    match client.write_file(&body).await {
                        Ok(content) => {
                            let _ = updates.send(Update::Saved {
                                path: content.path,
                                etag: content.etag,
                            });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Grep(query)) => {
                    match client.grep(&query).await {
                        Ok(matches) => {
                            let _ = updates.send(Update::Matches { query, matches });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::References {
                    name,
                    path,
                    offset,
                    text,
                }) => {
                    match client.references(&name, Some((&path, offset, &text))).await {
                        Ok(hits) => {
                            let _ = updates.send(Update::References { name, hits });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Diagnostics { path, text }) => {
                    match client.diagnostics(&path, &text).await {
                        Ok(problems) => {
                            let _ = updates.send(Update::Diagnostics { path, problems });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::DiagnosticsUnanswered {
                                path,
                                message: describe(&error),
                            });
                        }
                    }
                }
                Some(Request::LspStatus) => match client.lsp_servers().await {
                    Ok(servers) => {
                        let _ = updates.send(Update::Status(language_servers_line(&servers)));
                    }
                    Err(error) => {
                        let _ = updates.send(Update::Status(describe(&error)));
                    }
                },
                Some(Request::ProjectMap) => {
                    match client.project_map().await {
                        Ok(map) => {
                            let _ = updates.send(Update::ProjectMap(map));
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::OpenFile(path)) => {
                    match client.file(&path).await {
                        Ok(content) => {
                            let _ = updates.send(Update::File {
                                path: content.path,
                                text: content.text,
                                etag: content.etag,
                                marks: content.marks,
                            });
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Select(id)) => {
                    if let Some((existing, _)) = &open
                        && existing == &id
                    {
                        continue;
                    }
                    if let Some((_, task)) = open.take() {
                        task.abort();
                    }
                    let _ = updates.send(Update::Opened { id: id.clone() });
                    let task = {
                        let client = client.clone();
                        let updates = updates.clone();
                        let id = id.clone();
                        tokio::spawn(async move { follow_session(client, id, updates).await })
                    };
                    refresh_terminals(&client, &id, &updates).await;
                    open = Some((id, task));
                }
                Some(Request::Prompt(text)) => {
                    let Some((id, _)) = &open else {
                        let _ = updates.send(Update::Status("no session is open".to_owned()));
                        continue;
                    };
                    match client.prompt(id, text).await {
                        Ok(accepted) => {
                            let _ = updates.send(Update::Prompted {
                                session_id: accepted.session_id,
                                first_seq: accepted.first_seq,
                                turn_id: accepted.turn_id,
                            });
                            refresh_sessions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Cancel) => {
                    let Some((id, _)) = &open else { continue };
                    match client.cancel(id).await {
                        Ok(outcome) if outcome.cancelled => {
                            let turn = outcome.turn_id.unwrap_or_default();
                            let _ = updates.send(Update::Status(format!("cancel sent for {turn}")));
                        }
                        Ok(_) => {
                            let _ = updates.send(Update::Status("no turn was running".to_owned()));
                        }
                        Err(error) => { let _ = updates.send(Update::Status(describe(&error))); }
                    }
                }
                Some(Request::New(agent_id)) => {
                    let request = CreateSessionRequest {
                        // The daemon serves one workspace; an empty id means it.
                        workspace_id: String::new(),
                        agent_id,
                        title: None,
                    };
                    match client.create_session(&request).await {
                        Ok(summary) => {
                            let _ = updates.send(Update::Opened {
                                id: summary.id.clone(),
                            });
                            if let Some((_, task)) = open.take() {
                                task.abort();
                            }
                            let task = {
                                let client = client.clone();
                                let updates = updates.clone();
                                let id = summary.id.clone();
                                tokio::spawn(async move { follow_session(client, id, updates).await })
                            };
                            open = Some((summary.id.clone(), task));
                            refresh_sessions(&client, &updates).await;
                            refresh_terminals(&client, &summary.id, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Delete(id)) => {
                    if let Some((existing, _)) = &open
                        && existing == &id
                        && let Some((_, task)) = open.take()
                    {
                        task.abort();
                    }
                    match client.delete_session(&id).await {
                        Ok(summary) => {
                            let _ = updates.send(Update::Status(format!("ended {}", summary.id)));
                            refresh_sessions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                        }
                    }
                }
                Some(Request::Resolve { id, option }) => {
                    match client.resolve(&id, option).await {
                        Ok(decision) => {
                            let _ = updates.send(Update::Status(format!(
                                "answered {}: {}",
                                id, decision.decided_by
                            )));
                            refresh_permissions(&client, &updates).await;
                        }
                        Err(error) => {
                            let _ = updates.send(Update::Status(describe(&error)));
                            refresh_permissions(&client, &updates).await;
                        }
                    }
                }
            }
        }
    }
}

/// Re-read everything the UI shows at startup.
async fn refresh(client: &Client, include_ignored: bool, updates: &UnboundedSender<Update>) {
    match client.agents().await {
        Ok(agents) => {
            let _ = updates.send(Update::Agents(agents));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
    refresh_sessions(client, updates).await;
    refresh_permissions(client, updates).await;
    refresh_stats(client, updates).await;
    refresh_files(client, include_ignored, updates).await;
}

/// Read the workspace root, so the tree has rows before a key is pressed.
///
/// The root only: its children arrive when a directory is opened. A daemon with no
/// workspace answers `409` and the status line says so, which is the honest answer
/// to a tree with nothing behind it.
async fn refresh_files(client: &Client, include_ignored: bool, updates: &UnboundedSender<Update>) {
    match client.files("", include_ignored).await {
        Ok(entries) => {
            let _ = updates.send(Update::Listing {
                path: String::new(),
                entries,
            });
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

async fn refresh_sessions(client: &Client, updates: &UnboundedSender<Update>) {
    match client.sessions().await {
        Ok(sessions) => {
            let _ = updates.send(Update::Sessions(sessions));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

/// Read the panes a person may attach to: the open session's, and the sessionless
/// ones.
///
/// Sent as soon as a session opens as well as on every poll: an agent's shell is
/// exactly what a person presses `Ctrl-T` for, and a list that arrives a second
/// later is a pane that says the session has none.
///
/// One request rather than two, because `GET /v1/terminals` with no filter is the
/// only listing that can name both: the session's panes and the ones an agent
/// opened whose session this daemon could not map. A pane with no session cannot
/// be listed under one, so filtering the answer here is the only way it is ever
/// found.
async fn refresh_terminals(client: &Client, session: &str, updates: &UnboundedSender<Update>) {
    match client.terminals(None).await {
        Ok(terminals) => {
            let _ = updates.send(Update::Terminals(listed(session, terminals)));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

/// The panes of `session`, plus the panes no session owns.
///
/// Another session's shell is left out: it is already listed where it belongs,
/// and offering it here would be a pane a person could type into without ever
/// having opened the conversation it belongs to.
fn listed(session: &str, terminals: Vec<TerminalSummary>) -> Vec<TerminalSummary> {
    terminals
        .into_iter()
        .filter(|terminal| {
            terminal
                .session_id
                .as_deref()
                .is_none_or(|id| id == session)
        })
        .collect()
}

/// Read the integrations the stats bar draws.
///
/// A daemon that answers with no rows is a daemon with no roster, which the bar
/// says; a request that fails leaves the last rows on screen rather than blanking a
/// bar a person is reading — the status line already reports the failure.
async fn refresh_stats(client: &Client, updates: &UnboundedSender<Update>) {
    match client.stats().await {
        Ok(stats) => {
            let _ = updates.send(Update::Stats(stats));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

async fn refresh_permissions(client: &Client, updates: &UnboundedSender<Update>) {
    match client.pending_permissions().await {
        Ok(pending) => {
            let _ = updates.send(Update::Permissions(pending));
        }
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
        }
    }
}

/// Follow one session, reconnecting with the last sequence seen.
async fn follow_session(client: Client, id: String, updates: UnboundedSender<Update>) {
    let mut after = 0;
    loop {
        let frame = client.session_events(&id, after);
        tokio::pin!(frame);
        while let Some(item) = frame.next().await {
            match item {
                Ok(SessionStream::Event(event)) => {
                    after = event.seq;
                    if updates.send(Update::Event(event)).is_err() {
                        return;
                    }
                }
                Ok(SessionStream::Heartbeat { seq }) => after = after.max(seq),
                Ok(SessionStream::Resync { latest_seq }) => {
                    // The stream cannot replay what it dropped; asking again
                    // from where it now is is the only honest recovery.
                    after = latest_seq;
                    break;
                }
                Ok(SessionStream::Other { .. }) => {}
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "event stream: {}",
                        describe(&error)
                    )));
                    break;
                }
            }
        }
        tokio::time::sleep(RECONNECT).await;
        if after == 0 {
            // A first connection that never worked: say so once, then keep
            // trying quietly rather than filling the status line every 2 s.
            let _ = updates.send(Update::Status(format!(
                "waiting for session {id} on the daemon"
            )));
            return;
        }
    }
}

/// Hold one terminal attach: type into it, resize it, and forward its screen.
///
/// A task rather than a branch of the worker's loop because the socket is one
/// more source of frames and the worker is already selecting over three; giving
/// the attach its own loop is what keeps a quiet terminal from delaying a
/// keystroke or a poll.
async fn follow_terminal(
    endpoint: String,
    token: Option<String>,
    id: String,
    mut commands: UnboundedReceiver<TerminalCommand>,
    updates: UnboundedSender<Update>,
) {
    let mut terminal = match Attached::open(&endpoint, token.as_deref(), &id).await {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = updates.send(Update::Status(describe(&error)));
            let _ = updates.send(Update::TerminalClosed);
            return;
        }
    };

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(TerminalCommand::Write(bytes)) => {
                    if let Err(error) = terminal.write(&bytes).await {
                        let _ = updates.send(Update::Status(describe(&error)));
                        break;
                    }
                }
                Some(TerminalCommand::Resize(size)) => {
                    if let Err(error) = terminal.resize(size).await {
                        let _ = updates.send(Update::Status(describe(&error)));
                        break;
                    }
                }
                Some(TerminalCommand::Close) | None => break,
            },
            frame = terminal.next() => match frame {
                Ok(Some(TerminalEvent::Attached {
                    terminal,
                    replay_bytes,
                })) => {
                    let _ = updates.send(Update::TerminalOpened {
                        terminal,
                        replay_bytes,
                    });
                }
                Ok(Some(TerminalEvent::Data(bytes))) => {
                    let _ = updates.send(Update::TerminalData(bytes));
                }
                Ok(Some(TerminalEvent::Exit { code })) => {
                    // The daemon closes after this, so the exit is the last thing
                    // said; the close below is just the socket agreeing.
                    let _ = updates.send(Update::TerminalExited { code });
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = updates.send(Update::Status(describe(&error)));
                    break;
                }
            },
        }
    }
    let _ = updates.send(Update::TerminalClosed);
}

/// Follow the daemon's permission announcements and its settings.
async fn follow_permissions(
    client: Client,
    terminal: Option<crate::TerminalColours>,
    updates: UnboundedSender<Update>,
) {
    loop {
        let frames = client.profile_events();
        tokio::pin!(frames);
        while let Some(item) = frames.next().await {
            match item {
                Ok(ProfileStream::Permission(request)) => {
                    let _ = updates.send(Update::Status(format!(
                        "{} wants to {}",
                        request.session_id,
                        request.title.to_lowercase()
                    )));
                }
                // The stream opens with the current preferences, so a terminal
                // that shares no filesystem with its daemon still starts with the
                // theme the daemon would draw with, and a save arrives as the next
                // one. A theme this build cannot resolve is said out loud rather
                // than ignored: it is a preference, and somebody chose it.
                Ok(ProfileStream::Preferences(preferences)) => {
                    match crate::theme_for(&preferences, terminal) {
                        Some(theme) => {
                            let _ = updates.send(Update::DaemonTheme(theme));
                        }
                        None => {
                            let _ = updates.send(Update::Status(
                                "the daemon's preferences name a theme this build cannot \
                                 resolve; keeping the colours on screen"
                                    .to_owned(),
                            ));
                        }
                    }
                }
                Ok(ProfileStream::Diagnostics(notice)) => {
                    let _ = updates.send(Update::DiagnosticsPushed(notice));
                }
                // Somebody answered a question this terminal may be showing. The
                // pending list is state, so the answer is to refetch it rather than
                // to guess which dialog was open: a second window resolving a
                // request must not leave this one blocking a turn nobody is in.
                Ok(ProfileStream::Resolved { request_id, .. }) => {
                    let _ = updates.send(Update::Status(format!(
                        "{request_id} was answered elsewhere"
                    )));
                    refresh_permissions(&client, &updates).await;
                }
                Ok(ProfileStream::Resync) | Ok(ProfileStream::Heartbeat) => {
                    let _ = refresh_permissions(&client, &updates).await;
                }
                Ok(ProfileStream::Other { .. }) => {}
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "permission stream: {}",
                        describe(&error)
                    )));
                    break;
                }
            }
            // The announcement says a request exists; the pending list is the
            // state. Refetching keeps the modal and the list from disagreeing.
            let _ = refresh_permissions(&client, &updates).await;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

/// Start the design pass's worker: the seeded world instead of a daemon.
///
/// It speaks the same [`Update`] channel the real worker does, so the UI cannot
/// tell the difference — which is the point of the exercise. What changes is who
/// answers: the fixture replays a stored transcript when a session opens, replies
/// to a prompt from a script, and hands a terminal attach a screen it kept.
/// Everything a pane draws is still the pane's own code.
pub fn spawn_demo(demo: acpilot_demo::Demo, updates: UnboundedSender<Update>) -> Worker {
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("acpilot-tui-demo".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(run_demo(demo, receiver, updates)),
                Err(error) => {
                    let _ = updates.send(Update::Status(format!(
                        "no runtime for the seeded world: {error}"
                    )));
                }
            }
        })
        .expect("the demo thread starts");
    Worker { requests }
}

/// How long the seeded transcript takes between events.
///
/// Slow enough to see a chunk arrive and fast enough that a design review does
/// not wait: the point is that the panes work while events stream, not that the
/// fixture is quick.
const REPLAY: Duration = Duration::from_millis(45);

async fn run_demo(
    demo: acpilot_demo::Demo,
    mut requests: UnboundedReceiver<Request>,
    updates: UnboundedSender<Update>,
) {
    announce(&demo, &updates);
    // Sequences continue past the seeded transcript, so a scripted reply can never
    // collide with a replayed event and be dropped as a duplicate.
    let mut seq = demo
        .sessions
        .iter()
        .flat_map(|fixture| fixture.events.iter().map(|event| event.seq))
        .max()
        .unwrap_or(0);

    while let Some(request) = requests.recv().await {
        match request {
            Request::Quit => return,
            // The design pass has one tree and one file, and says so rather than
            // pretending a fixture is a workspace.
            Request::ListDir { .. } => {
                let _ = updates.send(Update::Status(
                    "design pass · the tree is the fixture's".to_owned(),
                ));
            }
            Request::SearchFiles(query) => {
                let _ = updates.send(Update::Status(format!(
                    "design pass · no workspace to walk for {query}"
                )));
            }
            // The fixture is the design pass's own buffer, and it is not a file: a
            // save has nowhere to go, and the status line says which world the window
            // is in rather than pretending a write happened.
            Request::SaveFile { path, .. } => {
                let _ = updates.send(Update::Status(format!(
                    "design pass · {path} is the fixture's text"
                )));
            }
            Request::Grep(query) => {
                // The design pass has one file, and greps it: enough to review the
                // list without pretending the fixture is a workspace.
                let needle = query.to_lowercase();
                let matches = demo
                    .editor
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| !needle.is_empty() && line.to_lowercase().contains(&needle))
                    .map(|(index, line)| acpilot_proto::file::GrepMatch {
                        path: demo.editor.path.clone(),
                        line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                        text: line.trim_end().to_owned(),
                    })
                    .collect();
                let _ = updates.send(Update::Matches { query, matches });
            }
            Request::References { name, .. } => {
                // The design pass parses its one fixture: the same filter over the text
                // it drew, so the panel can be reviewed without a workspace.
                let text = demo.editor.lines.join("\n");
                let lines: Vec<&str> = text.split('\n').collect();
                let language = acpilot_text::language_for(&demo.editor.path);
                let hits = acpilot_text::references::uses(language, &text, &name)
                    .into_iter()
                    .map(|hit| acpilot_proto::file::Reference {
                        path: demo.editor.path.clone(),
                        line: u32::try_from(hit.line + 1).unwrap_or(u32::MAX),
                        column: u32::try_from(hit.column + 1).unwrap_or(u32::MAX),
                        text: lines
                            .get(hit.line)
                            .map_or_else(String::new, |line| line.trim_end().to_owned()),
                    })
                    .collect();
                let _ = updates.send(Update::References { name, hits });
            }
            Request::Diagnostics { path, .. } => {
                // No daemon, no server: the design pass says which world it is in rather
                // than drawing a clean file, which is a check nobody ran. It is an
                // unanswered question rather than a plain status, so the frame stops
                // saying `checking` the way it would after a real answer.
                let _ = updates.send(Update::DiagnosticsUnanswered {
                    path,
                    message: "no language server · the design pass reads its own text".to_owned(),
                });
            }
            Request::LspStatus => {
                // No daemon, no servers: the design pass says which world it is in
                // rather than reporting an empty list as a fact.
                let _ = updates.send(Update::Status(
                    "design pass · there is no daemon to ask".to_owned(),
                ));
            }
            Request::ProjectMap => {
                // The design pass has no workspace, so it has no map: the panel is
                // drawn from its empty state, which is a state worth reviewing.
                let _ = updates.send(Update::Status(
                    "design pass · no workspace to map".to_owned(),
                ));
            }
            Request::OpenFile(path) => {
                if path == demo.editor.path {
                    let _ = updates.send(Update::File {
                        path: demo.editor.path.clone(),
                        text: demo.editor.lines.join("\n"),
                        // The fixture is not a file: no version to replace, and the
                        // repository its gutter was designed against is not read here.
                        etag: String::new(),
                        marks: Vec::new(),
                    });
                } else {
                    let _ = updates.send(Update::Status(format!(
                        "design pass · only {} is behind this tree",
                        demo.editor.path
                    )));
                }
            }
            Request::Select(id) => {
                let Some(fixture) = demo
                    .sessions
                    .iter()
                    .find(|fixture| fixture.summary.id == id)
                else {
                    let _ = updates.send(Update::Status(format!(
                        "the design pass has no session {id}"
                    )));
                    continue;
                };
                let _ = updates.send(Update::Opened {
                    id: fixture.summary.id.clone(),
                });
                for event in &fixture.events {
                    let _ = updates.send(Update::Event(event.clone()));
                    tokio::time::sleep(REPLAY).await;
                }
            }
            Request::Prompt(_) => {
                // The composer works, the turn does not: the reply is a script, so
                // the pane shows what streaming looks like without pretending a
                // model answered.
                let session = demo
                    .sessions
                    .first()
                    .map(|fixture| fixture.summary.id.clone())
                    .unwrap_or_default();
                for event in acpilot_demo::scripted_reply(seq + 1, &session) {
                    seq = event.seq;
                    let _ = updates.send(Update::Event(event));
                    tokio::time::sleep(REPLAY).await;
                }
                let _ = updates.send(Update::Status(
                    "design pass · the reply is scripted; a real turn needs the daemon".to_owned(),
                ));
            }
            Request::OpenTerminal { id } => {
                let terminal = demo
                    .terminals
                    .iter()
                    .find(|terminal| terminal.id == id)
                    .or_else(|| demo.terminals.first());
                let Some(terminal) = terminal.cloned() else {
                    let _ = updates.send(Update::Status(
                        "the design pass has no terminal to attach".to_owned(),
                    ));
                    continue;
                };
                let screen = demo.terminal_screen.clone().into_bytes();
                let _ = updates.send(Update::TerminalOpened {
                    terminal,
                    replay_bytes: screen.len(),
                });
                let _ = updates.send(Update::TerminalData(screen));
            }
            Request::CloseTerminal => {
                let _ = updates.send(Update::TerminalClosed);
            }
            // A shell is a process, and the design pass has none: saying so is the
            // honest answer to a key that asked for one.
            Request::NewTerminal { .. } => {
                let _ = updates.send(Update::Status(
                    "design pass · no daemon to start a shell".to_owned(),
                ));
            }
            Request::Resolve { id, option } => {
                let _ = updates.send(Update::Status(format!(
                    "design pass · answered {id} with {option:?} · nothing was written"
                )));
                let _ = updates.send(Update::Permissions(Vec::new()));
            }
            Request::Refresh { .. } => announce(&demo, &updates),
            // A keystroke into the fixture's shell is not answered: the screen it
            // shows is the one it kept, and pretending to run it would be the one
            // thing a design pass must not do.
            Request::TerminalWrite(_) | Request::TerminalResize(_) => {}
            Request::New(_) | Request::Delete(_) | Request::Cancel => {
                let _ = updates.send(Update::Status(
                    "design pass · there is no daemon to ask".to_owned(),
                ));
            }
        }
    }
}

/// Tell the UI what the profile has, the way the daemon's first poll would.
fn announce(demo: &acpilot_demo::Demo, updates: &UnboundedSender<Update>) {
    let _ = updates.send(Update::Agents(demo.agents.clone()));
    let _ = updates.send(Update::Sessions(
        demo.sessions
            .iter()
            .map(|fixture| fixture.summary.clone())
            .collect(),
    ));
    let _ = updates.send(Update::Terminals(demo.terminals.clone()));
}

/// The language servers as one status line.
///
/// A line rather than a pane because the question is asked to check something — is
/// the server running, and is it fast — and a pane would be opened, read and closed
/// for one glance. A language this build has no server for is not a row, which is
/// worth knowing when a references list turned out to be the parse tree's answer.
fn language_servers_line(servers: &[acpilot_proto::lsp::LspServerSummary]) -> String {
    use acpilot_proto::lsp::{LspServerStatus, PositionEncoding};

    if servers.is_empty() {
        return "no language server this build can start".to_owned();
    }
    let rows: Vec<String> = servers
        .iter()
        .map(|server| {
            let state = match server.status {
                LspServerStatus::Starting => "starting",
                LspServerStatus::Ready => "running",
                LspServerStatus::Indexing => "indexing",
                LspServerStatus::Crashed => "crashed",
                LspServerStatus::Stopped => "stopped",
            };
            let encoding = match server.encoding {
                Some(PositionEncoding::Utf8) => " utf-8",
                Some(PositionEncoding::Utf16) => " utf-16",
                None => "",
            };
            let round_trip = server
                .round_trip_ms
                .map_or_else(String::new, |ms| format!(" {ms} ms"));
            let language = server
                .languages
                .first()
                .map_or(server.id.as_str(), String::as_str);
            format!("{language} {state}{encoding}{round_trip}")
        })
        .collect();
    format!("lsp · {}", rows.join(" · "))
}

/// A failure as a status line, with its code when the daemon sent one.
fn describe(error: &ClientError) -> String {
    match error.code() {
        Some(code) => format!("{code}: {error}"),
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::listed;
    use acpilot_proto::terminal::{TerminalSize, TerminalSummary};

    fn pane(id: &str, session: Option<&str>) -> TerminalSummary {
        TerminalSummary {
            id: id.to_owned(),
            session_id: session.map(str::to_owned),
            workspace: None,
            host: None,
            size: TerminalSize { cols: 80, rows: 24 },
            alive: true,
            command: "/bin/sh".to_owned(),
        }
    }

    #[test]
    fn the_list_keeps_the_open_sessions_panes_and_the_ones_nobody_owns() {
        let kept = listed(
            "s1",
            vec![
                pane("mine", Some("s1")),
                pane("orphan", None),
                pane("theirs", Some("s2")),
            ],
        );
        let ids: Vec<&str> = kept.iter().map(|terminal| terminal.id.as_str()).collect();
        assert_eq!(ids, ["mine", "orphan"]);
    }
}
