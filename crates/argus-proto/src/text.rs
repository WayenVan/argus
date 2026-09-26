//! Bounds on the free text hook events carry, so one event always fits in a
//! frame whatever the agent was asked or answered.

use serde_json::Value;

/// Longest prompt or reply kept, in bytes; the rest is cut off.
pub const MAX_TEXT: usize = 64 * 1024;

/// Hook event fields that hold free text: a turn's prompt and final reply.
pub const TEXT_FIELDS: &[&str] = &["prompt", "last_assistant_message"];

const CUT_MARK: &str = "\n[… cut off by argus]";

/// Cuts `text` down to [`MAX_TEXT`] bytes at a character boundary.
pub fn clip(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(CUT_MARK);
    }
    text
}

/// Clips the [`TEXT_FIELDS`] of a hook event in place.
pub fn clip_event(event: &mut Value) {
    let Some(fields) = event.as_object_mut() else { return };
    for key in TEXT_FIELDS {
        if let Some(Value::String(text)) = fields.get_mut(*key)
            && text.len() > MAX_TEXT
        {
            *text = clip(std::mem::take(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clips_long_text_on_a_char_boundary() {
        assert_eq!(clip("short".into()), "short");
        let clipped = clip("é".repeat(MAX_TEXT)); // 2 bytes each
        assert!(clipped.len() <= MAX_TEXT + CUT_MARK.len());
        assert!(clipped.ends_with(CUT_MARK));
    }

    #[test]
    fn clips_only_text_fields() {
        let long = "x".repeat(2 * MAX_TEXT);
        let mut event = json!({"prompt": long, "last_assistant_message": long, "cwd": long, "v": 1});
        clip_event(&mut event);
        assert!(event["prompt"].as_str().unwrap().ends_with(CUT_MARK));
        assert!(event["last_assistant_message"].as_str().unwrap().ends_with(CUT_MARK));
        assert_eq!(event["cwd"].as_str().unwrap().len(), 2 * MAX_TEXT, "left alone");

        let mut odd = json!({"prompt": null, "last_assistant_message": 3});
        clip_event(&mut odd);
        assert_eq!(odd, json!({"prompt": null, "last_assistant_message": 3}));
        clip_event(&mut json!([1, 2]));
    }
}
