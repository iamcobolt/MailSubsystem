use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTier {
    Worker,
    Orchestrator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCatalogEntry {
    pub id: String,
    pub label: String,
    pub description: String,
    pub tier: AgentTier,
    pub is_default: bool,
    #[serde(default)]
    pub advanced_only: bool,
    #[serde(default)]
    pub sort_order: i32,
}

impl AgentCatalogEntry {
    fn new(
        id: &'static str,
        label: &'static str,
        description: &'static str,
        tier: AgentTier,
        is_default: bool,
        advanced_only: bool,
        sort_order: i32,
    ) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            tier,
            is_default,
            advanced_only,
            sort_order,
        }
    }
}

pub const DEFAULT_AGENT_ID: &str = "mail-assistant";

fn sort_catalog_entries(agents: &mut [AgentCatalogEntry]) {
    agents.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| left.label.cmp(&right.label))
    });
}

pub fn builtin_agents() -> Vec<AgentCatalogEntry> {
    let mut agents = vec![
        AgentCatalogEntry::new(
            "mail-assistant",
            "Mail Assistant",
            "Primary conversational front door for mailbox questions, summaries, and organization guidance.",
            AgentTier::Worker,
            true,
            false,
            0,
        ),
        AgentCatalogEntry::new(
            "email-analyzer",
            "Email Analyzer",
            "Classifies one email, summarizes it, and flags security risk before automation acts.",
            AgentTier::Worker,
            false,
            true,
            100,
        ),
        AgentCatalogEntry::new(
            "location-agent",
            "Location Agent",
            "Picks the most appropriate IMAP folder for a message using the account's live folder map.",
            AgentTier::Worker,
            false,
            true,
            110,
        ),
        AgentCatalogEntry::new(
            "orchestrator",
            "Orchestrator",
            "Supervises higher-judgment work, escalations, and batch-level quality decisions.",
            AgentTier::Orchestrator,
            false,
            true,
            120,
        ),
        AgentCatalogEntry::new(
            "digest-agent",
            "Digest Agent",
            "Builds structured daily or weekly inbox digests with trends, senders, and actions.",
            AgentTier::Worker,
            false,
            true,
            130,
        ),
        AgentCatalogEntry::new(
            "folder-consolidator",
            "Folder Consolidator",
            "Finds redundant folders and proposes safer consolidation patterns across the mailbox.",
            AgentTier::Worker,
            false,
            true,
            140,
        ),
    ];
    sort_catalog_entries(&mut agents);
    agents
}

pub fn visible_agents(catalog: &[AgentCatalogEntry]) -> Vec<AgentCatalogEntry> {
    let mut agents: Vec<_> = catalog
        .iter()
        .filter(|agent| !agent.advanced_only)
        .cloned()
        .collect();
    sort_catalog_entries(&mut agents);
    agents
}

#[cfg(test)]
pub fn advanced_agents(catalog: &[AgentCatalogEntry]) -> Vec<AgentCatalogEntry> {
    let mut agents: Vec<_> = catalog
        .iter()
        .filter(|agent| agent.advanced_only)
        .cloned()
        .collect();
    sort_catalog_entries(&mut agents);
    agents
}

pub fn user_facing_agents(allow_advanced: bool) -> Vec<AgentCatalogEntry> {
    let catalog = builtin_agents();
    if allow_advanced {
        catalog
    } else {
        visible_agents(&catalog)
    }
}

pub fn direct_subagent_chat_enabled() -> bool {
    std::env::var("MAIL_ASSISTANT_DIRECT_SUBAGENTS")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn find_agent(agent_id: &str) -> Option<AgentCatalogEntry> {
    let normalized = agent_id.trim();
    builtin_agents()
        .into_iter()
        .find(|agent| agent.id.eq_ignore_ascii_case(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_one_default_agent() {
        let defaults = builtin_agents()
            .into_iter()
            .filter(|agent| agent.is_default)
            .count();
        assert_eq!(defaults, 1);
    }

    #[test]
    fn find_agent_matches_case_insensitively() {
        let agent = find_agent("MAIL-ASSISTANT").expect("find builtin agent");
        assert_eq!(agent.id, DEFAULT_AGENT_ID);
        assert!(agent.is_default);
        assert!(!agent.advanced_only);
    }

    #[test]
    fn visible_agents_hide_advanced_specialists() {
        let catalog = builtin_agents();
        let visible = visible_agents(&catalog);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, DEFAULT_AGENT_ID);
    }

    #[test]
    fn advanced_agents_return_specialists_in_sort_order() {
        let catalog = builtin_agents();
        let advanced = advanced_agents(&catalog);
        assert!(advanced.iter().all(|agent| agent.advanced_only));
        assert_eq!(advanced[0].id, "email-analyzer");
        assert_eq!(
            advanced.last().map(|agent| agent.id.as_str()),
            Some("folder-consolidator")
        );
    }

    #[test]
    fn user_facing_agents_hide_advanced_specialists_by_default() {
        let visible = user_facing_agents(false);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, DEFAULT_AGENT_ID);
    }

    #[test]
    fn user_facing_agents_can_include_advanced_for_dev_mode() {
        let all = user_facing_agents(true);
        assert!(all.iter().any(|agent| agent.id == DEFAULT_AGENT_ID));
        assert!(all.iter().any(|agent| agent.advanced_only));
    }
}
