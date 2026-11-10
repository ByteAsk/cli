//! Shared argument deserializers for the tool argument structs.

use serde::Deserialize;

/// A count or a number of seconds, as the schema promises it: any JSON number
/// that is a whole number. `30.0` is 30 — it used to be refused as "invalid
/// type: floating point, expected u64" although the schema says
/// "number". A fraction or a negative number is still refused, with a message
/// that says why.
pub(crate) fn whole_u64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    use serde::de::Error;
    match Option::<f64>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) if value.fract() == 0.0 && (0.0..=u64::MAX as f64).contains(&value) => {
            Ok(Some(value as u64))
        }
        Some(value) => Err(D::Error::custom(format!(
            "expected a whole number of seconds/runs, got {value}"
        ))),
    }
}

/// [`whole_u64`] for a `u32` field.
pub(crate) fn whole_u32<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    use serde::de::Error;
    match whole_u64(deserializer)? {
        None => Ok(None),
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| D::Error::custom(format!("{value} is too large"))),
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Args {
        #[serde(default, deserialize_with = "super::whole_u64")]
        timeout_sec: Option<u64>,
        #[serde(default, deserialize_with = "super::whole_u32")]
        repeat: Option<u32>,
    }

    fn parse(json: &str) -> Result<Args, String> {
        serde_json::from_str::<Args>(json).map_err(|e| e.to_string())
    }

    #[test]
    fn a_whole_valued_float_is_accepted_and_a_fraction_is_explained() {
        assert_eq!(parse(r#"{"timeout_sec":30.0}"#).unwrap().timeout_sec, Some(30));
        assert_eq!(parse(r#"{"timeout_sec":30}"#).unwrap().timeout_sec, Some(30));
        assert_eq!(parse(r#"{"repeat":5.0}"#).unwrap().repeat, Some(5));
        assert_eq!(parse("{}").unwrap().timeout_sec, None);
        let err = parse(r#"{"timeout_sec":2.5}"#).err().unwrap();
        assert!(err.contains("whole number"), "{err}");
        assert!(parse(r#"{"timeout_sec":-1}"#).is_err());
        assert!(parse(r#"{"repeat":5000000000}"#).is_err());
    }
}
