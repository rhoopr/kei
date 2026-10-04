//! Reject ambiguous identities while retaining the original response elsewhere.

use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

struct Unique(Value);

impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor).map(Self)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
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
        while let Some(Unique(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("Duplicate JSON object key"));
            }
            let Unique(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Value> {
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let parsed = Unique::deserialize(&mut parser).and_then(|value| {
        parser.end()?;
        Ok(value.0)
    });
    parsed.map_err(|_error| {
        super::error::ShadowPageError::from(anyhow::anyhow!(
            "Invalid or ambiguous provider changes JSON"
        ))
        .into()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn unique_json_keeps_values_but_rejects_duplicate_scope_and_source_keys() {
        let control = br#"{"null":null,"array":[true,false,-2,3,0.1,"hello"]}"#;
        assert_eq!(
            super::parse(control).unwrap(),
            serde_json::from_slice::<serde_json::Value>(control).unwrap()
        );
        for body in [
            br#"{"zoneID":{"zoneName":"wrong","zoneName":"right"}}"#.as_slice(),
            br#"{"recordName":"first","recordName":"second"}"#,
            br#"{"fields":{"secret-key":1,"secret-key":2}}"#,
            br#"{} {}"#,
        ] {
            assert_eq!(
                super::parse(body).unwrap_err().to_string(),
                "Invalid or ambiguous provider changes JSON"
            );
        }
    }
}
