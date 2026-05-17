use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::ai::CompletionTool;

#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, args: Value) -> Result<String>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn AgentTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: impl AgentTool + 'static) {
        self.tools.insert(tool.name().to_string(), Box::new(tool));
    }

    pub fn as_completion_tools(&self) -> Vec<CompletionTool> {
        let mut tools: Vec<CompletionTool> = self
            .tools
            .values()
            .map(|tool| CompletionTool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    pub async fn execute(&self, name: &str, args: Value) -> Result<String> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args).await,
            None => Ok(format!("Error: unknown tool '{}'", name)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EchoTool;

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Returns the 'text' argument unchanged. Used for harness testing."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        Ok(args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_echo_tool_executes() {
        let tool = EchoTool;
        let out = tool
            .execute(serde_json::json!({ "text": "hello" }))
            .await
            .expect("execute");
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error_string_not_err() {
        let registry = ToolRegistry::new();
        let out = registry
            .execute("nonexistent", serde_json::json!({}))
            .await
            .expect("unknown tool should still return Ok");
        assert_eq!(out, "Error: unknown tool 'nonexistent'");
    }

    #[test]
    fn test_as_completion_tools_includes_registered_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let tools = registry.as_completion_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }
}
