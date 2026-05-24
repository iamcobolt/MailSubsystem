//! CLI command parsing and usage output.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Check,
    Sync,
    SyncSlow,
    Status,
    SyncIncremental,
    SyncWindow {
        days: Option<i64>,
        full: bool,
    },
    Api {
        bind: Option<String>,
    },
    App {
        bind: Option<String>,
    },
    Core,
    CoreStatus,
    Tui {
        api_url: Option<String>,
    },
    Analyze {
        message_id: Option<String>,
        force: bool,
    },
    AnalyzeWorker {
        limit: usize,
        concurrency: Option<usize>,
    },
    TestLlm {
        local: bool,
        frontier: bool,
    },
    Show {
        message_id: Option<String>,
    },
    ResolveOrphans,
    ProcessFrontierQueue {
        limit: usize,
    },
    Locate {
        message_id: Option<String>,
        force: bool,
        limit: Option<usize>,
    },
    AgentRun {
        agent_spec: String,
        task_id: String,
        input: Option<String>,
    },
    AgentRuns {
        limit: usize,
        status: Option<String>,
        agent: Option<String>,
    },
    AgentShow {
        run_id: String,
    },
    AgentBenchmark {
        limit: usize,
    },
    AgentScratchpad {
        agent: Option<String>,
        key: Option<String>,
    },
    AgentScratchpadDelete {
        agent: String,
        key: String,
    },
    AgentStats,
    AgentCalibrate {
        agent: Option<String>,
    },
    ScratchpadHygiene {
        account: Option<String>,
    },
    File {
        dry_run: bool,
    },
    Consolidate {
        dry_run: bool,
        account: Option<String>,
    },
    LifecycleCleanup {
        dry_run: bool,
    },
    Digest {
        window: String,
        account: Option<String>,
        json: bool,
    },
    BackfillMessageTokens,
    BackfillMessage {
        message_id: Option<String>,
    },
    BackfillFromImap {
        limit: usize,
    },
    MigrateSchema {
        apply: bool,
    },
    EmbedBackfill {
        limit: usize,
    },
    EmbedRebuild {
        limit: usize,
    },
    Help,
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Check => "check",
            Command::Sync => "sync",
            Command::SyncSlow => "sync-slow",
            Command::Status => "status",
            Command::SyncIncremental => "sync-incremental",
            Command::SyncWindow { full, .. } => {
                if *full {
                    "sync-window-full"
                } else {
                    "sync-window"
                }
            }
            Command::Api { .. } => "api",
            Command::App { .. } => "app",
            Command::Core => "core",
            Command::CoreStatus => "core-status",
            Command::Tui { .. } => "tui",
            Command::Analyze { .. } => "analyze",
            Command::AnalyzeWorker { .. } => "analyze-worker",
            Command::TestLlm { local, frontier } => {
                if *local {
                    "test-llm-local"
                } else if *frontier {
                    "test-llm-frontier"
                } else {
                    "test-llm"
                }
            }
            Command::Show { .. } => "show",
            Command::ResolveOrphans => "resolve-orphans",
            Command::ProcessFrontierQueue { .. } => "process-frontier-queue",
            Command::Locate { .. } => "locate",
            Command::AgentRun { .. } => "agent-run",
            Command::AgentRuns { .. } => "agent-runs",
            Command::AgentShow { .. } => "agent-show",
            Command::AgentBenchmark { .. } => "agent-benchmark",
            Command::AgentScratchpad { .. } => "agent-scratchpad",
            Command::AgentScratchpadDelete { .. } => "agent-scratchpad-delete",
            Command::AgentStats => "agent-stats",
            Command::AgentCalibrate { .. } => "agent-calibrate",
            Command::ScratchpadHygiene { .. } => "scratchpad-hygiene",
            Command::File { .. } => "file",
            Command::Consolidate { .. } => "consolidate",
            Command::LifecycleCleanup { .. } => "lifecycle-cleanup",
            Command::Digest { .. } => "digest",
            Command::BackfillMessageTokens => "backfill-message-tokens",
            Command::BackfillMessage { .. } => "backfill-message",
            Command::BackfillFromImap { .. } => "backfill-from-imap",
            Command::MigrateSchema { apply } => {
                if *apply {
                    "migrate-schema-apply"
                } else {
                    "migrate-schema"
                }
            }
            Command::EmbedBackfill { .. } => "embed-backfill",
            Command::EmbedRebuild { .. } => "embed-rebuild",
            Command::Help => "help",
        }
    }
}

fn arg_value<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<T>().ok())
}

fn first_positional_arg(args: &[String], start: usize, value_flags: &[&str]) -> Option<String> {
    let mut skip_next = false;
    for arg in args.iter().skip(start) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if value_flags.iter().any(|flag| flag == arg) {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--") {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

pub fn parse_command(args: &[String]) -> Command {
    match args.get(1).map(|s| s.as_str()) {
        Some("check") => Command::Check,
        Some("sync") => Command::Sync,
        Some("sync-slow") => Command::SyncSlow,
        Some("status") => Command::Status,
        Some("sync-incremental") => Command::SyncIncremental,
        Some("sync-window") => Command::SyncWindow {
            full: args.iter().any(|a| a == "--full"),
            days: arg_value::<i64>(args, "--days"),
        },
        Some("api") => Command::Api {
            bind: arg_value::<String>(args, "--bind"),
        },
        Some("app") => Command::App {
            bind: arg_value::<String>(args, "--bind"),
        },
        Some("core") => Command::Core,
        Some("core-status") => Command::CoreStatus,
        Some("tui") => Command::Tui {
            api_url: arg_value::<String>(args, "--api-url"),
        },
        Some("analyze") => {
            let force = args.get(2).map(|s| s.as_str()) == Some("--force");
            let message_id = if force {
                args.get(3).cloned()
            } else {
                args.get(2).cloned()
            };
            Command::Analyze { message_id, force }
        }
        Some("analyze-worker") => Command::AnalyzeWorker {
            limit: arg_value::<usize>(args, "--limit").unwrap_or(50),
            concurrency: arg_value::<usize>(args, "--concurrency"),
        },
        Some("test-llm") => Command::TestLlm {
            local: args.iter().any(|a| a == "--local"),
            frontier: args.iter().any(|a| a == "--frontier"),
        },
        Some("show") => Command::Show {
            message_id: args.get(2).cloned(),
        },
        Some("resolve-orphans") => Command::ResolveOrphans,
        Some("process-frontier-queue") => Command::ProcessFrontierQueue {
            limit: arg_value::<usize>(args, "--limit").unwrap_or(10),
        },
        Some("locate") => {
            let force = args.iter().any(|a| a == "--force");
            let limit = arg_value::<usize>(args, "--limit");
            let message_id = first_positional_arg(args, 2, &["--limit"]);
            Command::Locate {
                message_id,
                force,
                limit,
            }
        }
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("run") => {
            let Some(agent_spec) = args.get(3).cloned() else {
                return Command::Help;
            };
            let Some(task_id) = args.get(4).cloned() else {
                return Command::Help;
            };
            Command::AgentRun {
                agent_spec,
                task_id,
                input: arg_value::<String>(args, "--input"),
            }
        }
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("runs") => Command::AgentRuns {
            limit: arg_value::<usize>(args, "--limit").unwrap_or(20),
            status: arg_value::<String>(args, "--status"),
            agent: arg_value::<String>(args, "--agent"),
        },
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("show") => {
            let Some(run_id) = args.get(3).cloned() else {
                return Command::Help;
            };
            Command::AgentShow { run_id }
        }
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("benchmark") => {
            Command::AgentBenchmark {
                limit: arg_value::<usize>(args, "--limit").unwrap_or(20),
            }
        }
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("scratchpad") => {
            Command::AgentScratchpad {
                agent: arg_value::<String>(args, "--agent"),
                key: arg_value::<String>(args, "--key"),
            }
        }
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("scratchpad-delete") => {
            let Some(agent) = arg_value::<String>(args, "--agent") else {
                return Command::Help;
            };
            let Some(key) = arg_value::<String>(args, "--key") else {
                return Command::Help;
            };
            Command::AgentScratchpadDelete { agent, key }
        }
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("stats") => Command::AgentStats,
        Some("agent") if args.get(2).map(|s| s.as_str()) == Some("calibrate") => {
            Command::AgentCalibrate {
                agent: args.get(3).cloned(),
            }
        }
        Some("scratchpad-hygiene") => Command::ScratchpadHygiene {
            account: arg_value::<String>(args, "--account"),
        },
        Some("file") => Command::File {
            dry_run: args.iter().any(|a| a == "--dry-run"),
        },
        Some("consolidate") => Command::Consolidate {
            dry_run: !args.iter().any(|a| a == "--apply"),
            account: arg_value::<String>(args, "--account"),
        },
        Some("lifecycle-cleanup") => Command::LifecycleCleanup {
            dry_run: args.iter().any(|a| a == "--dry-run"),
        },
        Some("digest") => Command::Digest {
            window: if args.iter().any(|a| a == "--weekly") {
                "weekly".to_string()
            } else {
                "daily".to_string()
            },
            account: arg_value::<String>(args, "--account"),
            json: args.iter().any(|a| a == "--json"),
        },
        Some("backfill-message-tokens") => Command::BackfillMessageTokens,
        Some("backfill-message") => Command::BackfillMessage {
            message_id: args.get(2).cloned(),
        },
        Some("backfill-from-imap") => Command::BackfillFromImap {
            limit: arg_value::<usize>(args, "--limit").unwrap_or(100),
        },
        Some("migrate-schema") => Command::MigrateSchema {
            apply: args.iter().any(|a| a == "--apply"),
        },
        Some("embed-backfill") => Command::EmbedBackfill {
            limit: arg_value::<usize>(args, "--limit").unwrap_or(50),
        },
        Some("embed-rebuild") => Command::EmbedRebuild {
            limit: arg_value::<usize>(args, "--limit").unwrap_or(50),
        },
        _ => Command::Help,
    }
}

pub fn print_usage() {
    println!("MailSubsystem - IMAP + database foundation.");
    println!("Usage:");
    println!("  mailsubsystem app [--bind host:port] - start the local server app (core + API)");
    println!("  mailsubsystem tui [--api-url <url>] - start the terminal UI");
    println!();
    println!("Advanced/admin commands:");
    println!("  mailsubsystem check   - verify database and IMAP connectivity");
    println!("  mailsubsystem sync    - sync envelopes + full bodies (fast and slow in parallel)");
    println!("  mailsubsystem sync-slow - backfill full bodies for emails missing body_text");
    println!("  mailsubsystem sync-incremental - incremental sync using MODSEQ (changes since last sync)");
    println!("  mailsubsystem core   - start the local core work coordinator");
    println!("  mailsubsystem core-status - show core queue and pipeline status");
    println!("  mailsubsystem api [--bind host:port] - start the local HTTP API for wrappers");
    println!(
        "  mailsubsystem tui [--api-url <url>] - start the terminal chat UI against the local API"
    );
    println!("  mailsubsystem status  - show imap_folders and emails table state");
    println!("  mailsubsystem analyze [--force] [message_id] - AI analysis (batch or one record)");
    println!("  mailsubsystem analyze-worker [--limit N] [--concurrency M] - claim and analyze one safe parallel worker batch");
    println!(
        "  mailsubsystem test-llm --local   - test connection to local LLM (LM Studio / Ollama)"
    );
    println!("  mailsubsystem test-llm --frontier - test connection to frontier model (Gemini/OpenAI/Anthropic)");
    println!("  mailsubsystem show <message_id>   - print AI fields stored in DB for that email");
    println!("  mailsubsystem resolve-orphans      - find rows with location=NULL by Message-ID in other folders, update location");
    println!("  mailsubsystem process-frontier-queue [--limit N] - run frontier analysis on queued emails (default N=10)");
    println!("  mailsubsystem locate [--force] [--limit N] [message_id] - agentic location recommendation (batch or one record)");
    println!("  mailsubsystem agent run <agent_spec> <task_id> [--input <json>] - run an agent spec directly for debugging");
    println!("  mailsubsystem agent runs [--limit N] [--status <running|completed|failed|timed_out>] [--agent <name>] - list recent harness runs");
    println!("  mailsubsystem agent show <run_id> - show one harness run in detail");
    println!("  mailsubsystem agent benchmark [--limit N] - benchmark harness against stored analyzed emails (default N=20)");
    println!("  mailsubsystem agent scratchpad [--agent <name>] [--key <key>] - inspect scratchpad state");
    println!("  mailsubsystem agent scratchpad-delete --agent <name> --key <key> - delete one scratchpad entry");
    println!("  mailsubsystem agent stats - aggregate harness stats for the last 24h");
    println!("  mailsubsystem agent calibrate [<agent_name>] - run confidence calibration for one or all agents");
    println!("  mailsubsystem scratchpad-hygiene [--account <id>] - orchestrator-driven scratchpad cleanup");
    println!("  mailsubsystem file [--dry-run]     - apply location recommendations (create folder + MOVE); --dry-run to preview");
    println!("  mailsubsystem consolidate [--apply] [--account <id>] - propose (or apply) redundant folder consolidation");
    println!("  mailsubsystem lifecycle-cleanup [--dry-run] - trash expired OTPs (>1hr) and stale newsletters");
    println!("  mailsubsystem digest [--daily|--weekly] [--account <id>] [--json] - generate inbox activity digest");
    println!("  mailsubsystem backfill-message-tokens - backfill message_tokens (chars/4 estimate) for emails with body content");
    println!("  mailsubsystem backfill-message <message_id>  - fetch one message from IMAP and backfill null fields (searches by Message-ID if location unknown)");
    println!("  mailsubsystem backfill-from-imap [--limit N] - fetch from IMAP to backfill null received_date, raw_email_content, body_text, message_size, message_tokens (default N=100)");
    println!("  mailsubsystem migrate-schema [--apply] - validate schema, or intentionally apply embedded schema.sql");
    println!("  mailsubsystem embed-backfill [--limit N] - generate embeddings for emails missing them (RAG semantic search, default N=50)");
    println!("  mailsubsystem embed-rebuild [--limit N]  - rebuild all embeddings for a new model (nulls existing, recreates index, backfills; default N=50)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_core_status_command() {
        let args = vec!["mailsubsystem".to_string(), "core-status".to_string()];
        assert_eq!(parse_command(&args), Command::CoreStatus);
    }

    #[test]
    fn parse_migrate_schema_apply_command() {
        let args = vec![
            "mailsubsystem".to_string(),
            "migrate-schema".to_string(),
            "--apply".to_string(),
        ];
        assert_eq!(parse_command(&args), Command::MigrateSchema { apply: true });
    }

    #[test]
    fn parse_api_command_with_bind() {
        let args = vec![
            "mailsubsystem".to_string(),
            "api".to_string(),
            "--bind".to_string(),
            "127.0.0.1:4100".to_string(),
        ];
        assert_eq!(
            parse_command(&args),
            Command::Api {
                bind: Some("127.0.0.1:4100".to_string())
            }
        );
    }

    #[test]
    fn parse_app_command_with_bind() {
        let args = vec![
            "mailsubsystem".to_string(),
            "app".to_string(),
            "--bind".to_string(),
            "127.0.0.1:4100".to_string(),
        ];
        assert_eq!(
            parse_command(&args),
            Command::App {
                bind: Some("127.0.0.1:4100".to_string())
            }
        );
    }

    #[test]
    fn parse_locate_command_with_limit_and_message_id() {
        let args = vec![
            "mailsubsystem".to_string(),
            "locate".to_string(),
            "--force".to_string(),
            "--limit".to_string(),
            "75".to_string(),
            "message-1".to_string(),
        ];
        assert_eq!(
            parse_command(&args),
            Command::Locate {
                message_id: Some("message-1".to_string()),
                force: true,
                limit: Some(75)
            }
        );
    }

    #[test]
    fn parse_analyze_worker_command_with_limit_and_concurrency() {
        let args = vec![
            "mailsubsystem".to_string(),
            "analyze-worker".to_string(),
            "--limit".to_string(),
            "200".to_string(),
            "--concurrency".to_string(),
            "8".to_string(),
        ];
        assert_eq!(
            parse_command(&args),
            Command::AnalyzeWorker {
                limit: 200,
                concurrency: Some(8)
            }
        );
    }
}
