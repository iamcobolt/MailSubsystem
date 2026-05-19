//! RAG context builder: thread summaries, sender history, folder list, thread raw content, hybrid similarity search.

use anyhow::Result;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::db::{
    Database, SenderIntentProfile, SimilarEmailKeywordQuery, SimilarEmailResult,
    SimilarEmailSearchHints as DbSimilarEmailSearchHints, SimilarEmailVectorQuery,
    ThreadMessageRaw,
};
use crate::embeddings::{truncate_for_embedding, EmbeddingProvider};

/// Max chars to embed (Gemini limit ~8k tokens, ~32k chars; use conservative 16k).
const EMBED_MAX_CHARS: usize = 16_384;
const HYBRID_CANDIDATE_MULTIPLIER: usize = 4;
const HYBRID_MAX_CANDIDATES: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct SimilarSearchHints<'a> {
    pub sender: Option<&'a str>,
    pub category: Option<&'a str>,
    pub email_type: Option<&'a str>,
    pub organization: Option<&'a str>,
    pub list_id: Option<&'a str>,
    pub exclude_message_id: Option<&'a str>,
}

fn to_db_hints<'a>(hints: &'a SimilarSearchHints<'a>) -> DbSimilarEmailSearchHints<'a> {
    DbSimilarEmailSearchHints {
        sender: hints.sender,
        category: hints.category,
        email_type: hints.email_type,
        organization: hints.organization,
        list_id: hints.list_id,
        exclude_message_id: hints.exclude_message_id,
    }
}

fn serialize_similar_results(results: &[SimilarEmailResult]) -> String {
    let arr: Vec<Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "message_id": r.message_id,
                "subject": r.subject,
                "sender": r.sender,
                "location": r.location,
                "human_summary": r.human_summary,
                "organization": r.organization,
                "list_id": r.list_id,
                "spam_status": r.spam_status,
                "marketing_status": r.marketing_status,
                "email_type": r.email_type,
                "category": r.category,
                "topic": r.topic,
                "distance": r.distance,
            })
        })
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Context assembled for AI analysis (RAG).
pub struct RAGContext {
    pub thread_summaries: Vec<Value>,
    pub sender_history: Vec<Value>,
    pub sender_intent_profile: Option<SenderIntentProfile>,
    pub existing_folders: Vec<String>,
    /// Raw content (subject, sender, body) of related/thread messages for the model to use.
    pub thread_raw_messages: Vec<ThreadMessageRaw>,
    /// Similar emails from hybrid retrieval (semantic + lexical).
    pub similar_emails: Vec<SimilarEmailResult>,
}

pub struct RAGContextBuilder {
    pub database: Arc<Database>,
    /// Optional embedding provider for semantic leg of hybrid retrieval.
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
}

fn hybrid_candidate_limit(limit: usize) -> usize {
    let base = limit.max(1).saturating_mul(HYBRID_CANDIDATE_MULTIPLIER);
    base.min(HYBRID_MAX_CANDIDATES).max(limit)
}

fn fuse_hybrid_results(
    semantic: Vec<SimilarEmailResult>,
    lexical: Vec<SimilarEmailResult>,
    limit: usize,
) -> Vec<SimilarEmailResult> {
    if limit == 0 {
        return Vec::new();
    }
    if semantic.is_empty() {
        return lexical.into_iter().take(limit).collect();
    }
    if lexical.is_empty() {
        return semantic.into_iter().take(limit).collect();
    }

    // Reciprocal Rank Fusion (RRF): stable blend of semantic and lexical rankings.
    const RRF_K: f64 = 60.0;
    const WEIGHT_SEMANTIC: f64 = 1.0;
    const WEIGHT_LEXICAL: f64 = 0.9;

    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut records: HashMap<String, SimilarEmailResult> = HashMap::new();

    for (rank, row) in semantic.into_iter().enumerate() {
        let key = row.message_id.clone();
        let entry = scores.entry(key.clone()).or_insert(0.0);
        *entry += WEIGHT_SEMANTIC / (RRF_K + (rank + 1) as f64);
        records.entry(key).or_insert(row);
    }

    for (rank, row) in lexical.into_iter().enumerate() {
        let key = row.message_id.clone();
        let entry = scores.entry(key.clone()).or_insert(0.0);
        *entry += WEIGHT_LEXICAL / (RRF_K + (rank + 1) as f64);
        records.entry(key).or_insert(row);
    }

    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    ranked.sort_by(|(id_a, score_a), (id_b, score_b)| {
        score_b
            .partial_cmp(score_a)
            .unwrap_or(Ordering::Equal)
            .then_with(|| id_a.cmp(id_b))
    });

    let mut fused = Vec::with_capacity(limit);
    for (id, _) in ranked.into_iter() {
        if let Some(row) = records.remove(&id) {
            fused.push(row);
            if fused.len() >= limit {
                break;
            }
        }
    }
    fused
}

impl RAGContextBuilder {
    pub fn new(database: Arc<Database>) -> Self {
        Self {
            database,
            embedder: None,
        }
    }

    pub fn with_embedder(database: Arc<Database>, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            database,
            embedder: Some(embedder),
        }
    }

    /// Build initial context for single-pass analysis.
    pub async fn build_initial_context(
        &self,
        account_id: &str,
        related_message_ids: &[String],
        sender: Option<&str>,
        query_text: Option<&str>,
        hints: SimilarSearchHints<'_>,
    ) -> Result<RAGContext> {
        let thread_summaries = match self
            .database
            .get_thread_summaries_for_account(account_id, related_message_ids, 5)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_thread_summaries_error error={}", err);
                Vec::new()
            }
        };
        let sender_history = match self
            .database
            .get_sender_history_for_account_excluding(
                account_id,
                sender,
                5,
                hints.exclude_message_id,
            )
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_sender_history_error error={}", err);
                Vec::new()
            }
        };
        let sender_intent_profile = match self
            .database
            .get_sender_intent_profile_for_account(account_id, sender, 50)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_sender_intent_profile_error error={}", err);
                None
            }
        };
        let existing_folders = match self
            .database
            .get_existing_folders_for_account(account_id)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_existing_folders_error error={}", err);
                Vec::new()
            }
        };
        let thread_raw_messages = match self
            .database
            .get_thread_raw_content_for_account(account_id, related_message_ids, 10)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_thread_raw_error error={}", err);
                Vec::new()
            }
        };
        let similar_emails = match self
            .search_similar_with_threshold(account_id, query_text, 6, 0.35, &hints)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_similar_search_error error={}", err);
                Vec::new()
            }
        };
        Ok(RAGContext {
            thread_summaries,
            sender_history,
            sender_intent_profile,
            existing_folders,
            thread_raw_messages,
            similar_emails,
        })
    }

    /// Execute a RAG tool call (for iterative/thinking analysis).
    pub async fn execute_tool_call(
        &self,
        account_id: &str,
        tool_name: &str,
        params: Value,
    ) -> Result<String> {
        match tool_name {
            "get_thread_context" => {
                let message_ids: Vec<String> = params
                    .get("message_ids")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let include_body = params
                    .get("include_full_body")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.get_full_thread_context(account_id, &message_ids, include_body)
                    .await
            }
            "get_sender_history" => {
                let sender = params.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(50) as usize;
                let exclude_message_id = params.get("exclude_message_id").and_then(|v| v.as_str());
                self.get_sender_history_extended(account_id, sender, limit, exclude_message_id)
                    .await
            }
            "search_similar_emails" => {
                let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(10) as usize;
                let max_distance = params
                    .get("max_distance")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.35);
                let hints = SimilarSearchHints {
                    sender: params.get("sender").and_then(|v| v.as_str()),
                    category: params.get("category").and_then(|v| v.as_str()),
                    email_type: params.get("email_type").and_then(|v| v.as_str()),
                    organization: params.get("organization").and_then(|v| v.as_str()),
                    list_id: params.get("list_id").and_then(|v| v.as_str()),
                    exclude_message_id: params.get("exclude_message_id").and_then(|v| v.as_str()),
                };
                self.search_similar(account_id, query, limit, max_distance, &hints)
                    .await
            }
            _ => anyhow::bail!("Unknown tool: {}", tool_name),
        }
    }

    async fn get_full_thread_context(
        &self,
        account_id: &str,
        message_ids: &[String],
        include_body: bool,
    ) -> Result<String> {
        if include_body {
            let raws = self
                .database
                .get_thread_raw_content_for_account(account_id, message_ids, 50)
                .await?;
            return Ok(serde_json::to_string(&raws).unwrap_or_else(|_| "[]".to_string()));
        }
        let summaries = self
            .database
            .get_thread_summaries_for_account(account_id, message_ids, 50)
            .await?;
        Ok(serde_json::to_string(&summaries).unwrap_or_else(|_| "[]".to_string()))
    }

    async fn get_sender_history_extended(
        &self,
        account_id: &str,
        sender: &str,
        limit: usize,
        exclude_message_id: Option<&str>,
    ) -> Result<String> {
        let history = self
            .database
            .get_sender_history_for_account_excluding(
                account_id,
                Some(sender),
                limit,
                exclude_message_id,
            )
            .await?;
        Ok(serde_json::to_string(&history).unwrap_or_else(|_| "[]".to_string()))
    }

    async fn search_similar(
        &self,
        account_id: &str,
        query: &str,
        limit: usize,
        max_distance: f64,
        hints: &SimilarSearchHints<'_>,
    ) -> Result<String> {
        if query.trim().is_empty() {
            return Ok("[]".to_string());
        }
        let results = self
            .search_similar_hybrid(account_id, query, limit, max_distance, hints)
            .await?;
        Ok(serialize_similar_results(&results))
    }

    async fn search_similar_with_threshold(
        &self,
        account_id: &str,
        query_text: Option<&str>,
        limit: usize,
        max_distance: f64,
        hints: &SimilarSearchHints<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        let query = match query_text {
            Some(q) if !q.trim().is_empty() => q,
            _ => return Ok(Vec::new()),
        };
        self.search_similar_hybrid(account_id, query, limit, max_distance, hints)
            .await
    }

    async fn search_similar_hybrid(
        &self,
        account_id: &str,
        query: &str,
        limit: usize,
        max_distance: f64,
        hints: &SimilarSearchHints<'_>,
    ) -> Result<Vec<SimilarEmailResult>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let truncated = truncate_for_embedding(query, EMBED_MAX_CHARS);
        let candidate_limit = hybrid_candidate_limit(limit);

        let lexical = match self
            .database
            .search_similar_emails_by_keywords_with_hints_for_account(
                account_id,
                SimilarEmailKeywordQuery {
                    query_text: &truncated,
                    limit: candidate_limit,
                    hints: to_db_hints(hints),
                },
            )
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::warn!("rag_lexical_search_error error={}", err);
                Vec::new()
            }
        };

        let semantic = if let Some(embedder) = &self.embedder {
            match embedder.embed(&truncated).await {
                Ok(embedding) => match self
                    .database
                    .search_similar_emails_by_vector_with_threshold_and_hints_for_account(
                        account_id,
                        SimilarEmailVectorQuery {
                            embedding: &embedding,
                            limit: candidate_limit,
                            max_distance,
                            hints: to_db_hints(hints),
                        },
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(err) => {
                        log::warn!("rag_semantic_search_error error={}", err);
                        Vec::new()
                    }
                },
                Err(err) => {
                    log::warn!("rag_semantic_embed_error error={}", err);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        Ok(fuse_hybrid_results(semantic, lexical, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result(id: &str, distance: f64) -> SimilarEmailResult {
        SimilarEmailResult {
            message_id: id.to_string(),
            subject: None,
            sender: None,
            location: None,
            human_summary: None,
            organization: None,
            list_id: None,
            spam_status: None,
            marketing_status: None,
            email_type: None,
            category: None,
            topic: None,
            distance,
        }
    }

    #[test]
    fn fuse_hybrid_results_dedupes_and_respects_limit() {
        let semantic = vec![
            sample_result("a", 0.10),
            sample_result("b", 0.12),
            sample_result("c", 0.14),
        ];
        let lexical = vec![
            sample_result("b", 0.60),
            sample_result("d", 0.62),
            sample_result("a", 0.64),
        ];
        let fused = fuse_hybrid_results(semantic, lexical, 3);
        assert_eq!(fused.len(), 3);
        let ids: Vec<&str> = fused.iter().map(|r| r.message_id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"d") || ids.contains(&"c"));
    }

    #[test]
    fn fuse_hybrid_results_falls_back_when_semantic_empty() {
        let lexical = vec![sample_result("x", 0.70), sample_result("y", 0.72)];
        let fused = fuse_hybrid_results(Vec::new(), lexical, 1);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].message_id, "x");
    }

    #[test]
    fn serialize_similar_results_includes_location() {
        let mut result = sample_result("x", 0.10);
        result.location = Some("Work/LinkedIn".to_string());

        let payload: serde_json::Value =
            serde_json::from_str(&serialize_similar_results(&[result])).expect("json payload");

        assert_eq!(payload[0]["location"], "Work/LinkedIn");
    }

    #[tokio::test]
    async fn test_execute_tool_call_unknown_tool() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("Skipping RAG unknown-tool test (no DATABASE_URL)");
            return;
        };
        let db = crate::db::Database::new(&url).await.expect("connect");
        let builder = RAGContextBuilder::new(Arc::new(db));
        let err = builder
            .execute_tool_call(
                crate::config::DEFAULT_ACCOUNT_ID,
                "unknown_tool",
                serde_json::json!({}),
            )
            .await;
        assert!(err.is_err(), "expected Err for unknown tool");
        assert!(
            err.unwrap_err().to_string().contains("Unknown tool"),
            "error message should mention unknown tool"
        );
    }
}
