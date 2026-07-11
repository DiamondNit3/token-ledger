use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::model::{Client, LineRecord, ParseBatch, SourceSpec};

pub mod claude;
pub mod codex;

pub trait SourceAdapter: Send + Sync {
    fn client(&self) -> Client;
    fn display_name(&self) -> &'static str;
    fn discover(&self, config: &Config) -> Result<Vec<SourceSpec>>;
    fn parse_lines(
        &self,
        path: &Path,
        lines: &[LineRecord],
        previous_state: Option<&Value>,
    ) -> Result<ParseBatch>;
}

pub fn built_in_adapters() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter),
        Box::new(codex::CodexAdapter),
    ]
}
