//! JSON-RPC 2.0 envelopes.
//!
//! The wire is one JSON object per line (NDJSON) and the version field is a
//! fixed `"2.0"` marker, not a free string. `Response` has no invalid shape:
//! it is either a result or an error, and the decoder rejects a line with
//! both (or neither) before the transport ever sees it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Number, Value};
use thiserror::Error;

/// JSON-RPC parse error.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC invalid request.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC method not found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC invalid params.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC internal error.
pub const INTERNAL_ERROR: i64 = -32603;
/// Application error: `base_revision` did not match the live document.
pub const REVISION_CONFLICT: i64 = -32001;

/// The only protocol version this build speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Version {
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

/// A request id: a JSON number or string. The client assigns increasing
/// numbers; echoing a client's string is what lets other front-ends share
/// one connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(Number),
    String(String),
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Id::Number(Number::from(value))
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Number(n) => write!(f, "{n}"),
            Id::String(s) => write!(f, "{s:?}"),
        }
    }
}

/// One JSON-RPC request. `params` is a `Value` at this layer: the typed
/// [`crate::Call`] decoder turns it into a catalog call, and that is where
/// an unknown method or bad field becomes a typed [`RpcError`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: Version,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    pub fn new(id: Id, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: Version::V2,
            id,
            method: method.into(),
            params,
        }
    }
}

/// One JSON-RPC notification (an event with no id and no response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: Version::V2,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Error)]
#[error("{message} (code {code})")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The typed `data` a revision conflict carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionConflict {
    pub expected: u64,
    pub actual: u64,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error(detail: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, format!("parse error: {}", detail.into()))
    }

    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new(
            INVALID_REQUEST,
            format!("invalid request: {}", detail.into()),
        )
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, format!("invalid params: {}", detail.into()))
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, format!("internal error: {}", detail.into()))
    }

    /// Compare-and-swap failure: the client built its edit on `expected`,
    /// but the document is at `actual`.
    pub fn revision_conflict(expected: u64, actual: u64) -> Self {
        Self {
            code: REVISION_CONFLICT,
            message: "revision conflict".to_string(),
            data: Some(serde_json::json!({
                "expected": expected,
                "actual": actual,
            })),
        }
    }

    /// Decode the typed data of a [`REVISION_CONFLICT`], if this error is
    /// one. Anything else (a standard error, malformed data) is `None`.
    pub fn revision_conflict_data(&self) -> Option<RevisionConflict> {
        if self.code != REVISION_CONFLICT {
            return None;
        }
        serde_json::from_value(self.data.clone()?).ok()
    }
}

/// A JSON-RPC response. A result always has an id; an error may have a null
/// id (a line that could not even be parsed as a request).
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Result { id: Id, result: Value },
    Error { id: Option<Id>, error: RpcError },
}

impl Response {
    pub fn id(&self) -> Option<&Id> {
        match self {
            Response::Result { id, .. } => Some(id),
            Response::Error { id, .. } => id.as_ref(),
        }
    }

    pub fn into_result(self) -> Result<Value, RpcError> {
        match self {
            Response::Result { result, .. } => Ok(result),
            Response::Error { error, .. } => Err(error),
        }
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", &Version::V2)?;
        match self {
            Response::Result { id, result } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("result", result)?;
            }
            Response::Error { id, error } => {
                // `None` must be written as `"id": null`, not omitted: that
                // is how a parse-error response is identified.
                map.serialize_entry("id", id)?;
                map.serialize_entry("error", error)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let Value::Object(mut object) = value else {
            return Err(de::Error::custom("a response must be a JSON object"));
        };
        if object.remove("jsonrpc") != Some(serde_json::json!("2.0")) {
            return Err(de::Error::custom("response is not JSON-RPC 2.0"));
        }
        let id = match object.remove("id") {
            None | Some(Value::Null) => None,
            Some(value) => Some(serde_json::from_value::<Id>(value).map_err(de::Error::custom)?),
        };
        let result = object.remove("result");
        let error = object.remove("error");
        match (result, error) {
            (Some(result), None) => {
                let id = id.ok_or_else(|| de::Error::custom("a result response needs an id"))?;
                Ok(Response::Result { id, result })
            }
            (None, Some(error)) => Ok(Response::Error {
                id,
                error: serde_json::from_value(error).map_err(de::Error::custom)?,
            }),
            _ => Err(de::Error::custom(
                "a response needs exactly one of result or error",
            )),
        }
    }
}
