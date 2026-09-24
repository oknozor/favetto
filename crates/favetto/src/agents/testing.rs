//! Test doubles for the agent-state seam, shared by the `agents` and
//! `attention` unit tests. Only compiled under `cfg(test)`.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use favetto_core::model::{AgentStateEvent, InputReply};

use super::agent::{Agent, AgentContext, AgentDescriptor, CommandSpec, Invocation, SubmitStrategy};
use super::state::{InputResponder, StateContext, StateSource, StateSourceConfig, StateStart};

/// Records the structured replies a session sends through its state channel.
#[derive(Default)]
pub(crate) struct FakeResponder {
    replies: Mutex<Vec<(String, InputReply)>>,
}

impl FakeResponder {
    /// The `(request_id, reply)` pairs recorded so far.
    pub(crate) fn replies(&self) -> Vec<(String, InputReply)> {
        self.replies.lock().clone()
    }
}

impl InputResponder for FakeResponder {
    fn reply<'a>(
        &'a self,
        request_id: &'a str,
        reply: InputReply,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        self.replies.lock().push((request_id.to_string(), reply));
        Box::pin(async { Ok(()) })
    }
}

/// A scripted [`StateSource`] whose events come from the test, not a real CLI.
struct FakeStateSource {
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentStateEvent>>>,
    responder: Arc<FakeResponder>,
}

impl StateSource for FakeStateSource {
    fn label(&self) -> &'static str {
        "fake"
    }

    fn start(&self, _ctx: StateContext) -> anyhow::Result<StateStart> {
        let events = self
            .events_rx
            .lock()
            .take()
            .ok_or_else(|| anyhow::anyhow!("fake state source already started"))?;
        Ok(StateStart {
            events,
            stdout: None,
            responder: self.responder.clone(),
            stop: None,
        })
    }
}

/// A test agent that runs `script` under `sh` and reports a scripted state
/// channel instead of a screen-derived one.
struct FakeStateAgent {
    descriptor: AgentDescriptor,
    script: String,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentStateEvent>>>,
    responder: Arc<FakeResponder>,
}

impl Agent for FakeStateAgent {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }

    fn command(&self, _: &Invocation<'_>, _: &AgentContext) -> anyhow::Result<CommandSpec> {
        Ok(CommandSpec {
            program: PathBuf::from("sh"),
            args: vec!["-c".to_string(), self.script.clone()],
            env: Default::default(),
            cwd: None,
            stdin_prompt: None,
            stdin_eof: false,
            submit: SubmitStrategy::None,
        })
    }

    fn state_source(&self, _cfg: &StateSourceConfig) -> Option<Box<dyn StateSource>> {
        Some(Box::new(FakeStateSource {
            events_rx: Mutex::new(self.events_rx.lock().take()),
            responder: self.responder.clone(),
        }))
    }
}

/// Handles to a [`FakeStateAgent`] started through [`AgentManager`].
///
/// [`AgentManager`]: super::AgentManager
pub(crate) struct FakeStateHandle {
    /// The agent to pass to `AgentManager::start`.
    pub(crate) agent: Arc<dyn Agent>,
    /// Send normalized events as if a transport produced them.
    pub(crate) events: mpsc::UnboundedSender<AgentStateEvent>,
    /// The recorded structured replies.
    pub(crate) responder: Arc<FakeResponder>,
}

/// Build a fake state-reporting agent that runs `script` under `sh`.
pub(crate) fn fake_state_agent(script: &str) -> FakeStateHandle {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let responder = Arc::new(FakeResponder::default());
    let descriptor = AgentDescriptor {
        id: "fake".to_string(),
        name: "Fake".to_string(),
        command: "sh".to_string(),
        available: true,
        capabilities: favetto_core::model::AgentCapabilities {
            reports_state: true,
            permission_channel: true,
            ..Default::default()
        },
    };
    let agent = FakeStateAgent {
        descriptor,
        script: script.to_string(),
        events_rx: Mutex::new(Some(events_rx)),
        responder: responder.clone(),
    };
    FakeStateHandle {
        agent: Arc::new(agent),
        events: events_tx,
        responder,
    }
}
