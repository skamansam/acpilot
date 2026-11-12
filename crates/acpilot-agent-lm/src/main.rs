//! The `acpilot-agent-lm` binary: an ACP agent over stdio.
//!
//! It is deliberately thin. Every decision that can be tested lives in the
//! library; this file reads `--recipe` and `--profile`, wires four ACP handlers,
//! and gets out of the way. Promotion (P4.5-9) runs exactly this command.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use acpilot_agent_lm::{
    Loaded, SessionState, load, new_session, prompt_response, prompt_text, turn,
};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ErrorCode, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, SessionId,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Error, Stdio, on_receive_notification, on_receive_request,
};
use clap::Parser;

/// The command as `agents.toml` spells it.
#[derive(Debug, Parser)]
#[command(
    name = "acpilot-agent-lm",
    about = "Run a model recipe as an ACP agent",
    version
)]
struct Args {
    /// Recipe id, as it appears under the profile's `recipes/`.
    #[arg(long)]
    recipe: String,

    /// Profile that owns the recipe and its provider.
    #[arg(long)]
    profile: Option<String>,
}

type Sessions = Arc<Mutex<HashMap<String, Arc<SessionState>>>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args = Args::parse();
    let profile = args
        .profile
        .or_else(|| std::env::var("ACPILOT_RECIPE_PROFILE").ok())
        .or_else(|| std::env::var("ACPILOT_PROFILE").ok());
    let profile = match profile {
        Some(profile) if !profile.trim().is_empty() => profile,
        _ => {
            eprintln!("acpilot-agent-lm: no profile: pass --profile or set ACPILOT_RECIPE_PROFILE");
            std::process::exit(2);
        }
    };

    let loaded = match load(&profile, &args.recipe) {
        Ok(loaded) => Arc::new(loaded),
        Err(error) => {
            // Exit before stdio is claimed: a client that spawns this must see the
            // reason in stderr, not a protocol error about a missing recipe.
            eprintln!("acpilot-agent-lm: {error}");
            std::process::exit(2);
        }
    };

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));

    let new_sessions = Arc::clone(&sessions);
    let new_counter = Arc::clone(&counter);
    let prompt_sessions = Arc::clone(&sessions);
    let prompt_loaded = Arc::clone(&loaded);
    let cancel_sessions = Arc::clone(&sessions);

    let result = Agent
        .builder()
        .name("acpilot-agent-lm")
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .agent_info(Implementation::new(
                            "acpilot-agent-lm",
                            env!("CARGO_PKG_VERSION"),
                        )),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                // The sequence is this process's own; a session id only has to be
                // unique within the conversation the client opened.
                let sequence = new_counter.fetch_add(1, Ordering::Relaxed) + 1;
                let (id, state) = new_session(&request, sequence);
                new_sessions
                    .lock()
                    .expect("sessions lock")
                    .insert(id.clone(), Arc::new(state));
                responder.respond(NewSessionResponse::new(id))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                let key = request.session_id.0.to_string();
                let session = prompt_sessions
                    .lock()
                    .expect("sessions lock")
                    .get(&key)
                    .cloned();
                let Some(session) = session else {
                    return responder.respond_with_error(Error::new(
                        i32::from(ErrorCode::InvalidRequest),
                        format!("no session `{key}` in this agent"),
                    ));
                };

                session.push_user(prompt_text(&request));
                let loaded: Arc<Loaded> = Arc::clone(&prompt_loaded);
                // The turn runs on the connection, not inside this handler: a
                // handler that awaits the model would stop dispatching the
                // `session/cancel` that is supposed to interrupt it.
                let session_id = SessionId::new(key.clone());
                let task = connection.clone();
                connection.spawn(async move {
                    match turn(&session, &session_id, &loaded, &task).await {
                        Ok(result) => responder.respond(prompt_response(&result)),
                        Err(error) => responder.respond_with_error(error),
                    }
                })
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx| {
                let key = notification.session_id.0.to_string();
                if let Some(session) = cancel_sessions.lock().expect("sessions lock").get(&key) {
                    session.cancel();
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await;

    if let Err(error) = result {
        eprintln!("acpilot-agent-lm: {error}");
        std::process::exit(1);
    }
}
