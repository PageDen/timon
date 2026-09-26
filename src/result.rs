//! Typed result of one attempt.
//!
//! A worker or lead writes its final message to a file (Codex `-o`), optionally
//! constrained by a JSON Schema (`--output-schema`). Timon reads that file after
//! the attempt and reports what it found.
//!
//! The point of the module is to keep three outcomes apart that a process exit
//! code cannot distinguish:
//!
//! - the process succeeded and produced a result that satisfies its contract;
//! - the process succeeded but produced no result, or one that does not satisfy
//!   the contract — the harness can ignore `--json` and `--output-schema` when
//!   tools or MCP servers are active (openai/codex#15451);
//! - the process itself failed.
//!
//! Timon does not construct the harness flags here. The caller passes them to
//! the child and tells Timon which paths to read, so the two stay in step
//! without Timon guessing a flag spelling before PR3 qualifies one.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Largest result file Timon will read.
pub const MAX_RESULT_BYTES: u64 = 1024 * 1024;
/// Largest schema file Timon will read.
pub const MAX_SCHEMA_BYTES: u64 = 256 * 1024;

/// What Timon found where the attempt's result was meant to be.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResultStatus {
    /// No result file was requested for this attempt.
    NotRequested,
    /// The attempt was meant to write a result and did not.
    Missing { path: PathBuf },
    /// The file exists but could not be read, or is larger than the limit.
    Unreadable { path: PathBuf, reason: String },
    /// The file is empty.
    Empty { path: PathBuf },
    /// A schema was given, but the result is not JSON.
    NotJson { path: PathBuf, reason: String },
    /// The result is JSON but does not satisfy the schema.
    SchemaViolation {
        path: PathBuf,
        pointer: String,
        reason: String,
    },
    /// The schema itself could not be applied, so the result is *not* treated
    /// as validated. Timon implements a documented subset of JSON Schema and
    /// fails closed on anything outside it rather than passing a result it did
    /// not really check.
    SchemaNotApplied {
        path: PathBuf,
        pointer: String,
        reason: String,
    },
    /// The result was read, and validated when a schema was given.
    Parsed {
        path: PathBuf,
        bytes: u64,
        /// True only when a schema was given and the result satisfied it.
        schema_validated: bool,
    },
}

impl ResultStatus {
    /// True when a result was requested and is usable.
    pub fn is_usable(&self) -> bool {
        matches!(self, ResultStatus::Parsed { .. })
    }

    /// True when a result was requested but is unusable. A [`ResultStatus::NotRequested`]
    /// attempt is not a failure.
    pub fn is_failure(&self) -> bool {
        !matches!(
            self,
            ResultStatus::NotRequested | ResultStatus::Parsed { .. }
        )
    }
}

/// Reads the attempt's result file and validates it when a schema is given.
///
/// `result_path` is the file the caller told the child to write. `schema_path`
/// is the schema the caller gave the child. When `schema_path` is `None` the
/// result is treated as free-form text and only its presence is checked.
pub fn load(result_path: Option<&Path>, schema_path: Option<&Path>) -> ResultStatus {
    let Some(path) = result_path else {
        return ResultStatus::NotRequested;
    };
    let owned = path.to_path_buf();

    let bytes = match read_bounded(path, MAX_RESULT_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ResultStatus::Missing { path: owned };
        }
        Err(error) => {
            return ResultStatus::Unreadable {
                path: owned,
                reason: error.to_string(),
            };
        }
    };
    if bytes.trim().is_empty() {
        return ResultStatus::Empty { path: owned };
    }
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

    let Some(schema_path) = schema_path else {
        return ResultStatus::Parsed {
            path: owned,
            bytes: size,
            schema_validated: false,
        };
    };

    let value = match serde_json::from_str::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return ResultStatus::NotJson {
                path: owned,
                reason: error.to_string(),
            };
        }
    };

    let schema = match read_bounded(schema_path, MAX_SCHEMA_BYTES) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(schema) => schema,
            Err(error) => {
                return ResultStatus::SchemaNotApplied {
                    path: owned,
                    pointer: String::new(),
                    reason: format!("schema {} is not JSON: {error}", schema_path.display()),
                };
            }
        },
        Err(error) => {
            return ResultStatus::SchemaNotApplied {
                path: owned,
                pointer: String::new(),
                reason: format!("schema {} is unreadable: {error}", schema_path.display()),
            };
        }
    };

    match validate(&value, &schema, "") {
        Ok(()) => ResultStatus::Parsed {
            path: owned,
            bytes: size,
            schema_validated: true,
        },
        Err(SchemaError::Violation { pointer, reason }) => ResultStatus::SchemaViolation {
            path: owned,
            pointer,
            reason,
        },
        Err(SchemaError::Unsupported { pointer, reason }) => ResultStatus::SchemaNotApplied {
            path: owned,
            pointer,
            reason,
        },
    }
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > limit {
        return Err(io::Error::other(format!(
            "file is {} bytes; the limit is {limit}",
            metadata.len()
        )));
    }
    fs::read_to_string(path)
}

/// Why a result was not validated.
#[derive(Debug)]
enum SchemaError {
    /// The result contradicts the schema.
    Violation { pointer: String, reason: String },
    /// The schema uses something outside the supported subset.
    Unsupported { pointer: String, reason: String },
}

/// JSON Schema keywords this validator applies.
const SUPPORTED: &[&str] = &[
    "type",
    "required",
    "properties",
    "items",
    "enum",
    "additionalProperties",
];
/// Annotation keywords that carry no constraint and are skipped.
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
];

/// Validates `value` against the supported subset of JSON Schema.
///
/// Any keyword outside [`SUPPORTED`] and [`ANNOTATIONS`] stops validation with
/// [`SchemaError::Unsupported`]. Failing closed matters here: silently skipping
/// a keyword would report a result as schema-validated when the constraint that
/// mattered was never applied.
fn validate(value: &Value, schema: &Value, pointer: &str) -> Result<(), SchemaError> {
    let Some(schema) = schema.as_object() else {
        // `true`/`false` schemas are valid JSON Schema but not implemented here.
        return Err(SchemaError::Unsupported {
            pointer: pointer.to_owned(),
            reason: "schema is not an object".to_owned(),
        });
    };

    for keyword in schema.keys() {
        if !SUPPORTED.contains(&keyword.as_str()) && !ANNOTATIONS.contains(&keyword.as_str()) {
            return Err(SchemaError::Unsupported {
                pointer: pointer.to_owned(),
                reason: format!("schema keyword {keyword} is not supported"),
            });
        }
    }

    if let Some(expected) = schema.get("type") {
        check_type(value, expected, pointer)?;
    }

    if let Some(allowed) = schema.get("enum") {
        let Some(allowed) = allowed.as_array() else {
            return Err(SchemaError::Unsupported {
                pointer: pointer.to_owned(),
                reason: "enum is not an array".to_owned(),
            });
        };
        if !allowed.contains(value) {
            return Err(SchemaError::Violation {
                pointer: pointer.to_owned(),
                reason: "value is not one of the enumerated values".to_owned(),
            });
        }
    }

    if let Some(required) = schema.get("required") {
        let Some(required) = required.as_array() else {
            return Err(SchemaError::Unsupported {
                pointer: pointer.to_owned(),
                reason: "required is not an array".to_owned(),
            });
        };
        if let Some(object) = value.as_object() {
            for name in required {
                let Some(name) = name.as_str() else {
                    return Err(SchemaError::Unsupported {
                        pointer: pointer.to_owned(),
                        reason: "required contains a non-string entry".to_owned(),
                    });
                };
                if !object.contains_key(name) {
                    return Err(SchemaError::Violation {
                        pointer: pointer.to_owned(),
                        reason: format!("required property {name} is missing"),
                    });
                }
            }
        }
    }

    if let Some(properties) = schema.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err(SchemaError::Unsupported {
                pointer: pointer.to_owned(),
                reason: "properties is not an object".to_owned(),
            });
        };
        if let Some(object) = value.as_object() {
            for (name, subschema) in properties {
                if let Some(child) = object.get(name) {
                    validate(child, subschema, &child_pointer(pointer, name))?;
                }
            }
            if let Some(additional) = schema.get("additionalProperties") {
                check_additional(object, properties, additional, pointer)?;
            }
        }
    } else if let Some(additional) = schema.get("additionalProperties")
        && let Some(object) = value.as_object()
    {
        check_additional(object, &serde_json::Map::new(), additional, pointer)?;
    }

    if let Some(items) = schema.get("items") {
        if items.is_array() {
            return Err(SchemaError::Unsupported {
                pointer: pointer.to_owned(),
                reason: "tuple form of items is not supported".to_owned(),
            });
        }
        if let Some(array) = value.as_array() {
            for (index, element) in array.iter().enumerate() {
                validate(element, items, &child_pointer(pointer, &index.to_string()))?;
            }
        }
    }

    Ok(())
}

fn check_additional(
    object: &serde_json::Map<String, Value>,
    properties: &serde_json::Map<String, Value>,
    additional: &Value,
    pointer: &str,
) -> Result<(), SchemaError> {
    match additional {
        Value::Bool(true) => Ok(()),
        Value::Bool(false) => {
            for name in object.keys() {
                if !properties.contains_key(name) {
                    return Err(SchemaError::Violation {
                        pointer: pointer.to_owned(),
                        reason: format!("property {name} is not allowed"),
                    });
                }
            }
            Ok(())
        }
        Value::Object(_) => {
            for (name, child) in object {
                if !properties.contains_key(name) {
                    validate(child, additional, &child_pointer(pointer, name))?;
                }
            }
            Ok(())
        }
        _ => Err(SchemaError::Unsupported {
            pointer: pointer.to_owned(),
            reason: "additionalProperties is neither a boolean nor a schema".to_owned(),
        }),
    }
}

fn check_type(value: &Value, expected: &Value, pointer: &str) -> Result<(), SchemaError> {
    let names = match expected {
        Value::String(name) => vec![name.as_str()],
        Value::Array(entries) => {
            let mut names = Vec::with_capacity(entries.len());
            for entry in entries {
                let Some(name) = entry.as_str() else {
                    return Err(SchemaError::Unsupported {
                        pointer: pointer.to_owned(),
                        reason: "type contains a non-string entry".to_owned(),
                    });
                };
                names.push(name);
            }
            names
        }
        _ => {
            return Err(SchemaError::Unsupported {
                pointer: pointer.to_owned(),
                reason: "type is neither a string nor an array".to_owned(),
            });
        }
    };

    for name in &names {
        let matched = match *name {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            other => {
                return Err(SchemaError::Unsupported {
                    pointer: pointer.to_owned(),
                    reason: format!("type {other} is not supported"),
                });
            }
        };
        if matched {
            return Ok(());
        }
    }

    Err(SchemaError::Violation {
        pointer: pointer.to_owned(),
        reason: format!("value does not have type {}", names.join(" or ")),
    })
}

/// Builds a JSON Pointer for a child, escaping per RFC 6901.
fn child_pointer(parent: &str, token: &str) -> String {
    format!("{parent}/{}", token.replace('~', "~0").replace('/', "~1"))
}
