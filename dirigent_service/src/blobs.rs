//! Images in pi entries are inline base64 (`{type:"image", data, mimeType}`), which makes
//! a thread with a few screenshots megabytes to ship on every open. The service keeps the
//! bytes in a content-addressed `blobs` table instead: entries are stored and sent to
//! clients with `blob: <sha256>` in place of `data` (`strip`), and inflated back to pi's
//! form only when a thread is replayed into the runtime (`inflate`). Identical images share
//! one row. The walk is generic over the JSON so it covers user messages, tool results and
//! whatever else pi puts images in.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Replace every inline image's `data` with a `blob` id, handing the bytes to `store`.
pub fn strip(value: &mut Value, store: &mut impl FnMut(&str, &str, &[u8])) {
    walk(value, &mut |obj| {
        let Some(data) = obj.get("data").and_then(Value::as_str) else { return };
        let Ok(bytes) = B64.decode(data) else { return };
        let id = format!("{:x}", Sha256::digest(&bytes));
        let mime = obj.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream").to_owned();
        store(&id, &mime, &bytes);
        obj.remove("data");
        obj.insert("blob".into(), Value::String(id));
    });
}

/// The inverse: put `data` back for every `blob` that `load` knows. Unknown blobs are
/// dropped from the block (pi would reject a missing `data`).
pub fn inflate(value: &mut Value, load: &mut impl FnMut(&str) -> Option<Vec<u8>>) {
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
}
