use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub name: String,
    pub version: String,
    pub description: String,
    pub skills: Vec<String>,
    pub execution: ExecutionConfig,
    pub budget: BudgetConfig,
    pub state: StateConfig,
    pub output: OutputConfig,
    pub provider: ProviderConfig,
    pub escalation: EscalationConfig,
    pub system_prompt: String,
}

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub max_iterations: usize,
    pub temperature: f32,
    pub max_output_tokens: u32,
    pub checkpoint_every: usize,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct BudgetConfig {
    pub max_llm_calls: usize,
    pub max_tool_calls: usize,
}

#[derive(Debug, Clone)]
pub struct StateConfig {
    pub schema: Vec<String>,
    pub ttl_hours: u64,
}

#[derive(Debug, Clone)]
pub struct OutputConfig {
    pub required_fields: Vec<String>,
    pub validation: HashMap<String, FieldValidation>,
}

#[derive(Debug, Clone, Default)]
pub struct FieldValidation {
    pub enum_values: Option<Vec<String>>,
    pub field_type: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub tier: ProviderTier,
    pub prefer: String,
    pub fallback: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderTier {
    Worker,
    Orchestrator,
}

#[derive(Debug, Clone)]
pub struct EscalationConfig {
    pub confidence_threshold: f32,
    pub always_escalate_on_phishing: bool,
    pub always_escalate_on_threat: Vec<String>,
}

impl AgentSpec {
    pub fn parse_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read agent spec {}", path.display()))?;
        let (frontmatter, system_prompt) = split_frontmatter(&content)?;
        let raw: RawAgentSpec =
            toml::from_str(frontmatter).context("failed to parse TOML frontmatter")?;
        let skills = raw.skills.clone();
        let composed_prompt = compose_system_prompt(path, system_prompt, &skills)?;
        raw.into_agent_spec(composed_prompt)
    }

    pub fn parse_str(content: &str) -> Result<Self> {
        let (frontmatter, system_prompt) = split_frontmatter(content)?;
        let raw: RawAgentSpec =
            toml::from_str(frontmatter).context("failed to parse TOML frontmatter")?;
        raw.into_agent_spec(system_prompt)
    }

    pub fn validate_output(&self, output: &Value) -> Result<()> {
        self.output.validate(output)
    }
}

impl OutputConfig {
    pub fn validate(&self, output: &Value) -> Result<()> {
        let object = output
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("output must be a JSON object"))?;

        for field in &self.required_fields {
            match object.get(field) {
                Some(Value::Null) => bail!("required field '{}' must not be null", field),
                Some(_) => {}
                None => bail!("missing required field '{}'", field),
            }
        }

        for (field, validation) in &self.validation {
            let Some(value) = object.get(field) else {
                continue;
            };

            if let Some(enum_values) = &validation.enum_values {
                let Some(string_value) = value.as_str() else {
                    bail!("field '{}' must be a string matching enum values", field);
                };
                if !enum_values
                    .iter()
                    .any(|candidate| candidate == string_value)
                {
                    bail!(
                        "field '{}' has invalid enum value '{}'; expected one of {}",
                        field,
                        string_value,
                        enum_values.join(", ")
                    );
                }
            }

            if let Some(field_type) = &validation.field_type {
                match field_type.as_str() {
                    "number" => {
                        let Some(number) = value.as_f64() else {
                            bail!("field '{}' must be a number", field);
                        };
                        if let Some(min) = validation.min {
                            if number < min {
                                bail!("field '{}' must be >= {}", field, min);
                            }
                        }
                        if let Some(max) = validation.max {
                            if number > max {
                                bail!("field '{}' must be <= {}", field, max);
                            }
                        }
                    }
                    "string" => {
                        if !value.is_string() {
                            bail!("field '{}' must be a string", field);
                        }
                    }
                    "boolean" => {
                        if !value.is_boolean() {
                            bail!("field '{}' must be a boolean", field);
                        }
                    }
                    other => bail!(
                        "field '{}' has unsupported validation type '{}'",
                        field,
                        other
                    ),
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RawAgentSpec {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    execution: RawExecutionConfig,
    #[serde(default)]
    budget: RawBudgetConfig,
    #[serde(default)]
    state: RawStateConfig,
    #[serde(default)]
    output: RawOutputConfig,
    #[serde(default)]
    provider: RawProviderConfig,
    #[serde(default)]
    escalation: RawEscalationConfig,
}

impl RawAgentSpec {
    fn into_agent_spec(self, system_prompt: String) -> Result<AgentSpec> {
        if self.name.trim().is_empty() {
            anyhow::bail!("agent spec name must not be empty");
        }

        let tier = match self.provider.tier.to_lowercase().as_str() {
            "worker" => ProviderTier::Worker,
            "orchestrator" => ProviderTier::Orchestrator,
            other => anyhow::bail!("unknown provider tier: {}", other),
        };

        let validation = self
            .output
            .validation
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    FieldValidation {
                        enum_values: value.enum_values,
                        field_type: value.field_type,
                        min: value.min,
                        max: value.max,
                    },
                )
            })
            .collect();

        Ok(AgentSpec {
            name: self.name,
            version: self.version,
            description: self.description,
            skills: self.skills,
            execution: ExecutionConfig {
                max_iterations: self.execution.max_iterations.max(1),
                temperature: self.execution.temperature,
                max_output_tokens: self.execution.max_output_tokens.max(1),
                checkpoint_every: self.execution.checkpoint_every.max(1),
                timeout_secs: self.execution.timeout_secs.max(1),
            },
            budget: BudgetConfig {
                max_llm_calls: self.budget.max_llm_calls.max(1),
                max_tool_calls: self.budget.max_tool_calls.max(1),
            },
            state: StateConfig {
                schema: self.state.schema,
                ttl_hours: self.state.ttl_hours,
            },
            output: OutputConfig {
                required_fields: self.output.required_fields,
                validation,
            },
            provider: ProviderConfig {
                tier,
                prefer: self.provider.prefer,
                fallback: self.provider.fallback,
            },
            escalation: EscalationConfig {
                confidence_threshold: self.escalation.confidence_threshold,
                always_escalate_on_phishing: self.escalation.always_escalate_on_phishing,
                always_escalate_on_threat: self.escalation.always_escalate_on_threat,
            },
            system_prompt,
        })
    }
}

fn compose_system_prompt(path: &Path, system_prompt: String, skills: &[String]) -> Result<String> {
    let Some(skills_dir) = skill_docs_dir(path) else {
        if skills.is_empty() {
            return Ok(system_prompt);
        }
        anyhow::bail!(
            "agent spec {} declares skills but no specs/skills directory was found",
            path.display()
        );
    };

    let mut sections = Vec::new();
    let base_path = skills_dir.join("base-conventions.md");
    if base_path.exists() {
        sections.push(
            fs::read_to_string(&base_path)
                .with_context(|| format!("failed to read skill doc {}", base_path.display()))?
                .trim()
                .to_string(),
        );
    }

    for skill in skills {
        validate_skill_name(skill)?;
        let skill_path = skills_dir.join(format!("{skill}.md"));
        let content = fs::read_to_string(&skill_path)
            .with_context(|| format!("failed to read skill doc {}", skill_path.display()))?;
        sections.push(format!("## Skill: {skill}\n\n{}", content.trim()));
    }

    sections.push(system_prompt);
    Ok(sections.join("\n\n---\n\n"))
}

fn skill_docs_dir(spec_path: &Path) -> Option<PathBuf> {
    let spec_dir = spec_path.parent()?;
    let sibling = spec_dir.parent()?.join("skills");
    if sibling.is_dir() {
        return Some(sibling);
    }

    let nested = spec_dir.join("skills");
    if nested.is_dir() {
        return Some(nested);
    }

    None
}

fn validate_skill_name(skill: &str) -> Result<()> {
    if !skill.is_empty()
        && skill
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Ok(());
    }
    anyhow::bail!("skill names may only contain letters, numbers, '-' or '_'");
}

#[derive(Debug, Deserialize)]
struct RawExecutionConfig {
    #[serde(default = "default_max_iterations")]
    max_iterations: usize,
    #[serde(default = "default_temperature")]
    temperature: f32,
    #[serde(default = "default_max_output_tokens")]
    max_output_tokens: u32,
    #[serde(default = "default_checkpoint_every")]
    checkpoint_every: usize,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

impl Default for RawExecutionConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            temperature: default_temperature(),
            max_output_tokens: default_max_output_tokens(),
            checkpoint_every: default_checkpoint_every(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawBudgetConfig {
    #[serde(default = "default_max_llm_calls")]
    max_llm_calls: usize,
    #[serde(default = "default_max_tool_calls")]
    max_tool_calls: usize,
}

impl Default for RawBudgetConfig {
    fn default() -> Self {
        Self {
            max_llm_calls: default_max_llm_calls(),
            max_tool_calls: default_max_tool_calls(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawStateConfig {
    #[serde(default)]
    schema: Vec<String>,
    #[serde(default = "default_ttl_hours")]
    ttl_hours: u64,
}

impl Default for RawStateConfig {
    fn default() -> Self {
        Self {
            schema: Vec::new(),
            ttl_hours: default_ttl_hours(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawOutputConfig {
    #[serde(default)]
    required_fields: Vec<String>,
    #[serde(default)]
    validation: HashMap<String, RawFieldValidation>,
}

#[derive(Debug, Deserialize, Default)]
struct RawFieldValidation {
    #[serde(rename = "enum", default)]
    enum_values: Option<Vec<String>>,
    #[serde(rename = "type", default)]
    field_type: Option<String>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawProviderConfig {
    #[serde(default = "default_provider_tier")]
    tier: String,
    #[serde(default = "default_provider_prefer")]
    prefer: String,
    #[serde(default = "default_provider_fallback")]
    fallback: String,
}

impl Default for RawProviderConfig {
    fn default() -> Self {
        Self {
            tier: default_provider_tier(),
            prefer: default_provider_prefer(),
            fallback: default_provider_fallback(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawEscalationConfig {
    #[serde(default = "default_confidence_threshold")]
    confidence_threshold: f32,
    #[serde(default)]
    always_escalate_on_phishing: bool,
    #[serde(default)]
    always_escalate_on_threat: Vec<String>,
}

impl Default for RawEscalationConfig {
    fn default() -> Self {
        Self {
            confidence_threshold: default_confidence_threshold(),
            always_escalate_on_phishing: false,
            always_escalate_on_threat: Vec::new(),
        }
    }
}

fn split_frontmatter(content: &str) -> Result<(&str, String)> {
    if content.is_empty() {
        anyhow::bail!("agent spec is empty");
    }

    let mut line_start = 0usize;
    let mut line_index = 0usize;
    let mut first_line_end: Option<usize> = None;

    for (idx, ch) in content.char_indices() {
        if ch != '\n' {
            continue;
        }

        let line = trim_line(&content[line_start..idx]);
        if line_index == 0 {
            if line != "+++" {
                anyhow::bail!("agent spec must start with +++ frontmatter");
            }
            first_line_end = Some(idx + 1);
        } else if line == "+++" {
            let frontmatter_start = first_line_end.unwrap_or_default();
            let frontmatter = &content[frontmatter_start..line_start];
            let system_prompt = content.get(idx + 1..).unwrap_or("").trim().to_string();
            return Ok((frontmatter, system_prompt));
        }

        line_start = idx + 1;
        line_index += 1;
    }

    let last_line = trim_line(&content[line_start..]);
    if line_index == 0 {
        if last_line != "+++" {
            anyhow::bail!("agent spec must start with +++ frontmatter");
        }
    } else if last_line == "+++" {
        let frontmatter_start = first_line_end.unwrap_or_default();
        let frontmatter = &content[frontmatter_start..line_start];
        return Ok((frontmatter, String::new()));
    }

    anyhow::bail!("agent spec frontmatter is missing closing +++");
}

fn trim_line(line: &str) -> &str {
    line.trim_end_matches('\r')
}

fn default_version() -> String {
    "1.0".to_string()
}

fn default_max_iterations() -> usize {
    8
}

fn default_temperature() -> f32 {
    0.2
}

fn default_max_output_tokens() -> u32 {
    4096
}

fn default_checkpoint_every() -> usize {
    1
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_max_llm_calls() -> usize {
    10
}

fn default_max_tool_calls() -> usize {
    15
}

fn default_ttl_hours() -> u64 {
    168
}

fn default_provider_tier() -> String {
    "worker".to_string()
}

fn default_provider_prefer() -> String {
    "local".to_string()
}

fn default_provider_fallback() -> String {
    "frontier".to_string()
}

fn default_confidence_threshold() -> f32 {
    0.75
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_agent_md() {
        let input = r#"+++
name = "tester"
version = "2.0"
description = "Test agent"

[execution]
max_iterations = 4
temperature = 0.4
max_output_tokens = 2048
checkpoint_every = 2
timeout_secs = 30

[budget]
max_llm_calls = 3
max_tool_calls = 7

[state]
schema = ["sender_patterns"]
ttl_hours = 24

[output]
required_fields = ["status"]

[output.validation]
status = { enum = ["ok", "bad"] }

[provider]
tier = "orchestrator"
prefer = "frontier"
fallback = "local"

[escalation]
confidence_threshold = 0.9
always_escalate_on_phishing = true
always_escalate_on_threat = ["high"]
+++
You are a test prompt.
"#;

        let spec = AgentSpec::parse_str(input).expect("parse");
        assert_eq!(spec.name, "tester");
        assert_eq!(spec.version, "2.0");
        assert_eq!(spec.description, "Test agent");
        assert_eq!(spec.execution.max_iterations, 4);
        assert_eq!(spec.execution.temperature, 0.4);
        assert_eq!(spec.execution.max_output_tokens, 2048);
        assert_eq!(spec.execution.checkpoint_every, 2);
        assert_eq!(spec.execution.timeout_secs, 30);
        assert_eq!(spec.budget.max_llm_calls, 3);
        assert_eq!(spec.budget.max_tool_calls, 7);
        assert_eq!(spec.state.schema, vec!["sender_patterns"]);
        assert_eq!(spec.state.ttl_hours, 24);
        assert_eq!(spec.output.required_fields, vec!["status"]);
        assert_eq!(
            spec.output
                .validation
                .get("status")
                .and_then(|v| v.enum_values.clone()),
            Some(vec!["ok".to_string(), "bad".to_string()])
        );
        assert_eq!(spec.provider.tier, ProviderTier::Orchestrator);
        assert_eq!(spec.provider.prefer, "frontier");
        assert_eq!(spec.provider.fallback, "local");
        assert!((spec.escalation.confidence_threshold - 0.9).abs() < f32::EPSILON);
        assert!(spec.escalation.always_escalate_on_phishing);
        assert_eq!(spec.escalation.always_escalate_on_threat, vec!["high"]);
        assert_eq!(spec.system_prompt, "You are a test prompt.");
    }

    #[test]
    fn test_parse_defaults() {
        let input = r#"+++
name = "minimal"
+++
Prompt body.
"#;

        let spec = AgentSpec::parse_str(input).expect("parse");
        assert_eq!(spec.name, "minimal");
        assert_eq!(spec.version, "1.0");
        assert_eq!(spec.description, "");
        assert_eq!(spec.execution.max_iterations, 8);
        assert_eq!(spec.execution.temperature, 0.2);
        assert_eq!(spec.execution.max_output_tokens, 4096);
        assert_eq!(spec.execution.checkpoint_every, 1);
        assert_eq!(spec.execution.timeout_secs, 120);
        assert_eq!(spec.budget.max_llm_calls, 10);
        assert_eq!(spec.budget.max_tool_calls, 15);
        assert!(spec.state.schema.is_empty());
        assert_eq!(spec.state.ttl_hours, 168);
        assert!(spec.output.required_fields.is_empty());
        assert!(spec.output.validation.is_empty());
        assert_eq!(spec.provider.tier, ProviderTier::Worker);
        assert_eq!(spec.provider.prefer, "local");
        assert_eq!(spec.provider.fallback, "frontier");
        assert!((spec.escalation.confidence_threshold - 0.75).abs() < f32::EPSILON);
        assert!(!spec.escalation.always_escalate_on_phishing);
        assert!(spec.escalation.always_escalate_on_threat.is_empty());
    }

    #[test]
    fn test_parse_bad_toml_returns_error() {
        let input = r#"+++
name = "broken
+++
Prompt
"#;

        assert!(AgentSpec::parse_str(input).is_err());
    }

    #[test]
    fn test_parse_unknown_provider_tier_returns_error() {
        let input = r#"+++
name = "tester"

[provider]
tier = "invalid"
+++
Prompt
"#;

        assert!(AgentSpec::parse_str(input).is_err());
    }

    #[test]
    fn test_system_prompt_extracted_verbatim() {
        let input = "+++\nname = \"tester\"\n+++\n\n  Line one\nLine two  \n";
        let spec = AgentSpec::parse_str(input).expect("parse");
        assert_eq!(spec.system_prompt, "Line one\nLine two");
    }

    #[test]
    fn test_parse_file() {
        let path = std::env::temp_dir().join(format!(
            "agent-spec-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::write(&path, "+++\nname = \"from-file\"\n+++\nPrompt").expect("write");
        let spec = AgentSpec::parse_file(&path).expect("parse file");
        assert_eq!(spec.name, "from-file");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_parse_file_composes_base_and_skills() {
        let root = std::env::temp_dir().join(format!(
            "agent-spec-skill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let agents_dir = root.join("agents");
        let skills_dir = root.join("skills");
        fs::create_dir_all(&agents_dir).expect("create agents dir");
        fs::create_dir_all(&skills_dir).expect("create skills dir");
        fs::write(skills_dir.join("base-conventions.md"), "Base rules").expect("write base");
        fs::write(skills_dir.join("demo-skill.md"), "Demo rules").expect("write skill");

        let path = agents_dir.join("demo.md");
        fs::write(
            &path,
            "+++\nname = \"demo\"\nskills = [\"demo-skill\"]\n+++\nSpec prompt",
        )
        .expect("write spec");

        let spec = AgentSpec::parse_file(&path).expect("parse composed spec");
        assert_eq!(spec.skills, vec!["demo-skill".to_string()]);
        assert!(spec.system_prompt.contains("Base rules"));
        assert!(spec.system_prompt.contains("## Skill: demo-skill"));
        assert!(spec.system_prompt.contains("Demo rules"));
        assert!(spec.system_prompt.ends_with("Spec prompt"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_consolidator_spec_parses() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/folder-consolidator.md"))
            .expect("parse folder-consolidator spec");
        assert_eq!(spec.name, "folder-consolidator");
        assert_eq!(spec.execution.max_iterations, 10);
        assert_eq!(spec.budget.max_tool_calls, 20);
        assert!(spec
            .output
            .required_fields
            .contains(&"consolidation_proposals".to_string()));
        assert!(spec.output.required_fields.contains(&"summary".to_string()));
    }

    #[test]
    fn test_consolidator_output_validates() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/folder-consolidator.md"))
            .expect("parse folder-consolidator spec");
        let output = serde_json::json!({
            "consolidation_proposals": [
                {
                    "source_folder": "Receipts/2023",
                    "target_folder": "Receipts",
                    "action": "merge",
                    "reason": "Same content type",
                    "email_count": 12,
                    "confidence": 0.92
                }
            ],
            "summary": "Found 1 merge candidate."
        });
        let result = spec.validate_output(&output);
        assert!(
            result.is_ok(),
            "valid output should pass validation: {:?}",
            result
        );
    }

    #[test]
    fn test_consolidator_system_folder_guard_list_present() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/folder-consolidator.md"))
            .expect("parse folder-consolidator spec");
        for folder in [
            "INBOX", "Sent", "Trash", "Junk", "Drafts", "[Gmail]", "Spam", "Archive",
        ] {
            assert!(
                spec.system_prompt.contains(folder),
                "system prompt should include system folder guard for {}",
                folder
            );
        }
    }

    #[test]
    fn test_digest_spec_parses() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/digest-agent.md"))
            .expect("parse digest spec");
        assert_eq!(spec.name, "digest-agent");
        assert_eq!(spec.execution.max_iterations, 5);
        assert_eq!(spec.budget.max_tool_calls, 8);
        assert!(spec
            .output
            .required_fields
            .contains(&"digest_markdown".to_string()));
        assert!(spec
            .output
            .required_fields
            .contains(&"by_category".to_string()));
        assert!(spec
            .output
            .required_fields
            .contains(&"top_senders".to_string()));
    }

    #[test]
    fn test_digest_output_validates() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/digest-agent.md"))
            .expect("parse digest spec");
        let output = serde_json::json!({
            "period": "daily",
            "total_received": 47,
            "by_category": { "work": 18, "financial": 8 },
            "top_senders": [{ "address": "test@example.com", "count": 5 }],
            "action_summary": { "filed": 38, "trashed": 2, "junked": 5 },
            "digest_markdown": "# Digest\n\nContent here.",
            "summary": "47 emails received."
        });
        let result = spec.validate_output(&output);
        assert!(
            result.is_ok(),
            "valid digest output should pass validation: {:?}",
            result
        );
    }

    #[test]
    fn test_orchestrator_spec_parses() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/orchestrator.md"))
            .expect("parse orchestrator spec");
        assert_eq!(spec.name, "orchestrator");
        assert_eq!(spec.provider.tier, ProviderTier::Orchestrator);
        assert_eq!(spec.provider.prefer, "frontier");
        assert_eq!(spec.provider.fallback, "local");
        assert!(spec
            .output
            .required_fields
            .contains(&"task_type".to_string()));
        assert!(spec.output.required_fields.contains(&"result".to_string()));
    }

    #[test]
    fn test_orchestrator_batch_plan_output_validates() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/orchestrator.md"))
            .expect("parse orchestrator spec");
        let output = serde_json::json!({
            "task_type": "batch_plan",
            "result": {
                "priority_order": ["m1", "m2"],
                "groups": [
                    {"label": "thread", "message_ids": ["m1", "m2"], "reason": "same thread"}
                ],
                "worker_instructions": {
                    "m1": "Prior phishing from this sender; verify links carefully."
                },
                "confidence_threshold_override": 0.8
            }
        });
        let result = spec.validate_output(&output);
        assert!(
            result.is_ok(),
            "valid orchestrator batch_plan output should pass validation: {:?}",
            result
        );
    }

    #[test]
    fn test_orchestrator_batch_review_output_validates() {
        let spec = AgentSpec::parse_file(Path::new("specs/agents/orchestrator.md"))
            .expect("parse orchestrator spec");
        let output = serde_json::json!({
            "task_type": "batch_review",
            "result": {
                "reanalyze": ["m2"],
                "reanalyze_reason": {
                    "m2": "Classification inconsistent with similar sender cohort."
                },
                "campaigns_detected": [
                    {
                        "type": "phishing_campaign",
                        "sender_domain": "evil.test",
                        "message_ids": ["m1", "m2"],
                        "action": "bulk_trash"
                    }
                ],
                "batch_quality_score": 0.93
            }
        });
        let result = spec.validate_output(&output);
        assert!(
            result.is_ok(),
            "valid orchestrator batch_review output should pass validation: {:?}",
            result
        );
    }
}
