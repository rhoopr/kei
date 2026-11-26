//! Reject ambiguous identities while retaining the original response elsewhere.

use super::error::{ChangesBodyRejection, ShadowPageError};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::cell::Cell;

#[derive(Clone, Copy)]
struct UniqueVisitor<'a> {
    duplicate_key: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for UniqueVisitor<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UniqueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an unambiguous JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("Invalid JSON number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(self)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                self.duplicate_key.set(true);
                return Err(de::Error::custom("Duplicate JSON object key"));
            }
            let value = object.next_value_seed(self)?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Value> {
    let duplicate_key = Cell::new(false);
    let visitor = UniqueVisitor {
        duplicate_key: &duplicate_key,
    };
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let parsed = visitor.deserialize(&mut parser).and_then(|value| {
        parser.end()?;
        Ok(value)
    });
    parsed.map_err(|error| {
        // Serde returns a data error for duplicate keys. Keep the specific
        // cause from the visitor without relying on its error text.
        if duplicate_key.get() {
            ChangesBodyRejection::DuplicateKeys.into_shadow_error()
        } else if error.is_syntax() || error.is_eof() {
            ChangesBodyRejection::Malformed.into_shadow_error()
        } else {
            ShadowPageError::from(anyhow::anyhow!(
                "Invalid or ambiguous provider changes JSON"
            ))
            .into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::ChangesBodyRejection::{self, DuplicateKeys, Malformed};
    use crate::icloud::photos::error::ReplayRefusal;

    #[test]
    fn unique_json_keeps_values_but_rejects_duplicate_scope_and_source_keys() {
        let control = br#"{"null":null,"array":[true,false,-2,3,0.1,"hello"]}"#;
        assert_eq!(
            super::parse(control).unwrap(),
            serde_json::from_slice::<serde_json::Value>(control).unwrap()
        );
        for (body, expected) in [
            (
                br#"{"zoneID":{"zoneName":"wrong","zoneName":"right"}}"#.as_slice(),
                DuplicateKeys,
            ),
            (
                br#"{"recordName":"first","recordName":"second"}"#,
                DuplicateKeys,
            ),
            (
                br#"{"fields":{"secret-key":1,"secret-key":2}}"#,
                DuplicateKeys,
            ),
            (br#"[{"zone\u0049D":1,"zoneID":2}]"#, DuplicateKeys),
            (br#"{} {}"#, Malformed),
            (br#"{"zones":[{"private-truncated-response""#, Malformed),
            (br#"{"zones":[?]}"#, Malformed),
        ] {
            let error = super::parse(body).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Invalid or ambiguous provider changes JSON"
            );
            assert_eq!(
                ChangesBodyRejection::from_shadow_error(&error),
                Some(expected)
            );
            assert!(ReplayRefusal::from_shadow_error(&error).is_none());
        }
    }
}
