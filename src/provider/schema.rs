//! Host-side validation and projection for UtterPipe utterance-option schemas.

use std::collections::HashSet;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const MAX_SCHEMA_BYTES: usize = 65_536;
const MAX_SCHEMA_DEPTH: usize = 6;
const MAX_TOP_LEVEL_PROPERTIES: usize = 64;

pub(super) fn validate_schema_and_digest(schema: &Value, expected: &str) -> Result<(), String> {
    validate_schema(schema)?;
    let canonical = serde_json_canonicalizer::to_vec(schema)
        .map_err(|error| format!("utterance schema cannot be canonicalized: {error}"))?;
    let actual = format!("sha256:{:x}", Sha256::digest(canonical));
    if actual != expected {
        return Err(format!(
            "utterance schema digest mismatch: expected {expected}, calculated {actual}"
        ));
    }
    Ok(())
}

pub(super) fn project_allowed_properties(
    schema: &Value,
    allowed: &[String],
) -> Result<Map<String, Value>, String> {
    validate_schema(schema)?;
    let properties = schema["properties"]
        .as_object()
        .expect("validated schema has properties");
    let mut projected = Map::new();
    for name in allowed {
        let property = properties.get(name).ok_or_else(|| {
            format!("agent_utterance_options names provider option '{name}', which is absent")
        })?;
        projected.insert(name.clone(), property.clone());
    }
    Ok(projected)
}

pub(super) fn projected_object_schema(properties: Map<String, Value>) -> Value {
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "additionalProperties":false,
        "maxProperties":properties.len(),
        "properties":properties
    })
}

pub(super) fn validate_request(
    options: &Map<String, Value>,
    projected_schema: Option<&Value>,
) -> Result<(), String> {
    if options.is_empty() && projected_schema.is_none() {
        return Ok(());
    }
    let schema = projected_schema.ok_or_else(|| {
        "utterance_options are unavailable for the configured TTS provider".to_owned()
    })?;
    validate_schema(schema)?;
    if options.len() > MAX_TOP_LEVEL_PROPERTIES
        || serde_json::to_vec(options).map_or(true, |encoded| encoded.len() > MAX_SCHEMA_BYTES)
        || options
            .values()
            .any(|value| value_depth(value, 1) > MAX_SCHEMA_DEPTH)
    {
        return Err("utterance_options exceeds structural limits".into());
    }
    let properties = schema["properties"]
        .as_object()
        .expect("validated projected schema has properties");
    let mut invalid = Vec::new();
    for (name, value) in options {
        let path = format!("/{}", pointer(name));
        match properties.get(name) {
            Some(property) => {
                value_matches(property, value, &path, 1, &mut invalid);
            }
            None => invalid.push(path),
        }
        if invalid.len() >= 16 {
            break;
        }
    }
    if !invalid.is_empty() {
        return Err(format!(
            "utterance_options do not satisfy the startup-authorized schema at {}",
            invalid.join(", ")
        ));
    }
    Ok(())
}

fn validate_schema(schema: &Value) -> Result<(), String> {
    if serde_json::to_vec(schema).map_or(true, |encoded| encoded.len() > MAX_SCHEMA_BYTES) {
        return Err("utterance schema exceeds 65,536 bytes".into());
    }
    let root = schema
        .as_object()
        .ok_or_else(|| "utterance schema root must be an object".to_owned())?;
    allow_only(
        root,
        &[
            "$schema",
            "type",
            "additionalProperties",
            "maxProperties",
            "properties",
        ],
    )?;
    if root.get("$schema").and_then(Value::as_str)
        != Some("https://json-schema.org/draft/2020-12/schema")
        || root.get("type").and_then(Value::as_str) != Some("object")
        || root.get("additionalProperties").and_then(Value::as_bool) != Some(false)
    {
        return Err("utterance schema has an invalid closed object root".into());
    }
    let maximum = bounded_integer(root.get("maxProperties"), 64, "maxProperties")?;
    let properties = root
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "utterance schema properties must be an object".to_owned())?;
    if properties.len() > maximum || properties.len() > MAX_TOP_LEVEL_PROPERTIES {
        return Err("utterance schema contains too many properties".into());
    }
    for (name, property) in properties {
        if !valid_name(name) {
            return Err(format!(
                "utterance schema contains invalid option name '{name}'"
            ));
        }
        validate_property(property, 1, true)?;
    }
    Ok(())
}

fn validate_property(schema: &Value, depth: usize, top_level: bool) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err("utterance schema nesting exceeds six".into());
    }
    let object = schema
        .as_object()
        .ok_or_else(|| "utterance property schema must be an object".to_owned())?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "utterance property must name exactly one type".to_owned())?;
    let mut allowed = vec![
        "type",
        "enum",
        "default",
        "examples",
        "title",
        "description",
    ];
    match kind {
        "boolean" => {}
        "integer" | "number" => allowed.extend([
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ]),
        "string" => allowed.extend(["minLength", "maxLength"]),
        "array" => allowed.extend(["items", "minItems", "maxItems", "uniqueItems"]),
        "object" => allowed.extend([
            "properties",
            "required",
            "minProperties",
            "maxProperties",
            "additionalProperties",
        ]),
        _ => return Err(format!("utterance property type '{kind}' is unsupported")),
    }
    if top_level {
        allowed.push("x-utterpipe");
    }
    allow_only(object, &allowed)?;
    if top_level {
        annotation(object.get("title"), 80, "title")?;
        annotation(object.get("description"), 512, "description")?;
        validate_extension(object.get("x-utterpipe"))?;
    } else {
        if object.contains_key("title") {
            annotation(object.get("title"), 80, "title")?;
        }
        if object.contains_key("description") {
            annotation(object.get("description"), 512, "description")?;
        }
    }

    match kind {
        "integer" | "number" => validate_numeric_schema(object)?,
        "string" => {
            let maximum = bounded_integer(object.get("maxLength"), 4_096, "maxLength")?;
            if optional_integer(object.get("minLength"), "minLength")?.is_some_and(|v| v > maximum)
            {
                return Err("string minLength exceeds maxLength".into());
            }
        }
        "array" => {
            let maximum = bounded_integer(object.get("maxItems"), 64, "maxItems")?;
            if optional_integer(object.get("minItems"), "minItems")?.is_some_and(|v| v > maximum) {
                return Err("array minItems exceeds maxItems".into());
            }
            if object
                .get("uniqueItems")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err("uniqueItems must be boolean".into());
            }
            validate_property(
                object
                    .get("items")
                    .ok_or_else(|| "array schema requires items".to_owned())?,
                depth + 1,
                false,
            )?;
        }
        "object" => validate_object_schema(object, depth)?,
        _ => {}
    }

    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .ok_or_else(|| "enum must be an array".to_owned())?;
        if values.is_empty()
            || values.len() > 256
            || values
                .iter()
                .enumerate()
                .any(|(index, value)| values[..index].contains(value))
        {
            return Err("enum must contain 1 to 256 unique values".into());
        }
        for value in values {
            validate_annotation_value(object, value)?;
        }
    }
    if let Some(default) = object.get("default") {
        validate_annotation_value(object, default)?;
    }
    if let Some(examples) = object.get("examples") {
        let examples = examples
            .as_array()
            .ok_or_else(|| "examples must be an array".to_owned())?;
        if examples.len() > 4 {
            return Err("examples must contain at most four values".into());
        }
        for example in examples {
            validate_annotation_value(object, example)?;
        }
    }
    Ok(())
}

fn validate_numeric_schema(object: &Map<String, Value>) -> Result<(), String> {
    let bounds = |inclusive: &str, exclusive: &str, lower: bool| {
        let mut values = [(inclusive, false), (exclusive, true)]
            .into_iter()
            .filter_map(|(name, exclusive)| {
                object
                    .get(name)
                    .and_then(Value::as_f64)
                    .filter(|number| number.is_finite())
                    .map(|number| (number, exclusive))
            });
        let first = values.next()?;
        Some(values.fold(first, |current, candidate| {
            let candidate_wins = if lower {
                candidate.0 > current.0
            } else {
                candidate.0 < current.0
            };
            if candidate_wins {
                candidate
            } else if candidate.0 == current.0 {
                (current.0, current.1 || candidate.1)
            } else {
                current
            }
        }))
    };
    if ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"]
        .into_iter()
        .filter_map(|name| object.get(name))
        .any(|value| value.as_f64().is_none_or(|number| !number.is_finite()))
    {
        return Err("numeric schema has invalid bounds or multipleOf".into());
    }
    let lower = bounds("minimum", "exclusiveMinimum", true)
        .ok_or_else(|| "numeric schema requires a finite lower bound".to_owned())?;
    let upper = bounds("maximum", "exclusiveMaximum", false)
        .ok_or_else(|| "numeric schema requires a finite upper bound".to_owned())?;
    if lower.0 > upper.0
        || lower.0 == upper.0 && (lower.1 || upper.1)
        || object.get("multipleOf").is_some_and(|value| {
            value
                .as_f64()
                .is_none_or(|number| !number.is_finite() || number <= 0.0)
        })
    {
        return Err("numeric schema has invalid bounds or multipleOf".into());
    }
    Ok(())
}

fn validate_object_schema(object: &Map<String, Value>, depth: usize) -> Result<(), String> {
    if object.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        return Err("nested object schema must reject additional properties".into());
    }
    let maximum = bounded_integer(object.get("maxProperties"), 32, "maxProperties")?;
    if optional_integer(object.get("minProperties"), "minProperties")?.is_some_and(|v| v > maximum)
    {
        return Err("object minProperties exceeds maxProperties".into());
    }
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "nested object schema requires properties".to_owned())?;
    if properties.len() > maximum {
        return Err("nested object schema contains too many properties".into());
    }
    for (name, property) in properties {
        if !valid_name(name) {
            return Err(format!(
                "nested object contains invalid property name '{name}'"
            ));
        }
        validate_property(property, depth + 1, false)?;
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| "required must be an array".to_owned())?;
        let mut seen = HashSet::new();
        if required.iter().any(|name| {
            name.as_str()
                .is_none_or(|name| !properties.contains_key(name) || !seen.insert(name))
        }) {
            return Err("required must contain unique declared property names".into());
        }
    }
    Ok(())
}

fn validate_extension(value: Option<&Value>) -> Result<(), String> {
    let extension = value
        .and_then(Value::as_object)
        .ok_or_else(|| "top-level option requires x-utterpipe annotations".to_owned())?;
    allow_only(
        extension,
        &[
            "default_behavior",
            "use_when",
            "omit_when",
            "unit",
            "effects",
        ],
    )?;
    for name in ["default_behavior", "use_when", "omit_when"] {
        annotation(extension.get(name), 512, name)?;
    }
    if extension.contains_key("unit") {
        annotation(extension.get("unit"), 32, "unit")?;
    }
    if extension.get("effects").is_some_and(|value| {
        value.as_array().is_none_or(|effects| {
            effects.len() > 8
                || effects
                    .iter()
                    .any(|effect| valid_annotation(effect, 256).is_err())
        })
    }) {
        return Err("x-utterpipe effects are invalid".into());
    }
    Ok(())
}

fn annotation(value: Option<&Value>, maximum: usize, name: &str) -> Result<(), String> {
    valid_annotation(
        value.ok_or_else(|| format!("utterance annotation '{name}' is required"))?,
        maximum,
    )
    .map_err(|()| format!("utterance annotation '{name}' is invalid"))
}

fn valid_annotation(value: &Value, maximum: usize) -> Result<(), ()> {
    let text = value.as_str().ok_or(())?;
    if text.chars().count() > maximum
        || text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\t' | '\n' | '\r'))
        || text.contains("http://")
        || text.contains("https://")
    {
        Err(())
    } else {
        Ok(())
    }
}

fn allow_only(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(name) = object.keys().find(|name| !allowed.contains(&name.as_str())) {
        Err(format!("utterance schema keyword '{name}' is not allowed"))
    } else {
        Ok(())
    }
}

fn bounded_integer(value: Option<&Value>, maximum: usize, name: &str) -> Result<usize, String> {
    let value = optional_integer(value, name)?
        .ok_or_else(|| format!("utterance schema requires {name}"))?;
    if value > maximum {
        Err(format!("utterance schema {name} exceeds {maximum}"))
    } else {
        Ok(value)
    }
}

fn optional_integer(value: Option<&Value>, name: &str) -> Result<Option<usize>, String> {
    value
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or_else(|| format!("utterance schema {name} must be a nonnegative integer"))
        })
        .transpose()
}

fn valid_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && value.len() <= 64
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_annotation_value(schema: &Map<String, Value>, value: &Value) -> Result<(), String> {
    let mut invalid = Vec::new();
    if value_matches_map(schema, value, "/", 1, &mut invalid) {
        Ok(())
    } else {
        Err("enum, default, or example value does not satisfy its schema".into())
    }
}

fn value_matches(
    schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
    invalid: &mut Vec<String>,
) -> bool {
    value_matches_map(
        schema.as_object().expect("schema was validated"),
        value,
        path,
        depth,
        invalid,
    )
}

fn value_matches_map(
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
    depth: usize,
    invalid: &mut Vec<String>,
) -> bool {
    if depth > MAX_SCHEMA_DEPTH
        || schema
            .get("enum")
            .is_some_and(|values| !values.as_array().is_some_and(|items| items.contains(value)))
    {
        invalid.push(path.into());
        return false;
    }
    let valid = match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("number") => value.is_number(),
        Some("string") => value.as_str().is_some_and(|text| {
            let length = text.chars().count();
            length >= keyword_usize(schema, "minLength", 0)
                && length <= keyword_usize(schema, "maxLength", usize::MAX)
        }),
        Some("array") => value.as_array().is_some_and(|items| {
            if items.len() < keyword_usize(schema, "minItems", 0)
                || items.len() > keyword_usize(schema, "maxItems", usize::MAX)
                || schema.get("uniqueItems").and_then(Value::as_bool) == Some(true)
                    && items
                        .iter()
                        .enumerate()
                        .any(|(index, item)| items[..index].contains(item))
            {
                return false;
            }
            let item_schema = &schema["items"];
            items.iter().enumerate().all(|(index, item)| {
                value_matches(
                    item_schema,
                    item,
                    &format!("{path}/{index}"),
                    depth + 1,
                    invalid,
                )
            })
        }),
        Some("object") => value.as_object().is_some_and(|object| {
            let properties = schema["properties"]
                .as_object()
                .expect("schema was validated");
            if object.len() < keyword_usize(schema, "minProperties", 0)
                || object.len() > keyword_usize(schema, "maxProperties", usize::MAX)
            {
                return false;
            }
            let required_ok =
                schema
                    .get("required")
                    .and_then(Value::as_array)
                    .is_none_or(|required| {
                        required
                            .iter()
                            .all(|name| name.as_str().is_some_and(|name| object.contains_key(name)))
                    });
            required_ok
                && object.iter().all(|(name, member)| {
                    properties.get(name).is_some_and(|member_schema| {
                        value_matches(
                            member_schema,
                            member,
                            &format!("{path}/{}", pointer(name)),
                            depth + 1,
                            invalid,
                        )
                    })
                })
        }),
        _ => false,
    } && numeric_keywords_match(schema, value);
    if !valid && !invalid.iter().any(|item| item == path) {
        invalid.push(path.into());
    }
    valid
}

fn numeric_keywords_match(schema: &Map<String, Value>, value: &Value) -> bool {
    let Some(number) = value.as_f64() else {
        return true;
    };
    schema
        .get("minimum")
        .and_then(Value::as_f64)
        .is_none_or(|bound| number >= bound)
        && schema
            .get("maximum")
            .and_then(Value::as_f64)
            .is_none_or(|bound| number <= bound)
        && schema
            .get("exclusiveMinimum")
            .and_then(Value::as_f64)
            .is_none_or(|bound| number > bound)
        && schema
            .get("exclusiveMaximum")
            .and_then(Value::as_f64)
            .is_none_or(|bound| number < bound)
        && schema
            .get("multipleOf")
            .and_then(Value::as_f64)
            .is_none_or(|multiple| {
                let quotient = number / multiple;
                (quotient - quotient.round()).abs() <= f64::EPSILON * quotient.abs().max(1.0) * 8.0
            })
}

fn keyword_usize(schema: &Map<String, Value>, name: &str, fallback: usize) -> usize {
    schema
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(fallback)
}

fn pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn value_depth(value: &Value, depth: usize) -> usize {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| value_depth(item, depth + 1))
            .max()
            .unwrap_or(depth),
        Value::Object(object) => object
            .values()
            .map(|item| value_depth(item, depth + 1))
            .max()
            .unwrap_or(depth),
        _ => depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object","additionalProperties":false,"maxProperties":64,
            "properties":{
                "tone":{
                    "type":"string","enum":["neutral","bright"],"maxLength":16,
                    "title":"Voice tone","description":"Selects a voice tone.",
                    "x-utterpipe":{
                        "default_behavior":"Omission uses configured tone.",
                        "use_when":"Use when a different tone is wanted.",
                        "omit_when":"Omit when configured tone is suitable."
                    }
                }
            }
        })
    }

    #[test]
    fn validates_digest_projection_and_request_shape() {
        let schema = schema();
        let digest = format!(
            "sha256:{:x}",
            Sha256::digest(serde_json_canonicalizer::to_vec(&schema).unwrap())
        );
        validate_schema_and_digest(&schema, &digest).unwrap();
        let projected = project_allowed_properties(&schema, &["tone".into()]).unwrap();
        assert!(projected_object_schema(projected)["properties"]["tone"].is_object());
        let projected =
            projected_object_schema(project_allowed_properties(&schema, &["tone".into()]).unwrap());
        validate_request(
            &serde_json::from_value(json!({"tone":"bright"})).unwrap(),
            Some(&projected),
        )
        .unwrap();
        assert!(
            validate_request(
                &serde_json::from_value(json!({"tone":"unsupported"})).unwrap(),
                Some(&projected),
            )
            .is_err()
        );
        assert!(
            validate_request(
                &serde_json::from_value(json!({"voice":"secret"})).unwrap(),
                Some(&projected),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unsatisfiable_bounds_and_invalid_annotations() {
        let mut invalid_bounds = schema();
        invalid_bounds["properties"]["tone"]["minLength"] = json!(20);
        assert!(validate_schema(&invalid_bounds).is_err());

        let mut invalid_default = schema();
        invalid_default["properties"]["tone"]["default"] = json!("unsupported");
        assert!(validate_schema(&invalid_default).is_err());

        let mut numeric = schema();
        numeric["properties"]["tone"] = json!({
            "type":"number","minimum":1.0,"exclusiveMinimum":2.0,"maximum":2.0,
            "title":"Rate","description":"Speaking rate.",
            "x-utterpipe":{
                "default_behavior":"Omission uses configured rate.",
                "use_when":"Use to change rate.",
                "omit_when":"Omit when configured rate is suitable."
            }
        });
        assert!(validate_schema(&numeric).is_err());
    }
}
