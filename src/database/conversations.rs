use anyhow::{Context, Result};

use crate::config::DEFAULT_ACCOUNT_ID;
use crate::database::rows::{conversation_message_from_row, thread_summary_from_row};
use crate::db::{ConversationMessage, Database, ThreadSummary};

const MAX_CONVERSATION_IDENTIFIER_CHARS: usize = 512;
const MAX_CONVERSATION_AGENT_NAME_CHARS: usize = 64;
const MAX_CONVERSATION_TITLE_CHARS: usize = 200;
const MAX_CONVERSATION_MESSAGE_CONTENT_CHARS: usize = 200_000;

pub struct ConversationMessageInsert<'a> {
    pub account_id: &'a str,
    pub message_id: &'a str,
    pub thread_id: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub agent_name: Option<&'a str>,
    pub agent_run_id: Option<&'a str>,
}

fn validate_conversation_char_limit(field: &str, value: &str, max_chars: usize) -> Result<()> {
    let char_count = value.chars().count();
    if char_count > max_chars {
        anyhow::bail!("{field} exceeds the {max_chars} character limit");
    }
    Ok(())
}

impl Database {
    pub async fn create_thread(
        &self,
        thread_id: &str,
        agent_name: &str,
        title: Option<&str>,
        context_email_id: Option<&str>,
    ) -> Result<()> {
        self.create_thread_for_account(
            DEFAULT_ACCOUNT_ID,
            thread_id,
            agent_name,
            title,
            context_email_id,
        )
        .await
    }

    fn normalize_required_conversation_identifier(field: &str, value: &str) -> Result<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            anyhow::bail!("{field} must not be empty");
        }
        validate_conversation_char_limit(field, trimmed, MAX_CONVERSATION_IDENTIFIER_CHARS)?;
        Ok(trimmed.to_string())
    }

    fn normalize_required_conversation_text(
        field: &str,
        value: &str,
        max_chars: usize,
    ) -> Result<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            anyhow::bail!("{field} must not be empty");
        }
        validate_conversation_char_limit(field, trimmed, max_chars)?;
        Ok(trimmed.to_string())
    }

    fn normalize_optional_conversation_text(
        field: &str,
        value: Option<&str>,
        max_chars: usize,
    ) -> Result<Option<String>> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                validate_conversation_char_limit(field, value, max_chars)?;
                Ok(value.to_string())
            })
            .transpose()
    }

    fn normalize_conversation_role(role: &str) -> Result<String> {
        let role = Self::normalize_required_conversation_text("role", role, 16)?;
        if matches!(role.as_str(), "user" | "agent" | "system") {
            return Ok(role);
        }
        anyhow::bail!("role must be one of: user, agent, system");
    }

    fn validate_conversation_content(content: &str) -> Result<()> {
        if content.trim().is_empty() {
            anyhow::bail!("content must not be empty");
        }
        validate_conversation_char_limit("content", content, MAX_CONVERSATION_MESSAGE_CONTENT_CHARS)
    }

    pub async fn create_thread_for_account(
        &self,
        account_id: &str,
        thread_id: &str,
        agent_name: &str,
        title: Option<&str>,
        context_email_id: Option<&str>,
    ) -> Result<()> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", account_id)?;
        let thread_id = Self::normalize_required_conversation_identifier("thread_id", thread_id)?;
        let agent_name = Self::normalize_required_conversation_text(
            "agent_name",
            agent_name,
            MAX_CONVERSATION_AGENT_NAME_CHARS,
        )?;
        let title = Self::normalize_optional_conversation_text(
            "title",
            title,
            MAX_CONVERSATION_TITLE_CHARS,
        )?;
        let context_email_id = Self::normalize_optional_conversation_text(
            "context_email_id",
            context_email_id,
            MAX_CONVERSATION_IDENTIFIER_CHARS,
        )?;

        sqlx::query(
            r#"
            INSERT INTO conversation_threads (
                account_id,
                thread_id,
                agent_name,
                title,
                context_email_id
            )
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .bind(&agent_name)
        .bind(title.as_deref())
        .bind(context_email_id.as_deref())
        .execute(&self.pool)
        .await
        .context("create_thread_for_account")?;
        Ok(())
    }

    pub async fn list_threads(&self, limit: usize, offset: usize) -> Result<Vec<ThreadSummary>> {
        self.list_threads_for_account(DEFAULT_ACCOUNT_ID, limit, offset)
            .await
    }

    pub async fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadSummary>> {
        self.get_thread_for_account(DEFAULT_ACCOUNT_ID, thread_id)
            .await
    }

    pub async fn get_thread_for_account(
        &self,
        account_id: &str,
        thread_id: &str,
    ) -> Result<Option<ThreadSummary>> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", account_id)?;
        let thread_id = Self::normalize_required_conversation_identifier("thread_id", thread_id)?;
        let row = sqlx::query(
            r#"
            SELECT
                t.thread_id,
                t.agent_name,
                t.title,
                t.context_email_id,
                COUNT(m.message_id) AS message_count,
                MAX(m.created_at) AS last_message_at,
                t.created_at
            FROM conversation_threads t
            LEFT JOIN conversation_messages m
              ON m.account_id = t.account_id
             AND m.thread_id = t.thread_id
            WHERE t.account_id = $1
              AND t.thread_id = $2
            GROUP BY t.account_id, t.thread_id
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .fetch_optional(&self.pool)
        .await
        .context("get_thread_for_account")?;

        Ok(row.as_ref().map(thread_summary_from_row))
    }

    pub async fn list_threads_for_account(
        &self,
        account_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ThreadSummary>> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", account_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
            SELECT
                t.thread_id,
                t.agent_name,
                t.title,
                t.context_email_id,
                COUNT(m.message_id) AS message_count,
                MAX(m.created_at) AS last_message_at,
                t.created_at
            FROM conversation_threads t
            LEFT JOIN conversation_messages m
              ON m.account_id = t.account_id
             AND m.thread_id = t.thread_id
            WHERE t.account_id = $1
            GROUP BY t.account_id, t.thread_id
            ORDER BY t.updated_at DESC, t.created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(&account_id)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .context("list_threads_for_account")?;

        Ok(rows.iter().map(thread_summary_from_row).collect())
    }

    pub async fn get_thread_messages(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>> {
        self.get_thread_messages_for_account(DEFAULT_ACCOUNT_ID, thread_id, limit)
            .await
    }

    pub async fn get_thread_messages_for_account(
        &self,
        account_id: &str,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", account_id)?;
        let thread_id = Self::normalize_required_conversation_identifier("thread_id", thread_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
            SELECT
                message_id,
                thread_id,
                role,
                content,
                agent_name,
                agent_run_id,
                created_at
            FROM (
                SELECT
                    message_id,
                    thread_id,
                    role,
                    content,
                    agent_name,
                    agent_run_id,
                    created_at
                FROM conversation_messages
                WHERE account_id = $1
                  AND thread_id = $2
                ORDER BY created_at DESC, message_id DESC
                LIMIT $3
            ) recent
            ORDER BY created_at ASC, message_id ASC
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("get_thread_messages_for_account")?;

        Ok(rows.iter().map(conversation_message_from_row).collect())
    }

    pub async fn add_message(
        &self,
        message_id: &str,
        thread_id: &str,
        role: &str,
        content: &str,
        agent_name: Option<&str>,
        agent_run_id: Option<&str>,
    ) -> Result<()> {
        self.add_message_for_account(ConversationMessageInsert {
            account_id: DEFAULT_ACCOUNT_ID,
            message_id,
            thread_id,
            role,
            content,
            agent_name,
            agent_run_id,
        })
        .await
    }

    pub async fn add_message_for_account(
        &self,
        message: ConversationMessageInsert<'_>,
    ) -> Result<()> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", message.account_id)?;
        let message_id =
            Self::normalize_required_conversation_identifier("message_id", message.message_id)?;
        let thread_id =
            Self::normalize_required_conversation_identifier("thread_id", message.thread_id)?;
        let role = Self::normalize_conversation_role(message.role)?;
        Self::validate_conversation_content(message.content)?;
        let agent_name = Self::normalize_optional_conversation_text(
            "agent_name",
            message.agent_name,
            MAX_CONVERSATION_AGENT_NAME_CHARS,
        )?;
        let agent_run_id = Self::normalize_optional_conversation_text(
            "agent_run_id",
            message.agent_run_id,
            MAX_CONVERSATION_IDENTIFIER_CHARS,
        )?;

        let mut tx = self
            .pool
            .begin()
            .await
            .context("add_message_for_account begin transaction")?;

        let thread_exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM conversation_threads
                WHERE account_id = $1
                  AND thread_id = $2
            )
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .fetch_one(&mut *tx)
        .await
        .context("add_message_for_account check_thread")?;

        if !thread_exists {
            anyhow::bail!("add_message_for_account: thread '{}' not found", thread_id);
        }

        sqlx::query(
            r#"
            INSERT INTO conversation_messages (
                account_id,
                message_id,
                thread_id,
                role,
                content,
                agent_name,
                agent_run_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(&account_id)
        .bind(&message_id)
        .bind(&thread_id)
        .bind(&role)
        .bind(message.content)
        .bind(agent_name.as_deref())
        .bind(agent_run_id.as_deref())
        .execute(&mut *tx)
        .await
        .context("add_message_for_account insert")?;

        sqlx::query(
            r#"
            UPDATE conversation_threads
            SET updated_at = NOW()
            WHERE account_id = $1
              AND thread_id = $2
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .execute(&mut *tx)
        .await
        .context("add_message_for_account update_thread")?;

        tx.commit()
            .await
            .context("add_message_for_account commit")?;
        Ok(())
    }

    pub async fn update_thread_title(&self, thread_id: &str, title: Option<&str>) -> Result<()> {
        self.update_thread_title_for_account(DEFAULT_ACCOUNT_ID, thread_id, title)
            .await
    }

    pub async fn update_thread_title_for_account(
        &self,
        account_id: &str,
        thread_id: &str,
        title: Option<&str>,
    ) -> Result<()> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", account_id)?;
        let thread_id = Self::normalize_required_conversation_identifier("thread_id", thread_id)?;
        let title = Self::normalize_optional_conversation_text(
            "title",
            title,
            MAX_CONVERSATION_TITLE_CHARS,
        )?;

        sqlx::query(
            r#"
            UPDATE conversation_threads
            SET title = $3,
                updated_at = NOW()
            WHERE account_id = $1
              AND thread_id = $2
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .bind(title.as_deref())
        .execute(&self.pool)
        .await
        .context("update_thread_title_for_account")?;
        Ok(())
    }

    pub async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        self.delete_thread_for_account(DEFAULT_ACCOUNT_ID, thread_id)
            .await
    }

    pub async fn delete_thread_for_account(&self, account_id: &str, thread_id: &str) -> Result<()> {
        let account_id =
            Self::normalize_required_conversation_identifier("account_id", account_id)?;
        let thread_id = Self::normalize_required_conversation_identifier("thread_id", thread_id)?;
        sqlx::query(
            r#"
            DELETE FROM conversation_threads
            WHERE account_id = $1
              AND thread_id = $2
            "#,
        )
        .bind(&account_id)
        .bind(&thread_id)
        .execute(&self.pool)
        .await
        .context("delete_thread_for_account")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_role_validation_rejects_invalid_roles() {
        let error = Database::normalize_conversation_role("assistant")
            .expect_err("invalid conversation role should fail");
        assert!(error
            .to_string()
            .contains("role must be one of: user, agent, system"));
    }

    #[test]
    fn conversation_identifier_validation_rejects_blank_values() {
        let error = Database::normalize_required_conversation_identifier("thread_id", "   ")
            .expect_err("blank identifier should fail");
        assert!(error.to_string().contains("thread_id must not be empty"));
    }

    #[test]
    fn conversation_optional_title_normalizes_blank_to_none() {
        let title = Database::normalize_optional_conversation_text(
            "title",
            Some("   "),
            MAX_CONVERSATION_TITLE_CHARS,
        )
        .expect("blank optional title should normalize");
        assert!(title.is_none());
    }
}
