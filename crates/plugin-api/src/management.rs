//! Optional BPP 1.6 resource management, independent of business commands.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::StateOp;

pub const MAX_MANAGEMENT_DESCRIPTOR_BYTES: usize = 64 * 1024;
pub const MAX_MANAGEMENT_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementDescriptor {
    pub schema_version: u32,
    pub resources: Vec<ManagementResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementResource {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub schema: Value,
    pub columns: Vec<ManagementColumn>,
    pub operations: Vec<ManagementOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementColumn {
    pub key: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementOperation {
    List,
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementRequest {
    pub resource: String,
    pub operation: ManagementOperation,
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub record_id: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
}

const fn default_limit() -> u32 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementResponse {
    pub data: Value,
    pub state_ops: Vec<StateOp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementErrorCode {
    InvalidArgument,
    NotFound,
    Conflict,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct ManagementError {
    pub code: ManagementErrorCode,
    pub message: String,
}

impl ManagementError {
    pub fn new(code: ManagementErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn invalid(message: &str) -> ManagementError {
    ManagementError::new(ManagementErrorCode::InvalidArgument, message)
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn bounded_json(value: &impl Serialize, limit: usize) -> Result<(), ManagementError> {
    // The writer stops at the budget without allocating an unbounded serialized copy.
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("size limit"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value)
        .map_err(|_| invalid("management size limit exceeded"))
}

impl ManagementDescriptor {
    /// Checks descriptor bounds and rejects all non-local schema references.
    /// Hosts must also compile each resource schema using their JSON Schema validator.
    pub fn validate(&self) -> Result<(), ManagementError> {
        bounded_json(self, MAX_MANAGEMENT_DESCRIPTOR_BYTES)?;
        if self.schema_version != 1 || self.resources.len() > 16 {
            return Err(invalid(
                "invalid management descriptor version or resource count",
            ));
        }
        let mut ids = BTreeSet::new();
        for resource in &self.resources {
            if !identifier(&resource.id)
                || !ids.insert(&resource.id)
                || resource.title.len() > 256
                || resource.description.len() > 256
                || resource.columns.len() > 16
                || resource.operations.is_empty()
                || resource.operations.len() > 4
            {
                return Err(invalid("invalid management resource"));
            }
            let mut columns = BTreeSet::new();
            for column in &resource.columns {
                if !identifier(&column.key)
                    || !columns.insert(&column.key)
                    || column.title.len() > 256
                {
                    return Err(invalid("invalid management column"));
                }
            }
            if resource.operations.iter().collect::<BTreeSet<_>>().len()
                != resource.operations.len()
            {
                return Err(invalid("duplicate management operation"));
            }
            if !resource.schema.is_object() && !resource.schema.is_boolean() {
                return Err(invalid("resource schema must be an object or boolean"));
            }
            validate_references(&resource.schema)?;
        }
        Ok(())
    }
}

fn decode_fragment(fragment: &str) -> Result<String, ManagementError> {
    let mut decoded = Vec::new();
    let mut bytes = fragment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let low = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(invalid("invalid reference fragment"));
            };
            decoded.push(
                u8::try_from(high * 16 + low).map_err(|_| invalid("invalid reference fragment"))?,
            );
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid("invalid reference fragment"))
}

fn reference_target<'a>(root: &'a Value, fragment: &str) -> Option<(&'a Value, &'a Value)> {
    let mut path = String::new();
    let mut scope = root;
    let mut target = root;
    for segment in fragment.split('/').skip(1) {
        path.push('/');
        path.push_str(segment);
        target = root.pointer(&path)?;
        if target.get("$id").is_some_and(Value::is_string) {
            scope = target;
        }
    }
    Some((target, scope))
}

fn validate_references(schema: &Value) -> Result<(), ManagementError> {
    let mut pending = vec![(schema, schema)];
    let mut visited = BTreeSet::new();
    while let Some((value, mut scope)) = pending.pop() {
        if value.get("$id").is_some_and(Value::is_string) {
            scope = value;
        }
        if !visited.insert((std::ptr::from_ref(value), std::ptr::from_ref(scope))) {
            continue;
        }
        match value {
            Value::Object(object) => {
                for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
                    if let Some(reference) = object.get(keyword) {
                        let fragment = reference
                            .as_str()
                            .and_then(|s| s.strip_prefix('#'))
                            .ok_or_else(|| invalid("external or invalid schema reference"))?;
                        let fragment = decode_fragment(fragment)?;
                        if fragment.is_empty() || fragment.starts_with('/') {
                            if let Some((target, target_scope)) = reference_target(scope, &fragment)
                            {
                                pending.push((target, target_scope));
                            }
                        }
                        // Named anchors occur at schema-bearing locations visited below.
                        // Literal data never defines schema anchors.
                    }
                }
                for keyword in [
                    "$defs",
                    "definitions",
                    "properties",
                    "patternProperties",
                    "dependentSchemas",
                    "dependencies",
                ] {
                    if let Some(Value::Object(map)) = object.get(keyword) {
                        pending.extend(
                            map.values()
                                .filter(|v| v.is_object() || v.is_boolean())
                                .map(|v| (v, scope)),
                        );
                    }
                }
                for keyword in [
                    "additionalProperties",
                    "unevaluatedProperties",
                    "propertyNames",
                    "contains",
                    "additionalItems",
                    "unevaluatedItems",
                    "not",
                    "if",
                    "then",
                    "else",
                    "contentSchema",
                    "items",
                ] {
                    if let Some(schema) = object.get(keyword) {
                        pending.push((schema, scope));
                    }
                }
                for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
                    if let Some(Value::Array(schemas)) = object.get(keyword) {
                        pending.extend(schemas.iter().map(|v| (v, scope)));
                    }
                }
            }
            Value::Array(values) => pending.extend(values.iter().map(|v| (v, scope))),
            _ => {}
        }
    }
    Ok(())
}

impl ManagementRequest {
    pub fn validate(&self) -> Result<(), ManagementError> {
        if !identifier(&self.resource)
            || !(1..=100).contains(&self.limit)
            || self.search.len() > 256
            || self.cursor.as_ref().is_some_and(|s| s.len() > 512)
            || self
                .record_id
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256)
        {
            return Err(invalid("invalid management request bounds"));
        }
        if self.input.as_ref().is_some_and(|v| !v.is_object()) {
            return Err(invalid("management input must be an object"));
        }
        bounded_json(self, MAX_MANAGEMENT_RESPONSE_BYTES)
    }
}

impl ManagementResponse {
    pub fn validate(&self) -> Result<(), ManagementError> {
        bounded_json(self, MAX_MANAGEMENT_RESPONSE_BYTES)?;
        if self.state_ops.len() > 256 {
            return Err(invalid("too many management state operations"));
        }
        let records = self
            .data
            .get("records")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("management response requires records"))?;
        let cursor = self
            .data
            .get("next_cursor")
            .ok_or_else(|| invalid("missing next_cursor"))?;
        if !cursor.is_null() && cursor.as_str().is_none_or(|s| s.len() > 512) {
            return Err(invalid("invalid management response cursor"));
        }
        let mut ids = BTreeSet::new();
        for record in records {
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("invalid management record id"))?;
            if id.is_empty()
                || id.len() > 256
                || !ids.insert(id)
                || !record.get("values").is_some_and(Value::is_object)
            {
                return Err(invalid("invalid management record"));
            }
        }
        Ok(())
    }

    pub fn validate_for(&self, request: &ManagementRequest) -> Result<(), ManagementError> {
        self.validate()?;
        if request.operation == ManagementOperation::List && !self.state_ops.is_empty() {
            return Err(invalid("list cannot change state"));
        }
        Ok(())
    }
}
