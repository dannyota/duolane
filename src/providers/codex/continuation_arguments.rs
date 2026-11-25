use std::collections::HashSet;
use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

pub(super) fn arguments_equal(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn parse(text: &str) -> Option<Value> {
    serde_json::from_str::<Unique>(text).ok()?;
    if !numbers_are_safe(text) {
        return None;
    }
    serde_json::from_str(text).ok()
}

struct Unique;

impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = Unique;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("JSON without repeated object keys")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Unique, E> {
        Ok(Unique)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Unique, E> {
        Ok(Unique)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Unique, E> {
        Ok(Unique)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Unique, E> {
        Ok(Unique)
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Unique, E> {
        Ok(Unique)
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
        Ok(Unique)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
        while seq.next_element::<Unique>()?.is_some() {}
        Ok(Unique)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(serde::de::Error::custom("repeated object key"));
            }
            map.next_value::<Unique>()?;
        }
        Ok(Unique)
    }
}

fn numbers_are_safe(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else if bytes[i] == b'-' || bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E'))
            {
                i += 1;
            }
            if !number_is_safe(&text[start..i]) {
                return false;
            }
        } else {
            i += 1;
        }
    }
    true
}

fn number_is_safe(number: &str) -> bool {
    if !number.contains(['.', 'e', 'E']) {
        return number
            .parse::<i64>()
            .is_ok_and(|n| n.unsigned_abs() <= 1_u64 << 53);
    }
    let Ok(value) = number.parse::<f64>() else {
        return false;
    };
    if !value.is_finite() {
        return false;
    }
    let Some(original) = normalized(number) else {
        return false;
    };
    original.0.trim_start_matches('-').len() <= 15
        && normalized(&value.to_string()).is_some_and(|roundtrip| original == roundtrip)
}

fn normalized(number: &str) -> Option<(String, i64)> {
    let (mantissa, exponent) = match number.find(['e', 'E']) {
        Some(i) => (&number[..i], number[i + 1..].parse::<i64>().ok()?),
        None => (number, 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Some(("0".into(), 0));
    }
    digits = trimmed.to_owned();
    let trailing = digits.len() - digits.trim_end_matches('0').len();
    digits.truncate(digits.len() - trailing);
    if negative {
        digits.insert(0, '-');
    }
    Some((
        digits,
        exponent
            .checked_sub(i64::try_from(fraction).ok()?)?
            .checked_add(i64::try_from(trailing).ok()?)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::arguments_equal;

    #[test]
    fn argument_text_and_json_equality() {
        assert!(arguments_equal("not json", "not json"));
        assert!(arguments_equal(
            r#"{"a":1,"b":[{"x":2}]}"#,
            r#"{ "b": [ { "x": 2 } ], "a": 1 }"#
        ));
        assert!(arguments_equal(
            r#"{"s":"123 \\\" 1.0000000000000000001"}"#,
            r#"{ "s": "123 \\\" 1.0000000000000000001" }"#
        ));
        assert!(!arguments_equal("not json", "not json "));
    }

    #[test]
    fn imprecise_numbers_and_duplicate_keys_do_not_match() {
        assert!(!arguments_equal(
            r#"{"n":1.0000000000000000001}"#,
            r#"{"n":1.0}"#
        ));
        assert!(!arguments_equal(
            r#"{"n":9007199254740993}"#,
            r#"{"n":9007199254740992}"#
        ));
        assert!(!arguments_equal(
            r#"{"x":[{"a":1,"a":2}]}"#,
            r#"{"x":[{"a":2}]}"#
        ));
        assert!(!arguments_equal(
            r#"{"x":[{"a":2}]}"#,
            r#"{"x":[{"a":1,"a":2}]}"#
        ));
        assert!(!arguments_equal(r#"{"n":1e-324}"#, r#"{"n":0.0}"#));
        assert!(arguments_equal(r#"{"n":0.1}"#, r#"{ "n": 0.1 }"#));
    }
}
