//! Images in pi entries are inline base64 (`{type:"image", data, mimeType}`), which makes
//! a thread with a few screenshots megabytes to ship on every open. The service keeps the
//! bytes in a content-addressed `blobs` table instead: entries are stored and sent to
//! clients with `blob: <sha256>` in place of `data` (`strip`), and inflated back to pi's
//! form only when a thread is replayed into the runtime (`inflate`). Identical images share
//! one row. The walk is generic over the JSON so it covers user messages, tool results and
//! whatever else pi puts images in.
//!
//! A reply can also *show* an image: the model writes `![caption](/path/on/host.png)` (the
//! runtime's system prompt tells it so) and `link_images` reads the file into `blobs` here,
//! on the host where it lives, recording its id on the text block so any client can fetch
//! it (`dirigent_protocol::images`). `inflate` drops that record again.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use dirigent_protocol::images::image_targets;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Files larger than this are not shown (a link to one is left as it is).
const MAX_LINKED_IMAGE: u64 = 16 * 1024 * 1024;

/// Replace every inline image's `data` with a `blob` id, handing the bytes to `store`.
pub fn strip(value: &mut Value, store: &mut impl FnMut(&str, &str, &[u8])) {
    walk(value, &mut |obj| {
        let Some(data) = obj.get("data").and_then(Value::as_str) else { return };
        let Ok(bytes) = B64.decode(data) else { return };
        let id = blob_id(&bytes);
        let mime = obj.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream").to_owned();
        store(&id, &mime, &bytes);
        obj.remove("data");
        obj.insert("blob".into(), Value::String(id));
    });
}

/// For every markdown image link in an assistant message's text to a local image file
/// (absolute, or relative to `cwd`), hand the file's bytes to `store` and record the
/// link's blob id in the text block's `blobs` (link target → id). The text is left as
/// written. Links to URLs, missing files, non-images or files over `MAX_LINKED_IMAGE`
/// get no blob.
pub fn link_images(value: &mut Value, cwd: &Path, store: &mut impl FnMut(&str, &str, &[u8])) {
    for block in assistant_text_blocks(value) {
        let Some(text) = block.get("text").and_then(Value::as_str) else { continue };
        let mut blobs = Map::new();
        for target in image_targets(text) {
            let path = &text[target];
            if blobs.contains_key(path) {
                continue;
            }
            if let Some((id, mime, bytes)) = read_image(cwd, path) {
                store(&id, mime, &bytes);
                blobs.insert(path.to_owned(), Value::String(id));
            }
        }
        if !blobs.is_empty() {
            block.insert("blobs".into(), Value::Object(blobs));
        }
    }
}

/// The text blocks of `value`'s message, if it is an assistant's.
fn assistant_text_blocks(value: &mut Value) -> impl Iterator<Item = &mut Map<String, Value>> {
    let msg = value.get_mut("message").filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"));
    let blocks = msg.and_then(|m| m.get_mut("content")).and_then(Value::as_array_mut);
    blocks.into_iter().flatten().filter_map(Value::as_object_mut).filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
}

/// `(blob id, mime, bytes)` of the image file at `path`, if it is one we can show.
fn read_image(cwd: &Path, path: &str) -> Option<(String, &'static str, Vec<u8>)> {
    if path.is_empty() || path.contains("://") || path.starts_with("blob:") || path.starts_with("data:") {
        return None;
    }
    let full = cwd.join(path);
    let mime = match full.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    };
    if std::fs::metadata(&full).ok()?.len() > MAX_LINKED_IMAGE {
        return None;
    }
    let bytes = std::fs::read(&full).ok()?;
    Some((blob_id(&bytes), mime, bytes))
}

fn blob_id(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The inverse, the entry as pi wrote it: put `data` back for every `blob` that `load`
/// knows (unknown ones get an empty `data`: pi would reject a missing one) and drop the
/// text blocks' `blobs`.
pub fn inflate(value: &mut Value, load: &mut impl FnMut(&str) -> Option<Vec<u8>>) {
    for block in assistant_text_blocks(value) {
        block.remove("blobs");
    }
    walk(value, &mut |obj| {
        let Some(id) = obj.get("blob").and_then(Value::as_str) else { return };
        match load(id) {
            Some(bytes) => {
                obj.insert("data".into(), Value::String(B64.encode(bytes)));
            }
            None => {
                obj.insert("data".into(), Value::String(String::new()));
            }
        }
        obj.remove("blob");
    });
}

/// Depth-first over the JSON, calling `f` on every object with `type == "image"`.
fn walk(value: &mut Value, f: &mut impl FnMut(&mut serde_json::Map<String, Value>)) {
    match value {
        Value::Object(obj) => {
            if obj.get("type").and_then(Value::as_str) == Some("image") {
                f(obj);
            }
            for v in obj.values_mut() {
                walk(v, f);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| walk(v, f)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_then_inflate_roundtrips() {
        let original = json!({"type":"message","message":{"role":"user","content":[
            {"type":"text","text":"hi"},
            {"type":"image","data":"AQID","mimeType":"image/png"},
            {"type":"image","data":"AQID","mimeType":"image/png"},
        ]}});
        let mut v = original.clone();
        let mut stored = vec![];
        strip(&mut v, &mut |id, mime, bytes| stored.push((id.to_owned(), mime.to_owned(), bytes.to_vec())));
        // Same bytes, same id; `data` is gone from the wire form.
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0], stored[1]);
        assert_eq!(stored[0].2, [1, 2, 3]);
        let block = &v["message"]["content"][1];
        assert_eq!(block["blob"], stored[0].0);
        assert!(block.get("data").is_none());
        assert_eq!(v["message"]["content"][0], json!({"type":"text","text":"hi"}));

        inflate(&mut v, &mut |id| (id == stored[0].0).then(|| vec![1, 2, 3]));
        assert_eq!(v, original);
    }

    #[test]
    fn links_to_local_images_get_blobs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), [1, 2, 3]).unwrap();
        let abs = dir.path().join("shot.png").display().to_string();
        let text = format!(
            "Here:\n\n![home](shot.png)\n![again]({abs} \"t\")\n![twice](shot.png)\nnot ![a](missing.png) nor ![b](https://x/y.png) nor ![c](notes.txt)"
        );
        let original = json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":text}]}});
        let mut v = original.clone();
        let mut stored = vec![];
        link_images(&mut v, dir.path(), &mut |id, mime, bytes| stored.push((id.to_owned(), mime.to_owned(), bytes.to_vec())));
        // Each link target is read once; two targets naming the same file store it twice
        // (the table ignores duplicates).
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0], stored[1]);
        assert_eq!((stored[0].1.as_str(), stored[0].2.as_slice()), ("image/png", &[1, 2, 3][..]));
        // The text stays as the model wrote it; the block records the blobs.
        let id = &stored[0].0;
        assert_eq!(v["message"]["content"][0]["text"], text);
        assert_eq!(v["message"]["content"][0]["blobs"], json!({"shot.png": id, abs: id}));
        // The replay gets back what pi wrote.
        inflate(&mut v, &mut |_| None);
        assert_eq!(v, original);
        // Only assistant text is looked at.
        let mut u = json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"![u](shot.png)"}]}});
        link_images(&mut u, dir.path(), &mut |_, _, _| panic!("user text is not linked"));
        assert!(u["message"]["content"][0].get("blobs").is_none());
    }
}
