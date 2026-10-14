// SPDX-License-Identifier: Apache-2.0
//! File kinds: what a document *is* (a training config, a recipe, a log) as opposed
//! to its language. Kinds pick the schema, the editability and the editor budget.

use crate::syntax::Language;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum FileKind {
    TrainConfig,
    RunSpec,
    JobSpec,
    Recipe,
    Pipeline,
    HubEntry,
    BenchmarkSpec,
    Settings,
    PluginManifest,
    Prompt,
    Log,
    Json,
    Yaml,
    Toml,
    Markdown,
    Python,
    Text,
}

impl FileKind {
    /// Detects the kind from a path (name and extension) and the first bytes of content.
    pub fn detect(path: &str, head: &str) -> Self {
        let lower = format!("/{}", path.to_ascii_lowercase().replace('\\', "/"));
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        let ext = name.rsplit('.').next().unwrap_or("");
        let is_yaml = matches!(ext, "yaml" | "yml");
        if name == "frontier.toml" {
            return Self::Settings;
        }
        if name == "plugin.toml" || name == "extension.toml" {
            return Self::PluginManifest;
        }
        if name.ends_with(".recipe.json") || lower.contains("/recipes/") && ext == "json" {
            return Self::Recipe;
        }
        if name.ends_with(".pipeline.json") || head.contains("\"frontier-pipeline\"") {
            return Self::Pipeline;
        }
        if name.starts_with("runspec")
            || name.ends_with(".run.yaml")
            || name.ends_with(".run.yml")
            || head.contains("frontier.run/v1")
        {
            return Self::RunSpec;
        }
        if name.starts_with("jobspec") || name.ends_with(".job.yaml") || name.ends_with(".job.json")
        {
            return Self::JobSpec;
        }
        if lower.contains("/catalog/") && is_yaml {
            return Self::HubEntry;
        }
        if lower.contains("benchmark") && is_yaml {
            return Self::BenchmarkSpec;
        }
        if (lower.contains("/configs/") || lower.contains("frontier-train")) && is_yaml {
            return Self::TrainConfig;
        }
        if lower.contains("/prompts/") && ext == "md" {
            return Self::Prompt;
        }
        match ext {
            "log" | "out" | "err" => Self::Log,
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "md" | "markdown" => Self::Markdown,
            "py" => Self::Python,
            _ => Self::Text,
        }
    }

    pub fn language(self) -> Language {
        match self {
            Self::TrainConfig
            | Self::RunSpec
            | Self::HubEntry
            | Self::BenchmarkSpec
            | Self::Yaml => Language::Yaml,
            Self::JobSpec => Language::Yaml,
            Self::Recipe | Self::Pipeline | Self::Json => Language::Json,
            Self::Settings | Self::PluginManifest | Self::Toml => Language::Toml,
            Self::Prompt | Self::Markdown => Language::Markdown,
            Self::Python => Language::Python,
            Self::Log | Self::Text => Language::Plain,
        }
    }

    /// Kinds whose schema we own and that may be edited in the app.
    pub fn is_editable(self) -> bool {
        !matches!(self, Self::Log | Self::Python)
    }

    /// Schema id used by the registry (`<id>.schema.json` in the spec schemas).
    pub fn schema_id(self) -> Option<&'static str> {
        Some(match self {
            Self::TrainConfig => "frontier-train-config",
            Self::RunSpec => "runspec",
            Self::JobSpec => "jobspec",
            Self::Recipe => "recipe",
            Self::Pipeline => "pipeline-v1",
            Self::HubEntry => "hub-entry",
            Self::BenchmarkSpec => "benchmark-spec",
            Self::Settings => "frontier-config",
            Self::PluginManifest => "plugin-manifest",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        assert_eq!(FileKind::detect("x/frontier.toml", ""), FileKind::Settings);
        assert_eq!(
            FileKind::detect("runs/abc/runspec.yaml", ""),
            FileKind::RunSpec
        );
        assert_eq!(
            FileKind::detect("a.yaml", "apiVersion: frontier.run/v1"),
            FileKind::RunSpec
        );
        assert_eq!(
            FileKind::detect("ds/recipes/clean.json", ""),
            FileKind::Recipe
        );
        assert_eq!(
            FileKind::detect("packages/frontier-train/configs/a.yaml", ""),
            FileKind::TrainConfig
        );
        assert_eq!(
            FileKind::detect("catalog/cfd/ahmed.yaml", ""),
            FileKind::HubEntry
        );
        assert_eq!(FileKind::detect("job.log", ""), FileKind::Log);
        assert_eq!(FileKind::detect("notes.md", ""), FileKind::Markdown);
        assert_eq!(FileKind::Log.language(), Language::Plain);
    }
}
