use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRef {
    provider: String,
    model: String,
}

impl ModelRef {
    pub(crate) fn parse(value: &str, name: &str) -> Result<Self> {
        let (provider, model) = value
            .trim()
            .split_once('/')
            .ok_or_else(|| anyhow!("{name} must use provider/model format"))?;
        let provider = provider.trim().to_ascii_lowercase();
        let model = model.trim().to_string();
        if provider.is_empty() || model.is_empty() {
            anyhow::bail!("{name} must include both provider and model");
        }
        Ok(Self { provider, model })
    }

    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_model_refs() {
        let model_ref = ModelRef::parse(" Gemini / gemini-embedding-001 ", "TEST_MODEL").unwrap();
        assert_eq!(model_ref.provider(), "gemini");
        assert_eq!(model_ref.model(), "gemini-embedding-001");
    }

    #[test]
    fn rejects_missing_provider_or_model() {
        assert!(ModelRef::parse("gemini", "TEST_MODEL").is_err());
        assert!(ModelRef::parse("/model", "TEST_MODEL").is_err());
        assert!(ModelRef::parse("gemini/", "TEST_MODEL").is_err());
    }
}
