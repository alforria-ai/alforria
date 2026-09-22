//! `skill.ts` — openapi `SkillV2*`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash: Option<bool>,
    pub location: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum SkillSource {
    Directory {
        /// AbsolutePath
        path: String,
    },
    Url {
        url: String,
    },
    Embedded {
        skill: Box<SkillInfo>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn embedded_source_with_nested_skill_round_trips() {
        let value = json!({
            "type": "embedded",
            "skill": {
                "name": "nested",
                "description": "A nested skill",
                "slash": true,
                "location": "/repo/.opencode/skills/nested",
                "content": "# Nested"
            }
        });

        let source: SkillSource = serde_json::from_value(value.clone()).unwrap();
        let SkillSource::Embedded { skill } = &source else {
            panic!("expected embedded source");
        };
        assert_eq!(skill.name, "nested");
        assert_eq!(serde_json::to_value(&source).unwrap(), value);
    }

    #[test]
    fn directory_source_round_trips() {
        let value = json!({ "type": "directory", "path": "/repo/.opencode/skills" });
        let source: SkillSource = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&source).unwrap(), value);
    }

    #[test]
    fn skill_info_omits_optional_keys() {
        let value = json!({
            "name": "build",
            "location": "/repo/.opencode/skills/build",
            "content": "# Build"
        });
        let info: SkillInfo = serde_json::from_value(value).unwrap();
        let out = serde_json::to_value(&info).unwrap();
        assert!(out.get("description").is_none());
        assert!(out.get("slash").is_none());
    }
}
