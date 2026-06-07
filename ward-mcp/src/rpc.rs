// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Minimal JSON-RPC 2.0 types tailored for MCP's wire shape.
//!
//! We don't pull in a JSON-RPC crate because the surface is tiny and MCP
//! adds a few conventions (notifications, content-bearing errors) that
//! warrant our own representation anyway.

use serde::{Deserialize, Serialize};

/// A JSON-RPC request. `id` is `None` for notifications, per spec §4.1.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[allow(dead_code)] // present in JSON-RPC frames; we don't introspect
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    pub id: serde_json::Value,
}

impl Response {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id: id.unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn error(id: Option<serde_json::Value>, err: Error) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(err),
            id: id.unwrap_or(serde_json::Value::Null),
        }
    }
}

/// JSON-RPC 2.0 error object. Codes per the spec:
/// -32700 parse error, -32600 invalid request, -32601 method not found,
/// -32602 invalid params, -32603 internal error.
#[derive(Debug, Serialize)]
pub struct Error {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Error {
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self {
            code: -32700,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }
}
