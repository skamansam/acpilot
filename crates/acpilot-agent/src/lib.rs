//! The agent engine: registry, supervisor, sessions, and the ACP client.
//!
//! Empty in P1 by design. P2 builds it headless and tests it against a mock agent
//! so the rest of the product never has to wait on a real model. The two
//! transports it will own are ACP over stdio and the PTY transcript adapter that
//! lets an ordinary program act as an agent.
