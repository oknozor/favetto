//! Read-only MCP resources over the daemon's observation RPCs.
//!
//! Three resources are exposed, two static and one templated:
//!
//! | URI | RPC |
//! |-----|-----|
//! | `favetto://events` (optional `?limit=`) | `events.tail` |
//! | `favetto://catalog` | `catalog.list` |
//! | `favetto://workflow/{root_id}` | `workflow.inspect` |

use anyhow::Context;
use serde_json::{json, Value};
use uuid::Uuid;

use favetto_core::rpc::method;
use favetto_tui::client::Client;

use crate::rpc::ok_result;
use crate::tools::ToolError;

/// The event log tail.
pub const EVENTS_URI: &str = "favetto://events";
/// The task catalog.
pub const CATALOG_URI: &str = "favetto://catalog";
/// Template for one workflow root's runtime view.
pub const WORKFLOW_TEMPLATE: &str = "favetto://workflow/{root_id}";
/// Prefix of a concrete workflow resource URI.
const WORKFLOW_PREFIX: &str = "favetto://workflow/";

/// A static resource shown by `resources/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSpec {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

/// A resource template shown by `resources/templates/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceTemplateSpec {
    pub uri_template: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

/// The two static resources.
pub fn list() -> Vec<ResourceSpec> {
    vec![
        ResourceSpec {
            uri: EVENTS_URI,
            name: "events",
            description: "Tail of the persisted event log. Accepts `?limit=N`.",
            mime_type: "application/json",
        },
        ResourceSpec {
            uri: CATALOG_URI,
            name: "catalog",
            description: "The task catalog (definitions, not runtime instances).",
            mime_type: "application/json",
        },
    ]
}

/// The workflow-root resource template.
pub fn templates() -> Vec<ResourceTemplateSpec> {
    vec![ResourceTemplateSpec {
        uri_template: WORKFLOW_TEMPLATE,
        name: "workflow",
        description: "Runtime view of one workflow root, keyed by its root task id.",
        mime_type: "application/json",
    }]
}

/// The RPC a resource URI maps to.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceCall {
    Rpc { method: &'static str, params: Value },
}

/// Parse a resource URI into the RPC it reads. Pure, so it is unit-testable.
pub fn resolve(uri: &str) -> Result<ResourceCall, ToolError> {
    if uri == EVENTS_URI || uri.starts_with(&format!("{EVENTS_URI}?")) {
        let params = match limit_query(uri)? {
            Some(limit) => json!({ "limit": limit }),
            None => json!({}),
        };
        return Ok(ResourceCall::Rpc {
            method: method::EVENTS_TAIL,
            params,
        });
    }
    if uri == CATALOG_URI {
        return Ok(ResourceCall::Rpc {
            method: method::CATALOG_LIST,
            params: json!({}),
        });
    }
    if let Some(rest) = uri.strip_prefix(WORKFLOW_PREFIX) {
        let id = rest.split(['?', '#']).next().unwrap_or_default();
        Uuid::parse_str(id)
            .map_err(|_| ToolError::new(format!("invalid workflow root id in `{uri}`")))?;
        return Ok(ResourceCall::Rpc {
            method: method::WORKFLOW_INSPECT,
            params: json!({ "root_id": id }),
        });
    }
    Err(ToolError::new(format!("unknown resource `{uri}`")))
}

/// Read a resource, returning its decoded JSON payload.
pub async fn read(uri: &str, client: &Client) -> anyhow::Result<Value> {
    let ResourceCall::Rpc { method, params } = resolve(uri).map_err(anyhow::Error::from)?;
    let resp = client
        .request(method, params)
        .await
        .with_context(|| format!("read resource `{uri}`"))?;
    ok_result(&resp, method)
}

/// Extract a `limit` from a `?limit=N` query string, if present.
fn limit_query(uri: &str) -> Result<Option<u64>, ToolError> {
    let Some(query) = uri.split_once('?').map(|(_, query)| query) else {
        return Ok(None);
    };
    let limit = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "limit").then_some(value)
    });
    match limit {
        None => Ok(None),
        Some(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| ToolError::new(format!("invalid `limit` in `{uri}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_resources_and_template_are_declared() {
        let resources = list();
        assert_eq!(resources.len(), 2);
        assert!(resources.iter().any(|r| r.uri == EVENTS_URI));
        assert!(resources.iter().any(|r| r.uri == CATALOG_URI));
        let templates = templates();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].uri_template, WORKFLOW_TEMPLATE);
    }

    #[test]
    fn events_and_catalog_map_to_tail_and_list() {
        assert_eq!(
            resolve(EVENTS_URI).unwrap(),
            ResourceCall::Rpc {
                method: method::EVENTS_TAIL,
                params: json!({})
            }
        );
        assert_eq!(
            resolve("favetto://events?limit=25").unwrap(),
            ResourceCall::Rpc {
                method: method::EVENTS_TAIL,
                params: json!({ "limit": 25 })
            }
        );
        assert_eq!(
            resolve(CATALOG_URI).unwrap(),
            ResourceCall::Rpc {
                method: method::CATALOG_LIST,
                params: json!({})
            }
        );
    }

    #[test]
    fn workflow_template_parses_a_uuid() {
        let id = "00000000-0000-0000-0000-0000000000ab";
        assert_eq!(
            resolve(&format!("{WORKFLOW_PREFIX}{id}")).unwrap(),
            ResourceCall::Rpc {
                method: method::WORKFLOW_INSPECT,
                params: json!({ "root_id": id })
            }
        );
    }

    #[test]
    fn unknown_and_malformed_uris_error() {
        assert!(resolve("favetto://nope").is_err());
        assert!(resolve(&format!("{WORKFLOW_PREFIX}not-a-uuid")).is_err());
        assert!(resolve("favetto://events?limit=abc").is_err());
    }
}
