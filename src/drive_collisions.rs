//! Read-only Google Drive identity-collision analysis.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

const FOLDER: &str = "application/vnd.google-apps.folder";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Object {
    id: String,
    name: String,
    mime_type: String,
    #[serde(default)]
    parents: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct Report {
    pub(crate) objects: usize,
    pub(crate) file_groups: usize,
    pub(crate) file_objects: usize,
    pub(crate) folder_groups: usize,
    pub(crate) folder_objects: usize,
    pub(crate) cross_type_groups: usize,
    pub(crate) hidden_descendant_files: usize,
    warning: &'static str,
}
impl Report {
    pub(crate) fn has_collisions(&self) -> bool {
        self.file_groups > 0 || self.folder_groups > 0 || self.cross_type_groups > 0
    }
}
fn output_name(item: &Object) -> String {
    let suffix = match item.mime_type.as_str() {
        "application/vnd.google-apps.document" => ".docx",
        "application/vnd.google-apps.spreadsheet" => ".xlsx",
        "application/vnd.google-apps.presentation" => ".pptx",
        "application/vnd.google-apps.drawing" => ".svg",
        _ => "",
    };
    format!("{}{suffix}", item.name)
}
fn collision_groups(groups: &HashMap<(String, String), Vec<String>>) -> (usize, usize) {
    let many: Vec<_> = groups.values().filter(|items| items.len() > 1).collect();
    (many.len(), many.iter().map(|items| items.len()).sum())
}

/// Work with immutable Google object IDs, not path-resolved rclone listings.
pub(crate) fn summarize_json(raw: &[u8]) -> Result<Report> {
    let objects: Vec<Object> = serde_json::from_slice(raw)?;
    let mut ids = HashMap::new();
    for obj in objects {
        ensure!(!obj.id.is_empty() && !obj.name.is_empty() && !obj.mime_type.is_empty(),
            "Google object lacks a required identity field");
        ensure!(ids.insert(obj.id.clone(), obj).is_none(),
            "duplicate immutable Google object ID in response");
    }
    if ids.is_empty() {
        bail!("empty Google object inventory; cannot certify uniqueness");
    }
    let mut files: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut folders: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut types: HashMap<(String, String), (bool, bool)> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for obj in ids.values() {
        let parents: Vec<&str> = if obj.parents.is_empty() {
            vec!["<no-parent>"]
        } else {
            obj.parents.iter().map(String::as_str).collect()
        };
        for parent in parents {
            children.entry(parent.to_owned()).or_default().push(obj.id.clone());
            let key = (parent.to_owned(), output_name(obj));
            let family = types.entry(key.clone()).or_default();
            if obj.mime_type == FOLDER {
                family.0 = true;
                folders.entry(key).or_default().push(obj.id.clone());
            } else {
                family.1 = true;
                files.entry(key).or_default().push(obj.id.clone());
            }
        }
    }

    let duplicate_folders: HashSet<_> = folders.values()
        .filter(|siblings| siblings.len() > 1)
        .flat_map(|siblings| siblings.iter().cloned())
        .collect();
    let mut queue: VecDeque<String> = duplicate_folders.iter().cloned().collect();
    let mut visited_folders = HashSet::new();
    let mut hidden_files = HashSet::new();
    while let Some(folder_id) = queue.pop_front() {
        if !visited_folders.insert(folder_id.clone()) {
            continue;
        }
        for child in children.get(&folder_id).into_iter().flatten() {
            if let Some(item) = ids.get(child) {
                if item.mime_type == FOLDER {
                    queue.push_back(child.clone());
                } else {
                    hidden_files.insert(child.clone());
                }
            }
        }
    }

    let (file_groups, file_objects) = collision_groups(&files);
    let (folder_groups, folder_objects) = collision_groups(&folders);
    Ok(Report {
        objects: ids.len(), file_groups, file_objects, folder_groups, folder_objects,
        cross_type_groups: types.values().filter(|(a, b)| *a && *b).count(),
        hidden_descendant_files: hidden_files.len(),
        warning: "Based on provider response and default Google Docs export extensions; incomplete listings and custom exports may hide further collisions.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_files_folders_export_collisions_and_hidden_children() {
        let input = serde_json::json!([
            {"id":"f1","name":"Dup","mimeType":FOLDER,"parents":["root"]},
            {"id":"f2","name":"Dup","mimeType":FOLDER,"parents":["root"]},
            {"id":"sub","name":"nested","mimeType":FOLDER,"parents":["f2"]},
            {"id":"child","name":"inside.jpg","mimeType":"image/jpeg","parents":["sub"]},
            {"id":"a","name":"same.txt","mimeType":"text/plain","parents":["root"]},
            {"id":"b","name":"same.txt","mimeType":"text/plain","parents":["root"]},
            {"id":"c","name":"doc","mimeType":"application/vnd.google-apps.document","parents":["root"]},
            {"id":"d","name":"doc.docx","mimeType":"application/octet-stream","parents":["root"]}
        ]);
        let result = summarize_json(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(result.objects, 8);
        assert_eq!((result.folder_groups, result.folder_objects), (1, 2));
        assert_eq!((result.file_groups, result.file_objects), (2, 4));
        assert_eq!(result.hidden_descendant_files, 1);
        assert!(result.has_collisions());
    }
    #[test]
    fn file_and_folder_same_name_collide() {
        let input = serde_json::json!([
            {"id":"folder","name":"same","mimeType":FOLDER,"parents":["root"]},
            {"id":"file","name":"same","mimeType":"text/plain","parents":["root"]}
        ]);
        let report = summarize_json(&serde_json::to_vec(&input).unwrap()).unwrap();
        assert_eq!(report.cross_type_groups, 1);
    }
    #[test]
    fn same_names_different_parents_do_not_collide() {
        let input = serde_json::json!([
            {"id":"a","name":"same.txt","mimeType":"text/plain","parents":["one"]},
            {"id":"b","name":"same.txt","mimeType":"text/plain","parents":["two"]}
        ]);
        assert!(!summarize_json(&serde_json::to_vec(&input).unwrap()).unwrap().has_collisions());
    }
    #[test]
    fn reject_invalid_and_empty_object_inventories() {
        assert!(summarize_json(b"[]").is_err());
        assert!(summarize_json(br#"[{"id":"x","name":"a","mimeType":"text/plain"},{"id":"x","name":"b","mimeType":"text/plain"}]"#).is_err());
    }
}
