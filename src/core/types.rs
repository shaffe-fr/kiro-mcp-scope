//! Catalog and MCP server entry.
//!
//! An entry exists in two variants, distinguished solely by their *transport*
//! keys: `command` (+ `args`) for a local entry, `url` (+ `headers`) for a
//! remote one. Everything else — `type`, `env`, `disabled`, `autoApprove`,
//! `disabledTools`, and any field Kiro may add later — is common to both.
//!
//! An entry is stored as an ordered map of raw JSON values rather than a typed
//! struct. That preserves fields kms does not know about, so a project's
//! `mcp.json` written by Kiro round-trips untouched.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One MCP server entry, holding every field as it appears in the JSON.
///
/// `IndexMap` semantics are provided by `serde_json`'s `preserve_order`
/// feature, which backs [`Map`] with an insertion-ordered map. Serialization
/// re-imposes a deterministic order (see `workspace`), so the incoming order is
/// not relied upon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpServer {
    fields: Map<String, Value>,
}

/// Which transport an entry uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    Local,
    Remote,
}

impl McpServer {
    pub fn from_map(fields: Map<String, Value>) -> Self {
        Self { fields }
    }

    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    pub fn fields_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.fields
    }

    pub fn into_map(self) -> Map<String, Value> {
        self.fields
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// A local entry carries a string `command`.
    pub fn is_local(&self) -> bool {
        matches!(self.fields.get("command"), Some(Value::String(_)))
    }

    /// A remote entry carries a string `url`.
    pub fn is_remote(&self) -> bool {
        matches!(self.fields.get("url"), Some(Value::String(_)))
    }

    pub fn kind(&self) -> ServerKind {
        if self.is_remote() {
            ServerKind::Remote
        } else {
            ServerKind::Local
        }
    }
}

/// The catalog: the single source of server definitions, which Kiro never reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Server name to entry, in the order the catalog file lists them.
    pub servers: Map<String, Value>,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<McpServer> {
        self.servers
            .get(name)
            .and_then(Value::as_object)
            .cloned()
            .map(McpServer::from_map)
    }

    /// Server names, sorted, for deterministic listing.
    pub fn names_sorted(&self) -> Vec<String> {
        let mut names: Vec<String> = self.servers.keys().cloned().collect();
        names.sort();
        names
    }
}
