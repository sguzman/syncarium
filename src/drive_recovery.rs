//! Deterministic, identity-addressed Google Drive recovery plans.
//! Plan only. Actual copyid reads are indivisible and must not bypass the
//! shared Syncarium byte budget or the large-file resume requirements.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};

const FOLDER: &str = "application/vnd.google-apps.folder";
const SHORTCUT: &str = "application/vnd.google-apps.shortcut";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Object {
    id: String,
    name: String,
    mime_type: String,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    size: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecoveryItem {
    /// SHA-256 of the immutable cloud object ID, not of its payload.
    object_fingerprint: String,
    /// Relative to configured source destination. No original names exposed.
    relative_destination: String,
    reason: &'static str,
    source_size: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RecoveryPlan {
    pub(crate) items: Vec<RecoveryItem>,
    pub(crate) known_size_bytes: u64,
    pub(crate) unknown_size: usize,
    pub(crate) unsupported: usize,
    warning: &'static str,
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
fn supported(item: &Object) -> bool {
    if item.mime_type == FOLDER || item.mime_type == SHORTCUT {
        return false;
    }
    if item.mime_type.starts_with("application/vnd.google-apps.") {
        return matches!(item.mime_type.as_str(),
            "application/vnd.google-apps.document" |
            "application/vnd.google-apps.spreadsheet" |
            "application/vnd.google-apps.presentation" |
            "application/vnd.google-apps.drawing");
    }
    true
}
fn expected_size(item: &Object) -> Option<u64> {
    item.size.as_ref().and_then(|value| value.as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok())))
}
fn hashed_id(id: &str) -> String {
    hex::encode(Sha256::digest(id.as_bytes()))
}
fn safe_extension(name: &str) -> String {
    let candidate = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    if !candidate.is_empty()
        && candidate.len() <= 12
        && candidate.bytes().all(|x| x.is_ascii_alphanumeric())
    {
        format!(".{}", candidate.to_ascii_lowercase())
    } else {
        String::new()
    }
}
fn parse_objects(bytes: &[u8]) -> Result<HashMap<String, Object>> {
    let raw: Vec<Object> = serde_json::from_slice(bytes)?;
    if raw.is_empty() {
        bail!("Google object inventory is empty; refusing incomplete plan");
    }
    let mut objects = HashMap::new();
    for obj in raw {
        ensure!(!obj.id.is_empty() && !obj.name.is_empty() && !obj.mime_type.is_empty(),
            "Google Drive object missing ID/name/MIME type");
        ensure!(objects.insert(obj.id.clone(), obj).is_none(),
            "repeated immutable Google ID in source listing");
    }
    Ok(objects)
}

/// Identify every file whose path is ambiguous because a sibling file/folder
/// output name collides, or because it descends from an ambiguous folder.
pub(crate) fn plan_recovery_json(raw: &[u8]) -> Result<RecoveryPlan> {
    let objects = parse_objects(raw)?;
    let mut files: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut dirs: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut descendants: HashMap<String, Vec<String>> = HashMap::new();

    for obj in objects.values() {
        let parents: Vec<&str> = if obj.parents.is_empty() {
            vec!["<no-parent>"]
        } else {
            obj.parents.iter().map(String::as_str).collect()
        };
        for parent in parents {
            descendants.entry(parent.to_owned()).or_default().push(obj.id.clone());
            let key = (parent.to_owned(), output_name(obj));
            if obj.mime_type == FOLDER {
                dirs.entry(key).or_default().insert(obj.id.clone());
            } else {
                files.entry(key).or_default().insert(obj.id.clone());
            }
        }
    }

    let mut colliding_file_ids = HashSet::new();
    let mut ambiguous_folders = HashSet::new();
    for (key, group) in &files {
        if group.len() > 1 || dirs.contains_key(key) {
            colliding_file_ids.extend(group.iter().cloned());
        }
    }
    for (key, group) in &dirs {
        if group.len() > 1 || files.contains_key(key) {
            ambiguous_folders.extend(group.iter().cloned());
        }
    }

    let mut queue: VecDeque<String> = ambiguous_folders.iter().cloned().collect();
    let mut traversed = HashSet::new();
    let mut hidden_files = HashSet::new();
    while let Some(folder_id) = queue.pop_front() {
        if !traversed.insert(folder_id.clone()) {
            continue;
        }
        for child_id in descendants.get(&folder_id).into_iter().flatten() {
            if let Some(obj) = objects.get(child_id) {
                if obj.mime_type == FOLDER {
                    queue.push_back(obj.id.clone());
                } else {
                    hidden_files.insert(obj.id.clone());
                }
            }
        }
    }
    let ids: HashSet<_> = colliding_file_ids.union(&hidden_files).cloned().collect();
    let mut items = Vec::new();
    let mut unsupported = 0usize;
    let mut unknown_size = 0usize;
    let mut known_size_bytes = 0u64;
    for id in ids {
        let obj = objects.get(&id).expect("selected IDs come from object inventory");
        if !supported(obj) {
            unsupported += 1;
            continue;
        }
        let size = expected_size(obj);
        if let Some(bytes) = size {
            known_size_bytes = known_size_bytes.saturating_add(bytes);
        } else {
            unknown_size += 1;
        }
        let hash = hashed_id(&id);
        // Original path elements are untrusted; never reproduce them as
        // filesystem components. Extensions are conservative display hints.
        let extension = safe_extension(&output_name(obj));
        let relative_destination = format!(
            ".rclone-duplicates/objects/{}/id-{hash}{extension}", &hash[..2]
        );
        items.push(RecoveryItem {
            object_fingerprint: hash,
            relative_destination,
            reason: if colliding_file_ids.contains(&id) {
                "colliding_output_name"
            } else {
                "descendant_of_ambiguous_folder"
            },
            source_size: size,
        });
    }
    items.sort_by(|a, b| a.object_fingerprint.cmp(&b.object_fingerprint));
    Ok(RecoveryPlan {
        items, known_size_bytes, unknown_size, unsupported,
        warning: "Planning only: no downloads, no cloud ID-to-payload verification, no resolved legacy archive imports. This is not a completeness guarantee.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_identity_destinations_and_hidden_descendants() {
        let records = serde_json::json!([
            {"id":"dir-a","name":"Same","mimeType":FOLDER,"parents":["root"]},
            {"id":"dir-b","name":"Same","mimeType":FOLDER,"parents":["root"]},
            {"id":"deep","name":"inner","mimeType":FOLDER,"parents":["dir-b"]},
            {"id":"private child","name":"private.jpg","mimeType":"image/jpeg","parents":["deep"],"size":"2048"},
            {"id":"one","name":"duplicate.mov","mimeType":"video/quicktime","parents":["root"],"size":9000},
            {"id":"two","name":"duplicate.mov","mimeType":"video/quicktime","parents":["root"],"size":9000},
            {"id":"ordinary","name":"unrelated.mp3","mimeType":"audio/mpeg","parents":["root"],"size":"11"}
        ]);
        let mut shuffled = records.as_array().unwrap().clone();
        let original = plan_recovery_json(&serde_json::to_vec(&records).unwrap()).unwrap();
        shuffled.reverse();
        let again = plan_recovery_json(&serde_json::to_vec(&shuffled).unwrap()).unwrap();
        assert_eq!(original.items.len(), 3);
        assert_eq!(original.known_size_bytes, 20_048);
        assert_eq!(original.items.iter().map(|x| &x.relative_destination).collect::<Vec<_>>(),
                   again.items.iter().map(|x| &x.relative_destination).collect::<Vec<_>>());
        for item in original.items {
            assert!(!item.relative_destination.contains("private"));
            assert!(!item.relative_destination.contains("duplicate"));
            assert!(!item.relative_destination.contains(".."));
        }
    }

    #[test]
    fn folder_file_collision_includes_child_file_and_nonfolder() {
        let records = serde_json::json!([
            {"id":"folder","name":"Same","mimeType":FOLDER,"parents":["root"]},
            {"id":"file","name":"Same","mimeType":"application/octet-stream","parents":["root"],"size":"16"},
            {"id":"child","name":"nested.txt","mimeType":"text/plain","parents":["folder"],"size":20}
        ]);
        let plan = plan_recovery_json(&serde_json::to_vec(&records).unwrap()).unwrap();
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.known_size_bytes, 36);
    }

    #[test]
    fn missing_sizes_and_shortcuts_are_not_treated_as_verified_bytes() {
        let records = serde_json::json!([
            {"id":"a","name":"same","mimeType":"application/vnd.google-apps.document","parents":["root"]},
            {"id":"b","name":"same.docx","mimeType":"application/octet-stream","parents":["root"],"size":"8"},
            {"id":"s","name":"same.docx","mimeType":SHORTCUT,"parents":["root"],"size":0}
        ]);
        let plan = plan_recovery_json(&serde_json::to_vec(&records).unwrap()).unwrap();
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.unknown_size, 1);
        assert_eq!(plan.unsupported, 1);
    }

    #[test]
    fn empty_inventory_or_duplicate_ids_fail_closed() {
        assert!(plan_recovery_json(b"[]").is_err());
        assert!(plan_recovery_json(
            br#"[{"id":"x","name":"a","mimeType":"text/plain"},{"id":"x","name":"b","mimeType":"text/plain"}]"#
        ).is_err());
    }
}
