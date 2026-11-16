use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Section {
    pub header: String,
    pub guidance: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MeetingTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub sections: Vec<Section>,
}
impl MeetingTemplate {
    pub fn section_names(&self) -> Vec<&str> {
        self.sections.iter().map(|s| s.header.as_str()).collect()
    }
    pub fn sections_markdown(&self) -> String {
        self.sections
            .iter()
            .map(|s| {
                if s.guidance.is_empty() {
                    format!("## {}", s.header)
                } else {
                    format!("## {}\n{}", s.header, s.guidance)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
pub const DEFAULT_TEMPLATE_ID: &str = "general";
pub const MAX_TEMPLATE_BYTES: u64 = 64 * 1024;

pub fn parse_template(id: &str, text: &str) -> MeetingTemplate {
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<_> = normalized.split('\n').collect();
    let mut meta = BTreeMap::new();
    if lines.first().is_some_and(|l| crate::trim(l) == "---") {
        if let Some(end) = (1..lines.len()).find(|&i| crate::trim(lines[i]) == "---") {
            for line in &lines[1..end] {
                if let Some((k, v)) = line.split_once(':') {
                    meta.insert(crate::trim(k).to_lowercase(), crate::trim(v).to_string());
                }
            }
            lines = lines[end + 1..].to_vec();
        }
    }
    let mut intro = Vec::new();
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in lines {
        if let Some(header) = line.strip_prefix("## ") {
            sections.push((crate::trim(header).into(), Vec::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push(line);
        } else {
            intro.push(line);
        }
    }
    MeetingTemplate {
        id: id.into(),
        name: meta
            .get("name")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or(id.into()),
        description: meta.get("description").cloned().unwrap_or_default(),
        instructions: crate::trim(&intro.join("\n")).into(),
        sections: sections
            .into_iter()
            .filter(|(h, _)| !h.is_empty())
            .map(|(header, body)| Section {
                header,
                guidance: crate::trim(&body.join("\n")).into(),
            })
            .collect(),
    }
}
pub fn load_templates(user_dir: Option<&Path>) -> BTreeMap<String, MeetingTemplate> {
    let mut templates = BTreeMap::new();
    for (id, text) in [
        ("general", include_str!("../templates/general.md")),
        ("one-on-one", include_str!("../templates/one-on-one.md")),
        ("standup", include_str!("../templates/standup.md")),
    ] {
        templates.insert(id.into(), parse_template(id, text));
    }
    if let Some(dir) = user_dir {
        let dir = crate::expand(dir);
        let mut paths: Vec<_> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| {
                    if cfg!(windows) {
                        e.to_string_lossy().eq_ignore_ascii_case("md")
                    } else {
                        e == "md"
                    }
                })
            })
            .collect();
        if cfg!(windows) {
            paths.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        } else {
            paths.sort();
        }
        for path in paths {
            if path.metadata().is_ok_and(|m| m.len() <= MAX_TEMPLATE_BYTES) {
                if let Ok(text) = crate::read_text(&path) {
                    let id = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase();
                    let template = parse_template(&id, &text);
                    if !template.sections.is_empty() {
                        templates.insert(id, template);
                    }
                }
            }
        }
    }
    templates
}
pub fn resolve_template(id: Option<&str>, user_dir: Option<&Path>) -> MeetingTemplate {
    let mut templates = load_templates(user_dir);
    id.and_then(|id| templates.remove(id))
        .or_else(|| templates.remove(DEFAULT_TEMPLATE_ID))
        .unwrap()
}
