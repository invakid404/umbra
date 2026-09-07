//! Typed agent provider protocol over private local IPC.
use super::*;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use umbra_core::provider::{self as wire, protocol_error, Client, ProviderDescriptor};

/// Owned requests for protocol version 1.
#[derive(Serialize, Deserialize)]
pub enum Request {
    /// Capabilities.
    Capabilities {},
    /// Launch plan.
    LaunchPlan {
        /// Request.
        request: Box<AgentLaunchRequest>,
    },
    /// Resume plan.
    ResumePlan {
        /// Request.
        request: Box<AgentResumeRequest>,
        /// Session.
        session: Box<AgentSession>,
    },
    /// Observe.
    Observe {
        /// Event.
        event: AgentEvent,
    },
    /// Stop plan.
    StopPlan {
        /// Session.
        session: Box<AgentSession>,
    },
}
/// Method-tagged successful responses; errors travel in the transport envelope.
#[derive(Serialize, Deserialize)]
pub enum Response {
    /// Capabilities.
    Capabilities(AgentCapabilities),
    /// Launch plan.
    LaunchPlan(AgentLaunchPlan),
    /// Resume plan.
    ResumePlan(AgentLaunchPlan),
    /// Observe.
    Observe(Option<AgentSessionUpdate>),
    /// Stop plan.
    StopPlan(AgentStopPlan),
}
/// Synchronous proxy owning its provider process and connection.
pub struct Proxy {
    client: Mutex<Client>,
    capabilities: AgentCapabilities,
}
impl Proxy {
    /// Validate provider registration and establish a private protocol session.
    pub fn connect(descriptor: &ProviderDescriptor, timeout_ms: u64) -> Result<Self> {
        descriptor.validate("agent")?;
        let mut client = Client::connect(descriptor, timeout_ms)?;
        let capabilities = match client.call(&Request::Capabilities {})? {
            Response::Capabilities(caps) => caps,
            _ => return Err(protocol_error("capabilities response mismatch")),
        };
        Ok(Self {
            client: Mutex::new(client),
            capabilities,
        })
    }
    fn call(&self, request: &Request) -> Result<Response> {
        self.client
            .lock()
            .map_err(|_| protocol_error("poisoned provider connection"))?
            .call(request)
    }
}
impl Agent for Proxy {
    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities.clone()
    }
    fn launch_plan(&self, request: &AgentLaunchRequest) -> Result<AgentLaunchPlan> {
        match self.call(&Request::LaunchPlan {
            request: Box::new(request.clone()),
        })? {
            Response::LaunchPlan(value) => Ok(value),
            _ => Err(protocol_error("agent.launch_plan response mismatch")),
        }
    }
    fn resume_plan(
        &self,
        request: &AgentResumeRequest,
        session: &AgentSession,
    ) -> Result<AgentLaunchPlan> {
        match self.call(&Request::ResumePlan {
            request: Box::new(request.clone()),
            session: Box::new(session.clone()),
        })? {
            Response::ResumePlan(value) => Ok(value),
            _ => Err(protocol_error("agent.resume_plan response mismatch")),
        }
    }
    fn observe(&mut self, event: &AgentEvent) -> Result<Option<AgentSessionUpdate>> {
        match self.call(&Request::Observe {
            event: event.clone(),
        })? {
            Response::Observe(value) => Ok(value),
            _ => Err(protocol_error("agent.observe response mismatch")),
        }
    }
    fn stop_plan(&self, session: &AgentSession) -> Result<AgentStopPlan> {
        match self.call(&Request::StopPlan {
            session: Box::new(session.clone()),
        })? {
            Response::StopPlan(value) => Ok(value),
            _ => Err(protocol_error("agent.stop_plan response mismatch")),
        }
    }
}
/// Construct only this provider's backend after validating the handshake.
pub fn serve_provider<B: Agent>(id: &str, factory: impl FnOnce(&[u8]) -> Result<B>) -> Result<()> {
    let (connection, mut backend) = wire::accept(id, "agent", |options| {
        let backend = factory(options)?;
        let actual = backend.capabilities();
        let capabilities = actual.supported;
        Ok((backend, capabilities))
    })?;
    wire::serve(connection, |request| match request {
        Request::Capabilities {} => Ok(Response::Capabilities(backend.capabilities())),
        Request::LaunchPlan { request } => backend.launch_plan(&request).map(Response::LaunchPlan),
        Request::ResumePlan { request, session } => backend
            .resume_plan(&request, &session)
            .map(Response::ResumePlan),
        Request::Observe { event } => backend.observe(&event).map(Response::Observe),
        Request::StopPlan { session } => backend.stop_plan(&session).map(Response::StopPlan),
    })
}
