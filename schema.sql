-- ============================================================================
-- MailSubsystem canonical database schema
-- ============================================================================
-- This file is the single source of truth for MailSubsystem database DDL.
-- It is embedded in the binary and applied by `mailsubsystem migrate-schema --apply`.
-- Setup: psql -U your_user -d mailsubsystem -f schema.sql
-- ============================================================================

-- pgvector extension for semantic search (vector embeddings)
-- Install: brew install pgvector (macOS) or https://github.com/pgvector/pgvector
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS emails (
    account_id TEXT NOT NULL DEFAULT 'default',
    message_id TEXT NOT NULL,
    subject TEXT,
    sender TEXT,
    received_date TIMESTAMPTZ,
    spam_status TEXT CHECK (spam_status IN ('spam', 'not-spam') OR spam_status IS NULL),
    phishing_status TEXT CHECK (phishing_status IN ('phishing', 'not-phishing') OR phishing_status IS NULL),
    marketing_status TEXT CHECK (marketing_status IN ('marketing', 'not-marketing') OR marketing_status IS NULL),
    otp_status TEXT CHECK (otp_status IN ('otp', 'magic_link', 'password_reset', 'not_otp') OR otp_status IS NULL),
    otp_expires TIMESTAMPTZ,
    uid INTEGER,
    uid_validity INTEGER,
    ai_summary JSONB,
    human_summary TEXT,
    category TEXT CHECK (category IN ('personal', 'work', 'volunteering', 'financial', 'shopping', 'social', 'travel', 'health', 'education') OR category IS NULL),
    subcategory TEXT,
    organization TEXT,
    topic TEXT,
    email_type TEXT CHECK (email_type IN ('newsletter', 'announcement', 'notification', 'actionable', 'conversation', 'transactional', 'receipt', 'reference') OR email_type IS NULL),
    location TEXT,
    recipients_to TEXT[] DEFAULT '{}',
    recipients_cc TEXT[] DEFAULT '{}',
    recipients_bcc TEXT[] DEFAULT '{}',
    location_recommendation TEXT,
    offer_expires TIMESTAMPTZ,
    related_message_ids TEXT[] DEFAULT '{}',
    list_unsubscribe TEXT,
    list_id TEXT,
    x_priority TEXT,
    return_path TEXT,
    reply_to TEXT,
    custom_headers JSONB,
    message_size INTEGER,
    message_tokens INTEGER,
    analyzed_by TEXT,
    raw_email_content TEXT,
    body_text TEXT,
    body_synced_at TIMESTAMPTZ,
    is_read BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (account_id, message_id)
);

-- Optional columns for existing DBs (no-op if already present)
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS human_summary TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS otp_status TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS otp_expires TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS uid INTEGER;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS uid_validity INTEGER;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS raw_email_content TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS body_text TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS body_synced_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS is_read BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS recipients_to TEXT[] DEFAULT '{}';
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS recipients_cc TEXT[] DEFAULT '{}';
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS recipients_bcc TEXT[] DEFAULT '{}';
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS list_unsubscribe TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS list_id TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS x_priority TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS return_path TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS reply_to TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS custom_headers JSONB;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS message_size INTEGER;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS message_tokens INTEGER;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS organization TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS location_recommendation TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS location_create_if_missing BOOLEAN;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS offer_expires TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS deleted_from_server_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS batch_id TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS reanalysis_requested BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS reanalysis_reason TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS last_filed_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS last_filed_by TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS last_user_moved_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS move_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS user_pinned_folder TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS filing_lock_until TIMESTAMPTZ;

-- Embedding for semantic search (dimensions determined by embedding model at runtime)
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS embedding vector;

-- Normalize spam_status, phishing_status, marketing_status: drop checks, update, re-add checks.
-- (Update must run without checks so any existing variant values can be normalized.)
DO $$
BEGIN
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_spam_status_check;
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_phishing_status_check;
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_marketing_status_check;
END $$;

UPDATE emails
SET
    spam_status     = CASE WHEN spam_status IN ('unknown') THEN NULL WHEN spam_status = 'not_spam' THEN 'not-spam' ELSE spam_status END,
    phishing_status = CASE WHEN phishing_status IN ('unknown') THEN NULL WHEN phishing_status = 'not_phishing' THEN 'not-phishing' ELSE phishing_status END,
    marketing_status = CASE WHEN marketing_status IN ('unknown') THEN NULL WHEN marketing_status = 'not_marketing' THEN 'not-marketing' ELSE marketing_status END
WHERE spam_status IN ('unknown', 'not_spam')
   OR phishing_status IN ('unknown', 'not_phishing')
   OR marketing_status IN ('unknown', 'not_marketing');

DO $$
BEGIN
    ALTER TABLE emails ADD CONSTRAINT emails_spam_status_check
        CHECK (spam_status IN ('spam', 'not-spam') OR spam_status IS NULL);
    ALTER TABLE emails ADD CONSTRAINT emails_phishing_status_check
        CHECK (phishing_status IN ('phishing', 'not-phishing') OR phishing_status IS NULL);
    ALTER TABLE emails ADD CONSTRAINT emails_marketing_status_check
        CHECK (marketing_status IN ('marketing', 'not-marketing') OR marketing_status IS NULL);
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

-- Normalize otp_status: allow NULL
DO $$
BEGIN
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_otp_status_check;
    ALTER TABLE emails ADD CONSTRAINT emails_otp_status_check
        CHECK (otp_status IN ('otp', 'magic_link', 'password_reset', 'not_otp') OR otp_status IS NULL);
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

UPDATE emails SET otp_status = NULL WHERE otp_status = 'unknown';

-- Broaden category and email_type constraints for existing DBs
DO $$
BEGIN
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_category_check;
    ALTER TABLE emails ADD CONSTRAINT emails_category_check
        CHECK (category IN ('personal', 'work', 'volunteering', 'financial', 'shopping', 'social', 'travel', 'health', 'education') OR category IS NULL);
EXCEPTION WHEN OTHERS THEN NULL;
END $$;
DO $$
BEGIN
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_email_type_check;
    ALTER TABLE emails ADD CONSTRAINT emails_email_type_check
        CHECK (email_type IN ('newsletter', 'announcement', 'notification', 'actionable', 'conversation', 'transactional', 'receipt', 'reference') OR email_type IS NULL);
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

-- Indexes
CREATE INDEX IF NOT EXISTS idx_emails_category ON emails(category);
CREATE INDEX IF NOT EXISTS idx_emails_subcategory ON emails(subcategory);
CREATE INDEX IF NOT EXISTS idx_emails_spam_status ON emails(spam_status);
CREATE INDEX IF NOT EXISTS idx_emails_phishing_status ON emails(phishing_status);
CREATE INDEX IF NOT EXISTS idx_emails_marketing_status ON emails(marketing_status);
CREATE INDEX IF NOT EXISTS idx_emails_sender ON emails(sender);
CREATE INDEX IF NOT EXISTS idx_emails_received_date ON emails(received_date);
CREATE INDEX IF NOT EXISTS idx_emails_location ON emails(location);
CREATE INDEX IF NOT EXISTS idx_emails_account_location ON emails(account_id, location);
CREATE INDEX IF NOT EXISTS idx_emails_offer_expires ON emails(offer_expires);
CREATE INDEX IF NOT EXISTS idx_emails_organization ON emails(organization);
CREATE INDEX IF NOT EXISTS idx_emails_topic ON emails(topic);
CREATE INDEX IF NOT EXISTS idx_emails_email_type ON emails(email_type);
CREATE INDEX IF NOT EXISTS idx_emails_otp_status ON emails(otp_status);
CREATE INDEX IF NOT EXISTS idx_emails_otp_expires ON emails(otp_expires);
CREATE INDEX IF NOT EXISTS idx_emails_related_message_ids ON emails USING GIN(related_message_ids);
CREATE INDEX IF NOT EXISTS idx_emails_uid_location ON emails(account_id, location, uid, uid_validity) WHERE uid IS NOT NULL AND uid_validity IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_emails_updated_at ON emails(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_emails_body_synced_at ON emails(body_synced_at) WHERE body_text IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_emails_account_received_date ON emails(account_id, received_date DESC);
CREATE INDEX IF NOT EXISTS idx_emails_deleted_from_server_at ON emails(account_id, deleted_from_server_at) WHERE deleted_from_server_at IS NULL;

-- HNSW index for cosine similarity search (semantic / RAG).
-- Fresh installs start with `embedding vector` (no fixed dimensions), which cannot
-- accept a vector_cosine_ops HNSW index until a model pins the column to vector(N).
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_attribute
        WHERE attrelid = 'emails'::regclass
          AND attname = 'embedding'
          AND atttypmod > 0
    ) THEN
        EXECUTE 'CREATE INDEX IF NOT EXISTS idx_emails_embedding_cosine
                 ON emails USING hnsw (embedding vector_cosine_ops)
                 WHERE embedding IS NOT NULL';
    END IF;
END $$;

-- GIN expression index for hybrid RAG lexical retrieval.
CREATE INDEX IF NOT EXISTS idx_emails_rag_hybrid_fts_gin
ON emails
USING GIN ((
  setweight(
    to_tsvector(
      'english',
      regexp_replace(COALESCE(subject, ''), '[^[:space:]]{128,}', ' ', 'g')
    ),
    'A'
  ) ||
  setweight(
    to_tsvector(
      'english',
      regexp_replace(COALESCE(sender, ''), '[^[:space:]]{128,}', ' ', 'g')
    ),
    'A'
  ) ||
  setweight(
    to_tsvector(
      'english',
      regexp_replace(COALESCE(human_summary, ''), '[^[:space:]]{128,}', ' ', 'g')
    ),
    'B'
  ) ||
  setweight(
    to_tsvector(
      'english',
      regexp_replace(
        COALESCE(category, '') || ' ' ||
        COALESCE(subcategory, '') || ' ' ||
        COALESCE(organization, '') || ' ' ||
        COALESCE(topic, '') || ' ' ||
        COALESCE(email_type, '') || ' ' ||
        COALESCE(list_id, ''),
        '[^[:space:]]{128,}',
        ' ',
        'g'
      )
    ),
    'B'
  ) ||
  setweight(
    to_tsvector(
      'english',
      LEFT(
        regexp_replace(
          COALESCE(
            NULLIF(TRIM(COALESCE(body_text, '')), ''),
            regexp_replace(
              LEFT(COALESCE(raw_email_content, ''), 12000),
              '[^[:alnum:]@._-]+',
              ' ',
              'g'
            ),
            ''
          ),
          '[^[:space:]]{128,}',
          ' ',
          'g'
        ),
        2000
      )
    ),
    'C'
  )
))
WHERE ai_summary IS NOT NULL;

-- Speeds selection of newest rows needing embeddings in backfill.
CREATE INDEX IF NOT EXISTS idx_emails_embedding_backfill_received_date
ON emails (received_date DESC)
WHERE embedding IS NULL
  AND (
    (body_text IS NOT NULL AND TRIM(body_text) <> '')
    OR (raw_email_content IS NOT NULL AND LENGTH(raw_email_content) > 100)
  );

-- updated_at trigger
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS update_emails_updated_at ON emails;
CREATE TRIGGER update_emails_updated_at BEFORE UPDATE ON emails
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- ============================================================================
-- IMAP folders: list of mailboxes from IMAP LIST (and optionally LSUB)
-- ============================================================================

CREATE TABLE IF NOT EXISTS imap_folders (
    account_id TEXT NOT NULL DEFAULT 'default',
    folder_name TEXT NOT NULL,
    delimiter TEXT,
    is_noselect BOOLEAN NOT NULL DEFAULT false,
    attributes TEXT[] DEFAULT '{}',
    last_listed_at TIMESTAMPTZ,
    last_synced_uid INTEGER,
    uid_validity INTEGER,
    message_count INTEGER,
    priority INTEGER,
    highest_modseq BIGINT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (account_id, folder_name)
);

-- Optional columns for existing DBs
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS last_synced_uid INTEGER;
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS last_full_sync_uid INTEGER;
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS uid_validity INTEGER;
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS message_count INTEGER;
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS priority INTEGER;
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS highest_modseq BIGINT;
ALTER TABLE IF EXISTS imap_folders ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

DROP TRIGGER IF EXISTS update_imap_folders_updated_at ON imap_folders;
CREATE TRIGGER update_imap_folders_updated_at BEFORE UPDATE ON imap_folders
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Emails missing Message-ID: failure tracking for review (envelope sync bails when encountered)
CREATE TABLE IF NOT EXISTS emails_missing_message_id (
    account_id TEXT NOT NULL DEFAULT 'default',
    folder_name TEXT NOT NULL,
    uid INTEGER NOT NULL,
    uid_validity INTEGER NOT NULL,
    attempted_at TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (account_id, folder_name, uid, uid_validity)
);
ALTER TABLE IF EXISTS emails_missing_message_id ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

-- Frontier analysis queue: message_ids that need frontier (re)analysis after low-confidence local result.
CREATE TABLE IF NOT EXISTS frontier_analysis_queue (
    account_id TEXT NOT NULL DEFAULT 'default',
    message_id TEXT NOT NULL,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_at TIMESTAMPTZ,
    worker_id TEXT,
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, message_id),
    FOREIGN KEY (account_id, message_id) REFERENCES emails(account_id, message_id) ON DELETE CASCADE,
    CHECK (status IN ('pending', 'processing', 'failed', 'dead'))
);

-- Optional columns for existing DBs
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS available_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS locked_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS worker_id TEXT;
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS last_error TEXT;
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS frontier_analysis_queue ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

UPDATE frontier_analysis_queue
SET
    status = COALESCE(status, 'pending'),
    attempt_count = COALESCE(attempt_count, 0),
    available_at = COALESCE(available_at, NOW()),
    updated_at = COALESCE(updated_at, NOW());

DO $$
BEGIN
    ALTER TABLE frontier_analysis_queue DROP CONSTRAINT IF EXISTS frontier_analysis_queue_status_check;
    ALTER TABLE frontier_analysis_queue ADD CONSTRAINT frontier_analysis_queue_status_check
        CHECK (status IN ('pending', 'processing', 'failed', 'dead'));
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

-- Durable queue for full-body IMAP sync jobs.
CREATE TABLE IF NOT EXISTS body_sync_queue (
    id BIGSERIAL PRIMARY KEY,
    account_id TEXT NOT NULL DEFAULT 'default',
    folder_name TEXT NOT NULL,
    uid INTEGER NOT NULL,
    uid_validity INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_at TIMESTAMPTZ,
    worker_id TEXT,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(account_id, folder_name, uid, uid_validity, message_id),
    FOREIGN KEY (account_id, message_id) REFERENCES emails(account_id, message_id) ON DELETE CASCADE,
    CHECK (status IN ('pending', 'processing', 'done', 'failed', 'dead'))
);

-- Sync window runs: track last run timestamps per window size (days). window_days=0 => full sync.
CREATE TABLE IF NOT EXISTS sync_window_runs (
    account_id TEXT NOT NULL DEFAULT 'default',
    window_days INTEGER NOT NULL,
    PRIMARY KEY (account_id, window_days),
    last_run_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE IF EXISTS body_sync_queue ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE IF EXISTS sync_window_runs ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

-- ============================================================================
-- Phase 5L: account-scoped core pipeline migration
-- ============================================================================

UPDATE emails SET account_id = COALESCE(account_id, 'default');
UPDATE imap_folders SET account_id = COALESCE(account_id, 'default');
UPDATE emails_missing_message_id SET account_id = COALESCE(account_id, 'default');
UPDATE frontier_analysis_queue SET account_id = COALESCE(account_id, 'default');
UPDATE body_sync_queue SET account_id = COALESCE(account_id, 'default');
UPDATE sync_window_runs SET account_id = COALESCE(account_id, 'default');

ALTER TABLE emails ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE imap_folders ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE emails_missing_message_id ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE frontier_analysis_queue ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE body_sync_queue ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE sync_window_runs ALTER COLUMN account_id SET NOT NULL;

ALTER TABLE IF EXISTS frontier_analysis_queue
    DROP CONSTRAINT IF EXISTS frontier_analysis_queue_message_id_fkey;
ALTER TABLE IF EXISTS body_sync_queue
    DROP CONSTRAINT IF EXISTS body_sync_queue_message_id_fkey;
ALTER TABLE IF EXISTS otp_codes
    DROP CONSTRAINT IF EXISTS otp_codes_message_id_fkey;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint c
        WHERE c.conrelid = 'emails'::regclass
          AND c.contype IN ('p', 'u')
          AND ARRAY(
                SELECT a.attname::text
                FROM unnest(c.conkey) WITH ORDINALITY AS cols(attnum, ord)
                JOIN pg_attribute a
                  ON a.attrelid = c.conrelid
                 AND a.attnum = cols.attnum
                ORDER BY cols.ord
          ) = ARRAY['account_id', 'message_id']::text[]
    ) THEN
        ALTER TABLE emails
            ADD CONSTRAINT emails_account_id_message_id_key
            UNIQUE (account_id, message_id);
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint c
        WHERE c.conrelid = 'imap_folders'::regclass
          AND c.contype IN ('p', 'u')
          AND ARRAY(
                SELECT a.attname::text
                FROM unnest(c.conkey) WITH ORDINALITY AS cols(attnum, ord)
                JOIN pg_attribute a
                  ON a.attrelid = c.conrelid
                 AND a.attnum = cols.attnum
                ORDER BY cols.ord
          ) = ARRAY['account_id', 'folder_name']::text[]
    ) THEN
        ALTER TABLE imap_folders
            ADD CONSTRAINT imap_folders_account_id_folder_name_key
            UNIQUE (account_id, folder_name);
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint c
        WHERE c.conrelid = 'emails_missing_message_id'::regclass
          AND c.contype IN ('p', 'u')
          AND ARRAY(
                SELECT a.attname::text
                FROM unnest(c.conkey) WITH ORDINALITY AS cols(attnum, ord)
                JOIN pg_attribute a
                  ON a.attrelid = c.conrelid
                 AND a.attnum = cols.attnum
                ORDER BY cols.ord
          ) = ARRAY['account_id', 'folder_name', 'uid', 'uid_validity']::text[]
    ) THEN
        ALTER TABLE emails_missing_message_id
            ADD CONSTRAINT emails_missing_message_id_account_scope_key
            UNIQUE (account_id, folder_name, uid, uid_validity);
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint c
        WHERE c.conrelid = 'frontier_analysis_queue'::regclass
          AND c.contype IN ('p', 'u')
          AND ARRAY(
                SELECT a.attname::text
                FROM unnest(c.conkey) WITH ORDINALITY AS cols(attnum, ord)
                JOIN pg_attribute a
                  ON a.attrelid = c.conrelid
                 AND a.attnum = cols.attnum
                ORDER BY cols.ord
          ) = ARRAY['account_id', 'message_id']::text[]
    ) THEN
        ALTER TABLE frontier_analysis_queue
            ADD CONSTRAINT frontier_analysis_queue_account_id_message_id_key
            UNIQUE (account_id, message_id);
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint c
        WHERE c.conrelid = 'sync_window_runs'::regclass
          AND c.contype IN ('p', 'u')
          AND ARRAY(
                SELECT a.attname::text
                FROM unnest(c.conkey) WITH ORDINALITY AS cols(attnum, ord)
                JOIN pg_attribute a
                  ON a.attrelid = c.conrelid
                 AND a.attnum = cols.attnum
                ORDER BY cols.ord
          ) = ARRAY['account_id', 'window_days']::text[]
    ) THEN
        ALTER TABLE sync_window_runs
            ADD CONSTRAINT sync_window_runs_account_id_window_days_key
            UNIQUE (account_id, window_days);
    END IF;
END $$;

ALTER TABLE IF EXISTS body_sync_queue DROP CONSTRAINT IF EXISTS body_sync_queue_folder_name_uid_uid_validity_message_id_key;
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint c
        WHERE c.conrelid = 'body_sync_queue'::regclass
          AND c.contype IN ('p', 'u')
          AND ARRAY(
                SELECT a.attname::text
                FROM unnest(c.conkey) WITH ORDINALITY AS cols(attnum, ord)
                JOIN pg_attribute a
                  ON a.attrelid = c.conrelid
                 AND a.attnum = cols.attnum
                ORDER BY cols.ord
          ) = ARRAY['account_id', 'folder_name', 'uid', 'uid_validity', 'message_id']::text[]
    ) AND to_regclass('body_sync_queue_account_folder_uid_validity_message_key') IS NULL THEN
        ALTER TABLE body_sync_queue
            ADD CONSTRAINT body_sync_queue_account_folder_uid_validity_message_key
            UNIQUE (account_id, folder_name, uid, uid_validity, message_id);
    END IF;
END $$;

ALTER TABLE IF EXISTS frontier_analysis_queue
    DROP CONSTRAINT IF EXISTS frontier_analysis_queue_account_id_message_id_fkey;
ALTER TABLE IF EXISTS body_sync_queue
    DROP CONSTRAINT IF EXISTS body_sync_queue_account_id_message_id_fkey;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'frontier_analysis_queue_account_id_message_id_fkey'
          AND conrelid = 'frontier_analysis_queue'::regclass
    ) THEN
        ALTER TABLE frontier_analysis_queue
            ADD CONSTRAINT frontier_analysis_queue_account_id_message_id_fkey
            FOREIGN KEY (account_id, message_id)
            REFERENCES emails(account_id, message_id)
            ON DELETE CASCADE;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'body_sync_queue_account_id_message_id_fkey'
          AND conrelid = 'body_sync_queue'::regclass
    ) THEN
        ALTER TABLE body_sync_queue
            ADD CONSTRAINT body_sync_queue_account_id_message_id_fkey
            FOREIGN KEY (account_id, message_id)
            REFERENCES emails(account_id, message_id)
            ON DELETE CASCADE;
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_frontier_analysis_queue_enqueued_at ON frontier_analysis_queue(account_id, enqueued_at);
CREATE INDEX IF NOT EXISTS idx_frontier_analysis_queue_status_available_at ON frontier_analysis_queue(account_id, status, available_at);
CREATE INDEX IF NOT EXISTS idx_frontier_analysis_queue_status_locked_at ON frontier_analysis_queue(account_id, status, locked_at);
CREATE INDEX IF NOT EXISTS idx_body_sync_queue_status_available_at ON body_sync_queue(account_id, status, available_at);
CREATE INDEX IF NOT EXISTS idx_body_sync_queue_status_locked_at ON body_sync_queue(account_id, status, locked_at);
CREATE INDEX IF NOT EXISTS idx_body_sync_queue_account_message ON body_sync_queue(account_id, message_id);

-- Core runtime durable work queue.
CREATE TABLE IF NOT EXISTS core_work_queue (
    id BIGSERIAL PRIMARY KEY,
    account_id TEXT NOT NULL DEFAULT 'default',
    work_type TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    available_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_at TIMESTAMPTZ,
    lease_expires_at TIMESTAMPTZ,
    worker_id TEXT,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    CONSTRAINT core_work_queue_account_work_idempotency_key UNIQUE(account_id, work_type, idempotency_key),
    CHECK (work_type IN ('sync_full', 'sync_incremental', 'sync_body', 'analyze', 'embed', 'locate', 'file_preview', 'file_apply', 'assistant_heartbeat', 'subagent_task')),
    CHECK (status IN ('pending', 'processing', 'done', 'failed', 'dead'))
);

ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS work_type TEXT NOT NULL DEFAULT 'sync_incremental';
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS idempotency_key TEXT NOT NULL DEFAULT 'default';
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS payload JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS max_attempts INTEGER NOT NULL DEFAULT 3;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS available_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS locked_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS worker_id TEXT;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS last_error TEXT;
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS core_work_queue ADD COLUMN IF NOT EXISTS completed_at TIMESTAMPTZ;

DO $$
BEGIN
    ALTER TABLE core_work_queue DROP CONSTRAINT IF EXISTS core_work_queue_work_type_check;
    ALTER TABLE core_work_queue ADD CONSTRAINT core_work_queue_work_type_check
        CHECK (work_type IN ('sync_full', 'sync_incremental', 'sync_body', 'analyze', 'embed', 'locate', 'file_preview', 'file_apply', 'assistant_heartbeat', 'subagent_task'));
    ALTER TABLE core_work_queue DROP CONSTRAINT IF EXISTS core_work_queue_status_check;
    ALTER TABLE core_work_queue ADD CONSTRAINT core_work_queue_status_check
        CHECK (status IN ('pending', 'processing', 'done', 'failed', 'dead'));
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'core_work_queue_account_work_idempotency_key'
          AND conrelid = 'core_work_queue'::regclass
    ) THEN
        ALTER TABLE core_work_queue
            ADD CONSTRAINT core_work_queue_account_work_idempotency_key
            UNIQUE (account_id, work_type, idempotency_key);
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_core_work_queue_status_available_at ON core_work_queue(account_id, status, available_at);
CREATE INDEX IF NOT EXISTS idx_core_work_queue_status_locked_at ON core_work_queue(account_id, status, locked_at);
CREATE INDEX IF NOT EXISTS idx_core_work_queue_status_lease_expires_at ON core_work_queue(account_id, status, lease_expires_at);
CREATE INDEX IF NOT EXISTS idx_core_work_queue_work_type_status ON core_work_queue(account_id, work_type, status);

-- Mail Assistant multi-agent runtime: quiet insights, ephemeral sub-agent work, and filing policy history.
CREATE TABLE IF NOT EXISTS assistant_insights (
    id BIGSERIAL PRIMARY KEY,
    account_id TEXT NOT NULL DEFAULT 'default',
    insight_type TEXT NOT NULL,
    severity TEXT NOT NULL DEFAULT 'info',
    message TEXT NOT NULL,
    related_message_id TEXT,
    related_folder TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ,
    CHECK (severity IN ('info', 'warning', 'critical')),
    CHECK (status IN ('open', 'acknowledged', 'resolved', 'dismissed'))
);

CREATE TABLE IF NOT EXISTS subagent_tasks (
    task_id TEXT NOT NULL,
    account_id TEXT NOT NULL DEFAULT 'default',
    task_kind TEXT NOT NULL,
    worker_name TEXT NOT NULL,
    skill_bundle TEXT NOT NULL,
    message_ids TEXT[] NOT NULL DEFAULT '{}',
    input_context JSONB NOT NULL DEFAULT '{}'::jsonb,
    priority INTEGER NOT NULL DEFAULT 0,
    correlation_id TEXT NOT NULL,
    created_by TEXT NOT NULL DEFAULT 'mail-assistant',
    status TEXT NOT NULL DEFAULT 'pending',
    core_work_id BIGINT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, task_id),
    CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled'))
);

CREATE TABLE IF NOT EXISTS subagent_results (
    result_id BIGSERIAL PRIMARY KEY,
    account_id TEXT NOT NULL DEFAULT 'default',
    task_id TEXT NOT NULL,
    worker_name TEXT NOT NULL,
    task_kind TEXT NOT NULL,
    result_json JSONB NOT NULL,
    confidence REAL,
    evidence JSONB NOT NULL DEFAULT '[]'::jsonb,
    recommended_actions JSONB NOT NULL DEFAULT '[]'::jsonb,
    requires_review BOOLEAN NOT NULL DEFAULT FALSE,
    agent_run_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (account_id, task_id)
        REFERENCES subagent_tasks(account_id, task_id)
        ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS subagent_skill_lessons (
    account_id TEXT NOT NULL DEFAULT 'default',
    skill_bundle TEXT NOT NULL,
    lesson_key TEXT NOT NULL,
    lesson_type TEXT NOT NULL DEFAULT 'strategy',
    status TEXT NOT NULL DEFAULT 'candidate',
    summary TEXT NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::jsonb,
    score REAL,
    support_count INTEGER NOT NULL DEFAULT 1,
    negative_count INTEGER NOT NULL DEFAULT 0,
    source_task_id TEXT,
    source_result_id BIGINT,
    source_run_id TEXT,
    worker_name TEXT,
    agent_spec_version TEXT,
    promoted_at TIMESTAMPTZ,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ,
    last_rejected_at TIMESTAMPTZ,
    rejection_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, skill_bundle, lesson_key),
    CHECK (lesson_type IN ('strategy', 'tool_gap', 'failure_pattern', 'safety_rule')),
    CHECK (status IN ('candidate', 'active', 'paused', 'superseded', 'discarded'))
);

ALTER TABLE IF EXISTS subagent_skill_lessons ALTER COLUMN status SET DEFAULT 'candidate';
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS negative_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS source_task_id TEXT;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS source_result_id BIGINT;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS source_run_id TEXT;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS worker_name TEXT;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS agent_spec_version TEXT;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS promoted_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS last_rejected_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS subagent_skill_lessons ADD COLUMN IF NOT EXISTS rejection_reason TEXT;

DO $$
BEGIN
    ALTER TABLE subagent_skill_lessons DROP CONSTRAINT IF EXISTS subagent_skill_lessons_lesson_type_check;
    ALTER TABLE subagent_skill_lessons ADD CONSTRAINT subagent_skill_lessons_lesson_type_check
        CHECK (lesson_type IN ('strategy', 'tool_gap', 'failure_pattern', 'safety_rule'));
    ALTER TABLE subagent_skill_lessons DROP CONSTRAINT IF EXISTS subagent_skill_lessons_status_check;
    ALTER TABLE subagent_skill_lessons ADD CONSTRAINT subagent_skill_lessons_status_check
        CHECK (status IN ('candidate', 'active', 'paused', 'superseded', 'discarded'));
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

CREATE TABLE IF NOT EXISTS email_location_events (
    id BIGSERIAL PRIMARY KEY,
    account_id TEXT NOT NULL DEFAULT 'default',
    message_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT NOT NULL,
    from_folder TEXT,
    to_folder TEXT,
    reason TEXT,
    confidence REAL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (account_id, message_id)
        REFERENCES emails(account_id, message_id)
        ON DELETE CASCADE,
    CHECK (event_type IN ('system_recommended', 'system_moved', 'user_moved', 'user_reverted', 'assistant_challenge', 'recommendation_changed')),
    CHECK (actor IN ('user', 'core', 'mail-assistant', 'subagent', 'system'))
);

CREATE TABLE IF NOT EXISTS folder_learning_rules (
    id BIGSERIAL PRIMARY KEY,
    account_id TEXT NOT NULL DEFAULT 'default',
    scope_type TEXT NOT NULL,
    scope_value TEXT NOT NULL,
    preferred_folder TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.5,
    support_count INTEGER NOT NULL DEFAULT 1,
    conflict_count INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT 'user_move',
    status TEXT NOT NULL DEFAULT 'active',
    last_observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(account_id, scope_type, scope_value, preferred_folder),
    CHECK (scope_type IN ('sender', 'domain', 'list_id', 'organization')),
    CHECK (status IN ('active', 'paused', 'rejected'))
);

CREATE INDEX IF NOT EXISTS idx_assistant_insights_account_status
ON assistant_insights(account_id, status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_subagent_tasks_account_status
ON subagent_tasks(account_id, status, priority DESC, created_at ASC);
CREATE INDEX IF NOT EXISTS idx_subagent_tasks_correlation
ON subagent_tasks(account_id, correlation_id);
CREATE INDEX IF NOT EXISTS idx_subagent_results_task
ON subagent_results(account_id, task_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_subagent_skill_lessons_active
ON subagent_skill_lessons(account_id, skill_bundle, status, support_count DESC, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_subagent_skill_lessons_candidates
ON subagent_skill_lessons(account_id, status, support_count DESC, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_email_location_events_message
ON email_location_events(account_id, message_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_folder_learning_rules_lookup
ON folder_learning_rules(account_id, scope_type, scope_value, status);
CREATE INDEX IF NOT EXISTS idx_emails_filing_policy
ON emails(account_id, filing_lock_until, user_pinned_folder)
WHERE filing_lock_until IS NOT NULL OR user_pinned_folder IS NOT NULL;

-- System metadata: key-value store for tracking embedding model, dimensions, etc.
CREATE TABLE IF NOT EXISTS system_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

-- Upsert folders from a JSON array (e.g. from IMAP LIST response).
-- payload: [{"name":"INBOX","delimiter":"/","is_noselect":false,"attributes":["HasNoChildren"]}, ...]
CREATE OR REPLACE FUNCTION upsert_imap_folders_from_list(v_account_id TEXT, payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    elem JSONB;
    v_name TEXT;
    v_delimiter TEXT;
    v_is_noselect BOOLEAN;
    v_attributes TEXT[];
    v_now TIMESTAMPTZ := NOW();
BEGIN
    FOR elem IN SELECT * FROM jsonb_array_elements(payload)
    LOOP
        v_name := elem->>'name';
        IF v_name IS NULL OR v_name = '' THEN
            CONTINUE;
        END IF;
        v_delimiter := NULLIF(trim(elem->>'delimiter'), '');
        v_is_noselect := COALESCE((elem->>'is_noselect')::boolean, false);
        v_attributes := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'attributes')),
            '{}'
        );
        INSERT INTO imap_folders (
            account_id, folder_name, delimiter, is_noselect, attributes, last_listed_at, updated_at
        )
        VALUES (v_account_id, v_name, v_delimiter, v_is_noselect, v_attributes, v_now, v_now)
        ON CONFLICT (account_id, folder_name) DO UPDATE SET
            delimiter = EXCLUDED.delimiter,
            is_noselect = EXCLUDED.is_noselect,
            attributes = EXCLUDED.attributes,
            last_listed_at = EXCLUDED.last_listed_at,
            updated_at = NOW();
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION upsert_imap_folders_from_list(payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM upsert_imap_folders_from_list('default', payload);
END;
$$;

-- ============================================================================
-- Phase 3: durable analysis lifecycle and action dispatch
-- ============================================================================

ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS otp_code TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS threat_level TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS threat_indicators JSONB;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS analyzed_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS action_status TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS action_applied_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS analysis_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS analysis_failed_at TIMESTAMPTZ;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS analysis_permanent_failure BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS last_analysis_error TEXT;
ALTER TABLE IF EXISTS emails ADD COLUMN IF NOT EXISTS analyzed_by TEXT;

DO $$
BEGIN
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_threat_level_check;
    ALTER TABLE emails ADD CONSTRAINT emails_threat_level_check
        CHECK (threat_level IN ('none', 'low', 'medium', 'high', 'critical') OR threat_level IS NULL);
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

DO $$
BEGIN
    ALTER TABLE emails DROP CONSTRAINT IF EXISTS emails_action_status_check;
    ALTER TABLE emails ADD CONSTRAINT emails_action_status_check
        CHECK (action_status IN ('trashed', 'junked', 'otp_stored', 'none') OR action_status IS NULL);
EXCEPTION WHEN OTHERS THEN NULL;
END $$;

-- Backfill analyzed_at for rows that were already classified before Phase 3 existed.
UPDATE emails
SET analyzed_at = COALESCE(analyzed_at, updated_at, created_at, NOW())
WHERE analyzed_at IS NULL
  AND (
      ai_summary IS NOT NULL
      OR human_summary IS NOT NULL
      OR category IS NOT NULL
      OR subcategory IS NOT NULL
      OR organization IS NOT NULL
      OR topic IS NOT NULL
      OR email_type IS NOT NULL
      OR otp_status IS NOT NULL
      OR threat_level IS NOT NULL
  );

CREATE INDEX IF NOT EXISTS idx_emails_analyzed_at ON emails(analyzed_at DESC) WHERE analyzed_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_emails_action_pending ON emails(received_date DESC) WHERE analyzed_at IS NOT NULL AND action_status IS NULL;
CREATE INDEX IF NOT EXISTS idx_emails_analysis_retry ON emails(analysis_permanent_failure, analysis_failed_at, analysis_attempts) WHERE analyzed_at IS NULL;

CREATE TABLE IF NOT EXISTS otp_codes (
    id          BIGSERIAL PRIMARY KEY,
    account_id  TEXT NOT NULL DEFAULT 'default',
    message_id  TEXT NOT NULL,
    code        TEXT NOT NULL,
    expires_at  TIMESTAMPTZ,
    stored_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (account_id, message_id) REFERENCES emails(account_id, message_id) ON DELETE CASCADE
);

ALTER TABLE IF EXISTS otp_codes ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';
UPDATE otp_codes SET account_id = COALESCE(account_id, 'default');
ALTER TABLE otp_codes ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE IF EXISTS otp_codes DROP CONSTRAINT IF EXISTS otp_codes_account_id_message_id_fkey;
ALTER TABLE IF EXISTS otp_codes
    ADD CONSTRAINT otp_codes_account_id_message_id_fkey
    FOREIGN KEY (account_id, message_id) REFERENCES emails(account_id, message_id) ON DELETE CASCADE;
CREATE INDEX IF NOT EXISTS idx_otp_codes_message_id ON otp_codes(account_id, message_id);
CREATE INDEX IF NOT EXISTS idx_otp_codes_expires_at ON otp_codes(expires_at);

-- Recompute imap_folders.priority from folder name and message_count.
-- INBOX (or similar) -> 10; archive/junk/trash/deleted (or similar) -> 1;
-- all other selectable folders -> 2..9 by message_count (more messages = higher priority).
CREATE OR REPLACE FUNCTION recompute_imap_folder_priority(v_account_id TEXT)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    total_other int;
BEGIN
    -- INBOX-like: priority 10
    UPDATE imap_folders
    SET priority = 10, updated_at = NOW()
    WHERE account_id = v_account_id
      AND LOWER(TRIM(folder_name)) = 'inbox';

    -- Low-priority: archive, junk, trash, deleted (and common name variants)
    UPDATE imap_folders
    SET priority = 1, updated_at = NOW()
    WHERE account_id = v_account_id
      AND (priority IS NULL OR priority NOT IN (10))
      AND (
          LOWER(TRIM(folder_name)) IN (
              'archive', 'junk', 'junk e-mail', 'junk email',
              'trash', 'deleted messages', 'deleted', 'bin', 'deleted items'
          )
          OR LOWER(folder_name) LIKE '%trash%'
          OR LOWER(folder_name) LIKE '%deleted%'
          OR LOWER(folder_name) LIKE '%junk%'
          OR (LOWER(folder_name) LIKE '%archive%' AND LOWER(TRIM(folder_name)) <> 'inbox')
      );

    -- Remaining folders (and those not yet set): 2..9 by message_count
    WITH other AS (
        SELECT folder_name,
               COALESCE(message_count, 0) AS mc,
               ROW_NUMBER() OVER (ORDER BY COALESCE(message_count, 0) DESC, folder_name) AS rn,
               COUNT(*) OVER () AS cnt
        FROM imap_folders
        WHERE account_id = v_account_id
          AND (priority IS NULL OR (priority <> 10 AND priority <> 1))
    )
    UPDATE imap_folders f
    SET priority = LEAST(9, GREATEST(2, 2 + (7 * (o.cnt - o.rn) / NULLIF(GREATEST(o.cnt - 1, 1), 0)))), updated_at = NOW()
    FROM other o
    WHERE f.account_id = v_account_id
      AND f.folder_name = o.folder_name;
END;
$$;

CREATE OR REPLACE FUNCTION recompute_imap_folder_priority()
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM recompute_imap_folder_priority('default');
END;
$$;

-- Update message_count for folders from IMAP EXISTS. Payload: [{"folder_name":"INBOX","message_count":42}, ...]
CREATE OR REPLACE FUNCTION update_imap_folders_message_counts(v_account_id TEXT, payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    elem JSONB;
    v_name TEXT;
    v_count INTEGER;
BEGIN
    FOR elem IN SELECT * FROM jsonb_array_elements(payload)
    LOOP
        v_name := NULLIF(trim(elem->>'folder_name'), '');
        v_count := (elem->>'message_count')::integer;
        IF v_name IS NULL THEN
            CONTINUE;
        END IF;
        UPDATE imap_folders
        SET message_count = v_count, updated_at = NOW()
        WHERE account_id = v_account_id
          AND folder_name = v_name;
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION update_imap_folders_message_counts(payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM update_imap_folders_message_counts('default', payload);
END;
$$;

-- ============================================================================
-- Agent harness state
-- ============================================================================

CREATE TABLE IF NOT EXISTS agent_runs (
    run_id        TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL DEFAULT 'default',
    agent_name    TEXT NOT NULL,
    agent_version TEXT,
    task_id       TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'running',
    steps         INTEGER NOT NULL DEFAULT 0,
    llm_calls     INTEGER NOT NULL DEFAULT 0,
    tool_calls    INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER,
    output_tokens INTEGER,
    result        JSONB,
    error         TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at   TIMESTAMPTZ,
    duration_ms   INTEGER
);
ALTER TABLE IF EXISTS agent_runs ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

CREATE TABLE IF NOT EXISTS agent_state (
    account_id TEXT NOT NULL DEFAULT 'default',
    agent_name TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    PRIMARY KEY (account_id, agent_name, key)
);
ALTER TABLE IF EXISTS agent_state ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

CREATE TABLE IF NOT EXISTS agent_checkpoints (
    run_id     TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE CASCADE,
    step       INTEGER NOT NULL,
    messages   JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (run_id, step)
);

CREATE TABLE IF NOT EXISTS agent_tool_log (
    id         BIGSERIAL PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE CASCADE,
    step       INTEGER NOT NULL,
    tool_name  TEXT NOT NULL,
    arguments  JSONB,
    result     TEXT,
    latency_ms INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_agent_runs_account ON agent_runs(account_id, agent_name);
CREATE INDEX IF NOT EXISTS idx_agent_runs_task_id ON agent_runs(account_id, task_id);
CREATE INDEX IF NOT EXISTS idx_agent_runs_status ON agent_runs(status);
CREATE INDEX IF NOT EXISTS idx_agent_state_account ON agent_state(account_id, agent_name);
CREATE INDEX IF NOT EXISTS idx_agent_state_expires ON agent_state(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_agent_tool_log_run ON agent_tool_log(run_id);

-- ============================================================================
-- Conversation threads for Phase 8 agent chat
-- ============================================================================

CREATE TABLE IF NOT EXISTS conversation_threads (
    thread_id        TEXT NOT NULL,
    account_id       TEXT NOT NULL DEFAULT 'default',
    agent_name       TEXT NOT NULL,
    title            TEXT,
    context_email_id TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, thread_id)
);

CREATE TABLE IF NOT EXISTS conversation_messages (
    message_id   TEXT NOT NULL,
    thread_id    TEXT NOT NULL,
    account_id   TEXT NOT NULL DEFAULT 'default',
    role         TEXT NOT NULL CHECK (role IN ('user', 'agent', 'system')),
    content      TEXT NOT NULL,
    agent_name   TEXT,
    agent_run_id TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, message_id),
    FOREIGN KEY (account_id, thread_id)
        REFERENCES conversation_threads(account_id, thread_id)
        ON DELETE CASCADE
);

-- Optional columns for existing conversation tables from earlier Phase 8 iterations.
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS thread_id TEXT;
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS agent_name TEXT;
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS title TEXT;
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS context_email_id TEXT;
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE IF EXISTS conversation_threads ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS message_id TEXT;
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS thread_id TEXT;
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS role TEXT;
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS content TEXT;
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS agent_name TEXT;
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS agent_run_id TEXT;
ALTER TABLE IF EXISTS conversation_messages ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

UPDATE conversation_threads
SET
    account_id = COALESCE(account_id, 'default'),
    created_at = COALESCE(created_at, NOW()),
    updated_at = COALESCE(updated_at, created_at, NOW());

UPDATE conversation_messages
SET
    account_id = COALESCE(account_id, 'default'),
    created_at = COALESCE(created_at, NOW());

ALTER TABLE IF EXISTS conversation_threads ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE IF EXISTS conversation_messages ALTER COLUMN account_id SET NOT NULL;

CREATE INDEX IF NOT EXISTS idx_threads_account_updated
ON conversation_threads(account_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_messages_thread
ON conversation_messages(account_id, thread_id, created_at ASC);

-- ============================================================================
-- Batch tracking for optional orchestrator flow
-- ============================================================================

CREATE TABLE IF NOT EXISTS analysis_batches (
    batch_id             TEXT NOT NULL,
    account_id           TEXT NOT NULL DEFAULT 'default',
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at         TIMESTAMPTZ,
    email_count          INTEGER NOT NULL DEFAULT 0,
    status               TEXT NOT NULL DEFAULT 'pending',
    orchestrator_plan    JSONB,
    orchestrator_review  JSONB,
    quality_score        REAL,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, batch_id),
    CHECK (status IN ('pending', 'planning', 'processing', 'reviewing', 'completed', 'failed'))
);
ALTER TABLE IF EXISTS analysis_batches ADD COLUMN IF NOT EXISTS account_id TEXT NOT NULL DEFAULT 'default';

CREATE INDEX IF NOT EXISTS idx_analysis_batches_account_status
ON analysis_batches(account_id, status);

CREATE INDEX IF NOT EXISTS idx_analysis_batches_account_created
ON analysis_batches(account_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_emails_batch_id
ON emails(account_id, batch_id)
WHERE batch_id IS NOT NULL;

-- Phase 4: agent run escalation tracking
ALTER TABLE agent_runs ADD COLUMN IF NOT EXISTS escalated BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX IF NOT EXISTS idx_agent_runs_escalated
ON agent_runs(agent_name, escalated)
WHERE escalated = TRUE;

-- COB-57: per-worker confidence tracking for calibration
ALTER TABLE agent_runs ADD COLUMN IF NOT EXISTS output_confidence REAL;
CREATE INDEX IF NOT EXISTS idx_agent_runs_confidence
ON agent_runs(account_id, agent_name, output_confidence)
WHERE output_confidence IS NOT NULL AND status = 'completed';

-- Backfill output_confidence from result JSONB for existing completed runs
UPDATE agent_runs
SET output_confidence = (result->>'confidence')::real
WHERE status = 'completed'
  AND result IS NOT NULL
  AND result->>'confidence' IS NOT NULL
  AND output_confidence IS NULL;

-- Phase 4: scratchpad query support index already exists as idx_agent_state_account (Phase 1).

-- ============================================================================
-- Email import: upsert emails from IMAP fetch (message_id key; merge/diff-friendly)
-- ============================================================================
-- Payload: JSONB array of objects. Each object must have "message_id".
-- Optional keys: location, uid, uid_validity, subject, sender, received_date,
--   raw_email_content, body_text, is_read, recipients_to, recipients_cc, recipients_bcc,
--   list_unsubscribe, list_id, x_priority, return_path, reply_to, custom_headers,
--   message_size, message_tokens (computed from body_text/raw_email_content ~ chars/4), related_message_ids.
-- Omitted keys leave existing row values unchanged on update.
-- When body_text or raw_email_content is provided, body_synced_at is set to NOW().
-- Large content (raw_email_content, body_text) is fine; procedure breaks the array into per-email rows.
CREATE OR REPLACE FUNCTION upsert_emails_from_import(v_account_id TEXT, payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    elem JSONB;
    v_message_id TEXT;
    v_location TEXT;
    v_uid INTEGER;
    v_uid_validity INTEGER;
    v_subject TEXT;
    v_sender TEXT;
    v_received_date TIMESTAMPTZ;
    v_raw_email_content TEXT;
    v_body_text TEXT;
    v_is_read BOOLEAN;
    v_body_synced_at TIMESTAMPTZ;
    v_recipients_to TEXT[];
    v_recipients_cc TEXT[];
    v_recipients_bcc TEXT[];
    v_list_unsubscribe TEXT;
    v_list_id TEXT;
    v_x_priority TEXT;
    v_return_path TEXT;
    v_reply_to TEXT;
    v_custom_headers JSONB;
    v_message_size INTEGER;
    v_message_tokens INTEGER;
    v_related_message_ids TEXT[];
    v_domain TEXT;
BEGIN
    FOR elem IN SELECT * FROM jsonb_array_elements(payload)
    LOOP
        v_message_id := NULLIF(trim(elem->>'message_id'), '');
        IF v_message_id IS NULL THEN
            CONTINUE;
        END IF;

        v_location := NULLIF(trim(elem->>'location'), '');
        v_uid := (elem->>'uid')::integer;
        v_uid_validity := (elem->>'uid_validity')::integer;
        v_subject := NULLIF(trim(elem->>'subject'), '');
        v_sender := NULLIF(trim(elem->>'sender'), '');
        v_received_date := (elem->>'received_date')::timestamptz;
        v_raw_email_content := elem->>'raw_email_content';
        v_body_text := elem->>'body_text';
        v_is_read := (elem->>'is_read')::boolean;
        v_recipients_to := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'recipients_to')),
            NULL
        );
        v_recipients_cc := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'recipients_cc')),
            NULL
        );
        v_recipients_bcc := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'recipients_bcc')),
            NULL
        );
        v_body_synced_at := CASE
            WHEN elem ? 'body_text' AND (elem->>'body_text') IS NOT NULL THEN NOW()
            WHEN elem ? 'raw_email_content' AND (elem->>'raw_email_content') IS NOT NULL THEN NOW()
            ELSE NULL
        END;
        v_list_unsubscribe := NULLIF(trim(elem->>'list_unsubscribe'), '');
        v_list_id := NULLIF(trim(elem->>'list_id'), '');
        v_x_priority := NULLIF(trim(elem->>'x_priority'), '');
        v_return_path := NULLIF(trim(elem->>'return_path'), '');
        v_reply_to := NULLIF(trim(elem->>'reply_to'), '');
        v_custom_headers := elem->'custom_headers';
        v_message_size := (elem->>'message_size')::integer;
        -- Token estimate: ~chars/4 (common heuristic for English/LLM tokenizers)
        v_message_tokens := CASE
            WHEN v_body_text IS NOT NULL AND length(v_body_text) > 0 THEN ceil(length(v_body_text)::numeric / 4)::integer
            WHEN v_raw_email_content IS NOT NULL AND length(v_raw_email_content) > 0 THEN ceil(length(v_raw_email_content)::numeric / 4)::integer
            ELSE NULL
        END;
        v_related_message_ids := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'related_message_ids')),
            NULL
        );

        IF v_location IS NOT NULL THEN
            INSERT INTO email_location_events (
                account_id, message_id, event_type, actor, from_folder, to_folder,
                reason, confidence, metadata
            )
            SELECT
                v_account_id,
                v_message_id,
                'user_moved',
                'user',
                location,
                v_location,
                'observed_imap_location_change',
                1.0,
                jsonb_build_object('source', 'upsert_emails_from_import')
            FROM emails
            WHERE account_id = v_account_id
              AND message_id = v_message_id
              AND location IS NOT NULL
              AND LOWER(location) <> LOWER(v_location);

            IF FOUND THEN
                IF v_sender IS NOT NULL AND trim(v_sender) <> '' THEN
                    INSERT INTO folder_learning_rules (
                        account_id, scope_type, scope_value, preferred_folder,
                        confidence, support_count, source, last_observed_at, updated_at
                    )
                    VALUES (
                        v_account_id, 'sender', LOWER(trim(v_sender)), v_location,
                        0.65, 1, 'user_move', NOW(), NOW()
                    )
                    ON CONFLICT (account_id, scope_type, scope_value, preferred_folder) DO UPDATE SET
                        support_count = folder_learning_rules.support_count + 1,
                        confidence = LEAST(0.95, folder_learning_rules.confidence + 0.05),
                        last_observed_at = NOW(),
                        updated_at = NOW();

                    v_domain := NULLIF(LOWER(regexp_replace(split_part(v_sender, '@', 2), '[^a-zA-Z0-9.-].*$', '')), '');
                    IF v_domain IS NOT NULL THEN
                        INSERT INTO folder_learning_rules (
                            account_id, scope_type, scope_value, preferred_folder,
                            confidence, support_count, source, last_observed_at, updated_at
                        )
                        VALUES (
                            v_account_id, 'domain', v_domain, v_location,
                            0.55, 1, 'user_move', NOW(), NOW()
                        )
                        ON CONFLICT (account_id, scope_type, scope_value, preferred_folder) DO UPDATE SET
                            support_count = folder_learning_rules.support_count + 1,
                            confidence = LEAST(0.9, folder_learning_rules.confidence + 0.03),
                            last_observed_at = NOW(),
                            updated_at = NOW();
                    END IF;
                END IF;

                IF v_list_id IS NOT NULL THEN
                    INSERT INTO folder_learning_rules (
                        account_id, scope_type, scope_value, preferred_folder,
                        confidence, support_count, source, last_observed_at, updated_at
                    )
                    VALUES (
                        v_account_id, 'list_id', LOWER(trim(v_list_id)), v_location,
                        0.7, 1, 'user_move', NOW(), NOW()
                    )
                    ON CONFLICT (account_id, scope_type, scope_value, preferred_folder) DO UPDATE SET
                        support_count = folder_learning_rules.support_count + 1,
                        confidence = LEAST(0.97, folder_learning_rules.confidence + 0.05),
                        last_observed_at = NOW(),
                        updated_at = NOW();
                END IF;
            END IF;
        END IF;

        -- Explicitly set status columns to NULL so CHECK constraints are satisfied (no default 'unknown').
        INSERT INTO emails (
            account_id,
            message_id,
            location, uid, uid_validity,
            subject, sender, received_date,
            raw_email_content, body_text, body_synced_at,
            recipients_to, recipients_cc, recipients_bcc,
            is_read,
            spam_status, phishing_status, marketing_status, otp_status,
            list_unsubscribe, list_id, x_priority,
            return_path, reply_to, custom_headers, message_size, message_tokens,
            related_message_ids,
            updated_at
        )
        VALUES (
            v_account_id,
            v_message_id,
            v_location, v_uid, v_uid_validity,
            v_subject, v_sender, v_received_date,
            v_raw_email_content, v_body_text, v_body_synced_at,
            COALESCE(v_recipients_to, '{}'),
            COALESCE(v_recipients_cc, '{}'),
            COALESCE(v_recipients_bcc, '{}'),
            COALESCE(v_is_read, false),
            NULL, NULL, NULL, NULL,
            v_list_unsubscribe, v_list_id, v_x_priority,
            v_return_path, v_reply_to, v_custom_headers, v_message_size, v_message_tokens,
            COALESCE(v_related_message_ids, '{}'),
            NOW()
        )
        ON CONFLICT (account_id, message_id) DO UPDATE SET
            location = COALESCE(EXCLUDED.location, emails.location),
            last_user_moved_at = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN NOW()
                ELSE emails.last_user_moved_at
            END,
            user_pinned_folder = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN EXCLUDED.location
                ELSE emails.user_pinned_folder
            END,
            filing_lock_until = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN NOW() + INTERVAL '720 hours'
                ELSE emails.filing_lock_until
            END,
            last_filed_by = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN 'user'
                ELSE emails.last_filed_by
            END,
            move_count = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN COALESCE(emails.move_count, 0) + 1
                ELSE emails.move_count
            END,
            uid = COALESCE(EXCLUDED.uid, emails.uid),
            uid_validity = COALESCE(EXCLUDED.uid_validity, emails.uid_validity),
            subject = COALESCE(EXCLUDED.subject, emails.subject),
            sender = COALESCE(EXCLUDED.sender, emails.sender),
            received_date = COALESCE(EXCLUDED.received_date, emails.received_date),
            raw_email_content = COALESCE(EXCLUDED.raw_email_content, emails.raw_email_content),
            body_text = COALESCE(EXCLUDED.body_text, emails.body_text),
            body_synced_at = CASE WHEN EXCLUDED.body_synced_at IS NOT NULL THEN EXCLUDED.body_synced_at ELSE emails.body_synced_at END,
            recipients_to = CASE WHEN EXCLUDED.recipients_to <> '{}' THEN EXCLUDED.recipients_to ELSE emails.recipients_to END,
            recipients_cc = CASE WHEN EXCLUDED.recipients_cc <> '{}' THEN EXCLUDED.recipients_cc ELSE emails.recipients_cc END,
            recipients_bcc = CASE WHEN EXCLUDED.recipients_bcc <> '{}' THEN EXCLUDED.recipients_bcc ELSE emails.recipients_bcc END,
            is_read = COALESCE(EXCLUDED.is_read, emails.is_read),
            list_unsubscribe = COALESCE(EXCLUDED.list_unsubscribe, emails.list_unsubscribe),
            list_id = COALESCE(EXCLUDED.list_id, emails.list_id),
            x_priority = COALESCE(EXCLUDED.x_priority, emails.x_priority),
            return_path = COALESCE(EXCLUDED.return_path, emails.return_path),
            reply_to = COALESCE(EXCLUDED.reply_to, emails.reply_to),
            custom_headers = COALESCE(EXCLUDED.custom_headers, emails.custom_headers),
            message_size = COALESCE(EXCLUDED.message_size, emails.message_size),
            message_tokens = COALESCE(EXCLUDED.message_tokens, emails.message_tokens),
            related_message_ids = CASE WHEN EXCLUDED.related_message_ids <> '{}' THEN EXCLUDED.related_message_ids ELSE emails.related_message_ids END,
            updated_at = NOW();
        -- body_synced_at is set in the UPDATE above when v_body_synced_at IS NOT NULL.
        -- Caller marks by message_id for backfill to avoid over-marking duplicate (location, uid) rows.
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION upsert_emails_from_import(payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM upsert_emails_from_import('default', payload);
END;
$$;

-- ============================================================================
-- Backfill message_tokens for existing rows (chars/4 heuristic)
-- ============================================================================
-- Call: SELECT backfill_message_tokens('default');
-- Updates rows where message_tokens IS NULL and (body_text or raw_email_content) exists.
CREATE OR REPLACE FUNCTION backfill_message_tokens(v_account_id TEXT)
RETURNS bigint
LANGUAGE plpgsql
AS $$
DECLARE
    n bigint;
BEGIN
    UPDATE emails
    SET message_tokens = ceil(length(COALESCE(body_text, raw_email_content, ''))::numeric / 4)::integer,
        updated_at = NOW()
    WHERE account_id = v_account_id
      AND message_tokens IS NULL
      AND ( (body_text IS NOT NULL AND length(trim(body_text)) > 0)
         OR (raw_email_content IS NOT NULL AND length(raw_email_content) > 0) );
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END;
$$;

CREATE OR REPLACE FUNCTION backfill_message_tokens()
RETURNS bigint
LANGUAGE plpgsql
AS $$
BEGIN
    RETURN backfill_message_tokens('default');
END;
$$;

-- ============================================================================
-- Envelope sync: upsert from IMAP FETCH ENVELOPE (message_id key; location, uid, subject, from, to, cc, bcc, date)
-- ============================================================================
-- Payload: JSONB array. Each object MUST have message_id (real Message-ID from headers; no synthetic).
-- Caller must not pass envelopes without message_id - treat as failure.
CREATE OR REPLACE FUNCTION upsert_emails_from_envelope(v_account_id TEXT, payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    elem JSONB;
    v_message_id TEXT;
    v_location TEXT;
    v_uid INTEGER;
    v_uid_validity INTEGER;
    v_subject TEXT;
    v_sender TEXT;
    v_recipients_to TEXT[];
    v_recipients_cc TEXT[];
    v_recipients_bcc TEXT[];
    v_received_date TIMESTAMPTZ;
    v_domain TEXT;
BEGIN
    FOR elem IN SELECT * FROM jsonb_array_elements(payload)
    LOOP
        v_uid := (elem->>'uid')::integer;
        v_uid_validity := (elem->>'uid_validity')::integer;
        v_location := NULLIF(trim(elem->>'location'), '');

        v_message_id := NULLIF(trim(elem->>'message_id'), '');
        IF v_message_id IS NULL OR v_message_id = '' THEN
            RAISE EXCEPTION 'upsert_emails_from_envelope: message_id required (no synthetic). location=%, uid=%', v_location, v_uid;
        END IF;

        v_subject := NULLIF(trim(elem->>'subject'), '');
        v_sender := NULLIF(trim(elem->>'sender'), '');
        v_recipients_to := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'recipients_to')),
            '{}'
        );
        v_recipients_cc := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'recipients_cc')),
            '{}'
        );
        v_recipients_bcc := COALESCE(
            ARRAY(SELECT jsonb_array_elements_text(elem->'recipients_bcc')),
            '{}'
        );
        v_received_date := (elem->>'received_date')::timestamptz;

        IF v_location IS NOT NULL THEN
            INSERT INTO email_location_events (
                account_id, message_id, event_type, actor, from_folder, to_folder,
                reason, confidence, metadata
            )
            SELECT
                v_account_id,
                v_message_id,
                'user_moved',
                'user',
                location,
                v_location,
                'observed_imap_location_change',
                1.0,
                jsonb_build_object('source', 'upsert_emails_from_envelope')
            FROM emails
            WHERE account_id = v_account_id
              AND message_id = v_message_id
              AND location IS NOT NULL
              AND LOWER(location) <> LOWER(v_location);

            IF FOUND AND v_sender IS NOT NULL AND trim(v_sender) <> '' THEN
                INSERT INTO folder_learning_rules (
                    account_id, scope_type, scope_value, preferred_folder,
                    confidence, support_count, source, last_observed_at, updated_at
                )
                VALUES (
                    v_account_id, 'sender', LOWER(trim(v_sender)), v_location,
                    0.65, 1, 'user_move', NOW(), NOW()
                )
                ON CONFLICT (account_id, scope_type, scope_value, preferred_folder) DO UPDATE SET
                    support_count = folder_learning_rules.support_count + 1,
                    confidence = LEAST(0.95, folder_learning_rules.confidence + 0.05),
                    last_observed_at = NOW(),
                    updated_at = NOW();

                v_domain := NULLIF(LOWER(regexp_replace(split_part(v_sender, '@', 2), '[^a-zA-Z0-9.-].*$', '')), '');
                IF v_domain IS NOT NULL THEN
                    INSERT INTO folder_learning_rules (
                        account_id, scope_type, scope_value, preferred_folder,
                        confidence, support_count, source, last_observed_at, updated_at
                    )
                    VALUES (
                        v_account_id, 'domain', v_domain, v_location,
                        0.55, 1, 'user_move', NOW(), NOW()
                    )
                    ON CONFLICT (account_id, scope_type, scope_value, preferred_folder) DO UPDATE SET
                        support_count = folder_learning_rules.support_count + 1,
                        confidence = LEAST(0.9, folder_learning_rules.confidence + 0.03),
                        last_observed_at = NOW(),
                        updated_at = NOW();
                END IF;
            END IF;
        END IF;

        -- Explicitly set status columns to NULL (allowed by CHECK); avoids default 'unknown' on older DBs.
        INSERT INTO emails (
            account_id,
            message_id,
            location, uid, uid_validity,
            subject, sender, received_date,
            recipients_to, recipients_cc, recipients_bcc,
            spam_status, phishing_status, marketing_status, otp_status,
            updated_at
        )
        VALUES (
            v_account_id,
            v_message_id,
            v_location, v_uid, v_uid_validity,
            v_subject, v_sender, v_received_date,
            v_recipients_to, v_recipients_cc, v_recipients_bcc,
            NULL, NULL, NULL, NULL,
            NOW()
        )
        ON CONFLICT (account_id, message_id) DO UPDATE SET
            location = COALESCE(EXCLUDED.location, emails.location),
            last_user_moved_at = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN NOW()
                ELSE emails.last_user_moved_at
            END,
            user_pinned_folder = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN EXCLUDED.location
                ELSE emails.user_pinned_folder
            END,
            filing_lock_until = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN NOW() + INTERVAL '720 hours'
                ELSE emails.filing_lock_until
            END,
            last_filed_by = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN 'user'
                ELSE emails.last_filed_by
            END,
            move_count = CASE
                WHEN EXCLUDED.location IS NOT NULL
                 AND emails.location IS NOT NULL
                 AND LOWER(EXCLUDED.location) <> LOWER(emails.location)
                THEN COALESCE(emails.move_count, 0) + 1
                ELSE emails.move_count
            END,
            uid = COALESCE(EXCLUDED.uid, emails.uid),
            uid_validity = COALESCE(EXCLUDED.uid_validity, emails.uid_validity),
            subject = COALESCE(EXCLUDED.subject, emails.subject),
            sender = COALESCE(EXCLUDED.sender, emails.sender),
            received_date = COALESCE(EXCLUDED.received_date, emails.received_date),
            recipients_to = CASE WHEN EXCLUDED.recipients_to <> '{}' THEN EXCLUDED.recipients_to ELSE emails.recipients_to END,
            recipients_cc = CASE WHEN EXCLUDED.recipients_cc <> '{}' THEN EXCLUDED.recipients_cc ELSE emails.recipients_cc END,
            recipients_bcc = CASE WHEN EXCLUDED.recipients_bcc <> '{}' THEN EXCLUDED.recipients_bcc ELSE emails.recipients_bcc END,
            updated_at = NOW();
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION upsert_emails_from_envelope(payload JSONB)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM upsert_emails_from_envelope('default', payload);
END;
$$;
