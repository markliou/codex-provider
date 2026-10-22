//! Go's `encoding/json` encoding of a decoded JSON value, byte for byte.
//!
//! The pool hashes and forwards JSON it decoded into Go's `map[string]any`:
//! the fallback routing key hashes `json.Marshal` of the prompt prefix, and a
//! custom-provider request the pool rewrote was re-encoded with
//! `json.Marshal`. Go writes that encoding its own way: object keys sorted
//! bytewise at every level, `<`, `>`, `&`, U+2028 and U+2029 escaped, every
//! number decoded as a float64 and written in Go's shortest form (`1e+21`,
//! `1e-7`, `-0`, and integers past 2^53 rounded). Matching it keeps existing
//! sticky sessions where they are and keeps rewritten requests identical
//! across the switch from Go.

use serde_json::Value;

pub fn marshal(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => write_float(out, number.as_f64().unwrap_or(0.0)),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_value(out, item);
            }
            out.push(b']');
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(out, key);
                out.push(b':');
                if let Some(item) = object.get(key) {
                    write_value(out, item);
                }
            }
            out.push(b'}');
        }
    }
}

/// encoding/json floatEncoder for float64.
fn write_float(out: &mut Vec<u8>, value: f64) {
    let abs = value.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        // strconv 'e' with the shortest digits writes a signed exponent of at
        // least two digits; json then turns e-09 into e-9. Rust's shortest
        // exponent form has neither the + nor the padding.
        let text = format!("{value:e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        out.extend_from_slice(mantissa.as_bytes());
        out.push(b'e');
        if exponent.starts_with('-') {
            out.extend_from_slice(exponent.as_bytes());
        } else {
            out.push(b'+');
            if exponent.len() < 2 {
                out.push(b'0');
            }
            out.extend_from_slice(exponent.as_bytes());
        }
        return;
    }
    out.extend_from_slice(format!("{value}").as_bytes());
}

/// encoding/json string encoding with HTML escaping on.
fn write_string(out: &mut Vec<u8>, text: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for character in text.chars() {
        match character {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '<' | '>' | '&' | '\u{0}'..='\u{1f}' => {
                let code = u32::from(character);
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(code >> 4) as usize]);
                out.push(HEX[(code & 0xf) as usize]);
            }
            '\u{2028}' => out.extend_from_slice(b"\\u2028"),
            '\u{2029}' => out.extend_from_slice(b"\\u2029"),
            _ => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Expected output from Go 1.24: json.Unmarshal into any, then json.Marshal
    // (the first case without its -0, see the module comment).
    #[test]
    fn writes_what_go_writes() {
        let cases = [
            (
                r#"{"b":1,"a":[1.5,1e21,1e-7,0.000001,123456789012345678,2.5e-8,100,1e20,0.1],"c":"<a&b>\u2028\u2029\u0001\b\f\n\r\t\"\\é😀","d":null,"e":true,"Z":{"y":{},"x":[]}}"#,
                r#"{"Z":{"x":[],"y":{}},"a":[1.5,1e+21,1e-7,0.000001,123456789012345680,2.5e-8,100,100000000000000000000,0.1],"b":1,"c":"\u003ca\u0026b\u003e\u2028\u2029\u0001\b\f\n\r\t\"\\é😀","d":null,"e":true}"#,
            ),
            (
                r#"{"input":[{"role":"user","content":"hi"}],"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"z":{"type":"string"},"a":{"type":"number","minimum":-1.25e-10}}}}]}"#,
                r#"{"input":[{"content":"hi","role":"user"}],"tools":[{"name":"f","parameters":{"properties":{"a":{"minimum":-1.25e-10,"type":"number"},"z":{"type":"string"}},"type":"object"},"type":"function"}]}"#,
            ),
            (
                r#"[1e300,-1.7976931348623157e308,5e-324,3.0,12345678901234567890]"#,
                r#"[1e+300,-1.7976931348623157e+308,5e-324,3,12345678901234567000]"#,
            ),
        ];
        for (input, want) in cases {
            let value: Value = serde_json::from_str(input).unwrap_or(Value::Null);
            assert_eq!(String::from_utf8_lossy(&marshal(&value)), want);
        }
    }
}
