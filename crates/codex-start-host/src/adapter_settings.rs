//! Edit one VS Code JSONC setting without replacing unrelated text.

use crate::error::{HostError, Result};
use serde_json::Value;

const KEY: &str = "chatgpt.cliExecutable";

pub fn value(text: &str) -> Result<Option<Value>> {
    let (clean, _) = parse(text)?;
    let value: Value = serde_json::from_str(&clean).map_err(invalid)?;
    Ok(value.get(KEY).cloned())
}

pub fn update(text: &str, value: Option<&Value>) -> Result<String> {
    let (clean, raw) = parse(text)?;
    let mut cursor = skip(&clean, 0) + 1;
    let mut previous_comma = None;
    let mut found = None;
    while clean.as_bytes()[skip(&clean, cursor)] != b'}' {
        cursor = skip(&clean, cursor);
        let start = cursor;
        let (key, end) = read_value(&clean, cursor)?;
        cursor = skip(&clean, end) + 1; // Colon; the whole document was validated.
        cursor = skip(&clean, cursor);
        let value_start = cursor;
        let (_, end) = read_value(&clean, cursor)?;
        let next = skip(&raw, end);
        let following_comma = (raw.as_bytes()[next] == b',').then_some(next);
        if key.as_str() == Some(KEY) {
            if found.is_some() {
                return Err(HostError::Config(
                    "duplicate chatgpt.cliExecutable settings".into(),
                ));
            }
            found = Some((start, value_start, end, previous_comma, following_comma));
        }
        cursor = skip(&clean, end);
        if clean.as_bytes()[cursor] == b',' {
            previous_comma = Some(cursor);
            cursor += 1;
        }
    }
    let mut result = text.to_owned();
    match (found, value) {
        (Some((_, start, end, _, _)), Some(value)) => {
            result.replace_range(start..end, &value.to_string());
        }
        (Some((start, _, end, before, after)), None) => {
            let (start, end) = if let Some(after) = after {
                (start, after + 1)
            } else {
                (before.unwrap_or(start), end)
            };
            result.replace_range(start..end, "");
        }
        (None, Some(value)) => {
            let object: Value = serde_json::from_str(&clean).map_err(invalid)?;
            let comma = if object.as_object().expect("validated object").is_empty() {
                ""
            } else {
                ","
            };
            let start = skip(&clean, 0) + 1;
            result.insert_str(start, &format!("\n  \"{KEY}\": {value}{comma}\n"));
        }
        (None, None) => {}
    }
    parse(&result)?;
    Ok(result)
}

fn invalid(error: impl std::fmt::Display) -> HostError {
    HostError::Config(format!("invalid VS Code settings JSONC: {error}"))
}

fn skip(text: &str, mut cursor: usize) -> usize {
    while text
        .as_bytes()
        .get(cursor)
        .is_some_and(u8::is_ascii_whitespace)
    {
        cursor += 1;
    }
    cursor
}

fn read_value(text: &str, start: usize) -> Result<(Value, usize)> {
    let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
    let value = stream
        .next()
        .ok_or_else(|| invalid("missing value"))?
        .map_err(invalid)?;
    Ok((value, start + stream.byte_offset()))
}

fn parse(text: &str) -> Result<(String, String)> {
    let mut bytes = text.as_bytes().to_vec();
    let mut cursor = 0;
    let mut string = false;
    while cursor < bytes.len() {
        if string {
            match bytes[cursor] {
                b'\\' => cursor += 1,
                b'"' => string = false,
                _ => {}
            }
        } else if bytes[cursor] == b'"' {
            string = true;
        } else if bytes[cursor..].starts_with(b"//") {
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                bytes[cursor] = b' ';
                cursor += 1;
            }
            continue;
        } else if bytes[cursor..].starts_with(b"/*") {
            bytes[cursor] = b' ';
            bytes[cursor + 1] = b' ';
            cursor += 2;
            while cursor + 1 < bytes.len() && !bytes[cursor..].starts_with(b"*/") {
                bytes[cursor] = b' ';
                cursor += 1;
            }
            if cursor + 1 >= bytes.len() {
                return Err(invalid("unclosed comment"));
            }
            bytes[cursor] = b' ';
            bytes[cursor + 1] = b' ';
            cursor += 2;
            continue;
        }
        cursor += 1;
    }
    let raw = String::from_utf8(bytes.clone()).map_err(invalid)?;
    string = false;
    cursor = 0;
    while cursor < bytes.len() {
        if string {
            match bytes[cursor] {
                b'\\' => cursor += 1,
                b'"' => string = false,
                _ => {}
            }
        } else if bytes[cursor] == b'"' {
            string = true;
        } else if bytes[cursor] == b',' {
            let next = skip(&raw, cursor + 1);
            if bytes.get(next).is_some_and(|b| matches!(b, b'}' | b']')) {
                bytes[cursor] = b' ';
            }
        }
        cursor += 1;
    }
    let clean = String::from_utf8(bytes).map_err(invalid)?;
    let document: Value = serde_json::from_str(&clean).map_err(invalid)?;
    if !document.is_object() {
        return Err(invalid("settings must be an object"));
    }
    Ok((clean, raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_comments_nested_values_and_user_changes() {
        let original = "{\n // Keep this comment\n \"nested\": {\"chatgpt.cliExecutable\":\"ignore\",},\n \"url\": \"https://example.com/*x*/\",\n \"chatgpt.cliExecutable\": null,\n}";
        let installed = update(original, Some(&json!("/a path/adapter"))).unwrap();
        assert!(installed.contains("// Keep this comment"));
        assert!(installed.contains("\"url\": \"https://example.com/*x*/\""));
        assert_eq!(value(&installed).unwrap(), Some(json!("/a path/adapter")));
        assert_eq!(update(&installed, Some(&Value::Null)).unwrap(), original);
        assert!(value(&update(&installed, None).unwrap()).unwrap().is_none());
    }

    #[test]
    fn inserts_and_removes_at_all_object_positions() {
        for original in ["{}", "{/*note*/}", "{\"x\":1}", "{\"x\":1,}"] {
            let installed = update(original, Some(&json!("adapter"))).unwrap();
            assert_eq!(value(&installed).unwrap(), Some(json!("adapter")));
            assert!(value(&update(&installed, None).unwrap()).unwrap().is_none());
        }
        for text in [
            "{\"chatgpt.cliExecutable\":1,\"x\":2}",
            "{\"x\":2,\"chatgpt.cliExecutable\":1}",
        ] {
            assert!(value(&update(text, None).unwrap()).unwrap().is_none());
        }
        assert!(update("{broken", Some(&json!("adapter"))).is_err());
        assert!(
            update(
                "{\"chatgpt.cliExecutable\":1,\"chatgpt.cliExecutable\":2}",
                None
            )
            .is_err()
        );
    }
}
