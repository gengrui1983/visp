//! TraceContext — lightweight W3C Trace Context data type.
//!
//! This module defines a minimal `TraceContext` value type that carries
//! W3C Trace Context fields (trace_id, span_id, trace_flags, trace_state)
//! through the agent system. It deliberately does **not** depend on
//! `tracing` or `opentelemetry` crates.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur when constructing or parsing a `TraceContext`.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum TraceContextError {
    #[error("trace_id must be exactly 32 hex characters")]
    InvalidTraceIdLength,

    #[error("span_id must be exactly 16 hex characters")]
    InvalidSpanIdLength,

    #[error("invalid W3C traceparent header format")]
    InvalidTraceparent,

    #[error("value contains non-hex characters")]
    InvalidHex,
}

/// Lightweight W3C Trace Context value type.
///
/// Carries trace_id, span_id, trace_flags, trace_state, and an optional
/// visp-internal parent_span_id through the agent system without depending
/// on `tracing` or `opentelemetry`.
///
/// The `parent_span_id` field is **not** part of the W3C Trace Context
/// spec; it is a visp-internal field used for cross-mpsc parentage tracking
/// across agent/task boundaries.  It is always `None` when parsed from a
/// W3C `traceparent` header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// 32 hex character trace ID (lowercase)
    pub trace_id: String,
    /// 16 hex character span ID (lowercase)
    pub span_id: String,
    /// W3C trace flags (e.g. 0x01 = sampled)
    pub trace_flags: u8,
    /// Optional vendor-specific trace state
    pub trace_state: Option<String>,
    /// visp-internal parent span ID (16 hex), used for cross-mpsc parentage
    /// tracking.  Set to `None` when parsed from a W3C traceparent header.
    pub parent_span_id: Option<String>,
}

impl TraceContext {
    /// Create a new `TraceContext`, validating hex length and character set.
    ///
    /// `parent_span_id` is optional (16 hex or `None`); if provided it is
    /// validated for hex content and length.
    pub fn new(
        trace_id: String,
        span_id: String,
        trace_flags: u8,
        trace_state: Option<String>,
        parent_span_id: Option<String>,
    ) -> Result<Self, TraceContextError> {
        // Validate trace_id length (32 hex chars)
        if trace_id.len() != 32 {
            return Err(TraceContextError::InvalidTraceIdLength);
        }
        // Validate span_id length (16 hex chars)
        if span_id.len() != 16 {
            return Err(TraceContextError::InvalidSpanIdLength);
        }
        // Validate hex characters for both
        if !trace_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(TraceContextError::InvalidHex);
        }
        if !span_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(TraceContextError::InvalidHex);
        }
        // Validate parent_span_id if present
        if let Some(ref psid) = parent_span_id {
            if psid.len() != 16 {
                return Err(TraceContextError::InvalidSpanIdLength);
            }
            if !psid.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(TraceContextError::InvalidHex);
            }
        }
        // Normalize to lowercase
        let trace_id = trace_id.to_ascii_lowercase();
        let span_id = span_id.to_ascii_lowercase();
        let parent_span_id = parent_span_id.map(|s| s.to_ascii_lowercase());

        Ok(Self {
            trace_id,
            span_id,
            trace_flags,
            trace_state,
            parent_span_id,
        })
    }

    /// Parse a W3C `traceparent` header value.
    ///
    /// Format: `{version}-{trace_id}-{span_id}-{trace_flags}`
    ///
    /// Only version `00` is supported.
    pub fn from_traceparent(s: &str) -> Result<Self, TraceContextError> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 4 {
            return Err(TraceContextError::InvalidTraceparent);
        }
        let version = parts[0];
        let trace_id = parts[1];
        let span_id = parts[2];
        let flags_str = parts[3];

        // Only support version 00
        if version != "00" {
            return Err(TraceContextError::InvalidTraceparent);
        }

        // Validate flags is 2 hex chars
        if flags_str.len() != 2 || !flags_str.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(TraceContextError::InvalidTraceparent);
        }
        let trace_flags =
            u8::from_str_radix(flags_str, 16).map_err(|_| TraceContextError::InvalidTraceparent)?;

        // Use the existing validation for trace_id and span_id
        // (also checks hex character validity and length).
        // parent_span_id is always None for W3C parsed headers.
        Self::new(
            trace_id.to_string(),
            span_id.to_string(),
            trace_flags,
            None,
            None,
        )
    }
}

// ---------------------------------------------------------------------------
// SpanW3CId — newtype for W3C span ID stored in span extensions.
// ---------------------------------------------------------------------------

/// Newtype wrapping a 16-hex W3C span ID, stored in span extensions for
/// cross‑layer span identification.
///
/// Written to the extension of every `visp.agent.run`, `visp.agent.iteration`,
/// and `visp.subagent.spawn` span so that [`ParentLinkLayer`] can register
/// a bidirectional mapping between W3C span IDs and tracing span IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanW3CId(pub String);

// ---------------------------------------------------------------------------
// W3C ID generators
// ---------------------------------------------------------------------------

/// Generate a W3C-compatible 16-hex-character span ID from a random UUID v4.
pub fn generate_w3c_span_id() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")[..16].to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "trace_context_tests.rs"]
mod tests;
