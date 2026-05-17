#[path = "mailbox_tools.rs"]
pub mod agent_tools;
#[path = "confidence_calibration.rs"]
pub mod calibration;
#[path = "run_executor.rs"]
pub mod executor;
#[path = "agent_spec.rs"]
pub mod spec;
#[path = "scratchpad_state.rs"]
pub mod state;
#[path = "tool_registry.rs"]
pub mod tools;

pub use agent_tools::{
    build_analysis_tools, build_digest_tools, build_location_tools, build_mail_assistant_tools,
    build_orchestrator_tools,
};
pub use executor::{
    AgentHarness, HarnessEvent, HarnessEventCallback, RunResult, DB_COMPLETENESS_PENDING_STATUS,
};
pub use spec::{AgentSpec, ProviderTier};
pub use state::AgentState;
pub use tools::{AgentTool, EchoTool, ToolRegistry};

use std::sync::Arc;

use crate::ai::AIProvider;

pub fn resolve_provider(
    spec: &AgentSpec,
    local: Option<Arc<dyn AIProvider>>,
    frontier: Option<Arc<dyn AIProvider>>,
) -> Result<Arc<dyn AIProvider>, &'static str> {
    match spec.provider.tier {
        ProviderTier::Worker => local
            .or(frontier)
            .ok_or("no provider available for worker agent"),
        ProviderTier::Orchestrator => frontier
            .or(local)
            .ok_or("no provider available for orchestrator agent"),
    }
}
