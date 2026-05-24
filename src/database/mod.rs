//! Database facade for the MailSubsystem persistence layer.
//!
//! This module preserves the public `crate::db` API while the implementation
//! lives in focused modules under `src/database/`.

pub(crate) mod agent_runs;
pub(crate) mod analysis_batches;
pub(crate) mod body_sync_queue;
pub(crate) mod completeness;
pub(crate) mod config;
pub(crate) mod connection;
pub(crate) mod conversations;
pub(crate) mod core_work;
pub(crate) mod email_analysis;
pub(crate) mod email_ingest;
pub(crate) mod email_queries;
pub(crate) mod frontier_queue;
pub(crate) mod mailbox_sync;
pub(crate) mod records;
pub(crate) mod retrieval;
pub(crate) mod rows;
pub(crate) mod schema_management;
pub(crate) mod scratchpad;
pub(crate) mod subagent_state;

pub use agent_runs::{AgentRunDetail, AgentRunStats, AgentRunStatus, AgentRunSummary};
pub use body_sync_queue::{BodySyncQueueDepth, BodySyncQueueEntry, BodySyncQueueItem};
pub use completeness::DbCompletenessSnapshot;
pub use config::{DatabaseConfig, SchemaMigrationMode};
pub use connection::Database;
pub use conversations::ConversationMessageInsert;
pub use core_work::{CoreWorkQueueEntry, CoreWorkStatusItem, CoreWorkStatusSummary, CoreWorkType};
pub use email_analysis::{
    PendingLocationApply, UpdateAiFieldsInput, DEFAULT_ANALYSIS_LOCK_TTL_SECS,
};
pub use email_queries::{DigestWindowStats, EmailListFilters};
pub use frontier_queue::FrontierQueueEntry;
pub use mailbox_sync::{FolderSyncResult, ImapFolder, SystemFilingMoveRecord};
pub use records::{ConversationMessage, EmailRecord, StoreEmailInput, ThreadSummary};
pub use retrieval::{
    SenderIntentProfile, SimilarEmailKeywordQuery, SimilarEmailResult, SimilarEmailSearchHints,
    SimilarEmailVectorQuery, ThreadMessageRaw,
};
pub use scratchpad::{ScratchpadEntry, ScratchpadStats};
pub use subagent_state::{
    AssistantInsightInsert, SubagentResultRecord, SubagentSkillLessonRecord, SubagentTaskRecord,
};

#[cfg(test)]
mod integration_tests;
