//! Public documentation. Operational routes are deliberately excluded.

use serde_json::{json, Map, Value};

/// Canonical public paths, also used by documentation compatibility tests.
pub const NETWORK_PATHS: &[&str] = &[
    "/v1/network/",
    "/v1/network/{system}/",
    "/v1/network/{system}/{project}/",
    "/v1/network/{system}/{project}/{target}",
    "/v1/network/{system}/{project}/{target}/{status}",
];

pub fn html() -> &'static str {
    include_str!("../assets/index.html")
}

/// Generate the public description once at startup, never per request.
pub fn openapi() -> Value {
    let routes = [
        ("listNetworks", "List all systems", "NetworkDirectory", 0),
        ("getSystem", "List projects in a system", "System", 1),
        ("getProject", "List targets in a project", "Project", 2),
        ("getTarget", "List network nodes in a target", "Target", 3),
        ("getTargetByStatus", "Filter network nodes by status", "Target", 4),
    ];
    let names = ["system", "project", "target", "status"];
    let mut paths = Map::new();
    for (path, (operation, summary, schema, count)) in NETWORK_PATHS.iter().zip(routes) {
        let parameters: Vec<Value> = names[..count]
            .iter()
            .map(|name| json!({"$ref": format!("#/components/parameters/{name}")}))
            .collect();
        let mut responses = Map::new();
        responses.insert("200".into(), json!({
            "description": "Current availability. An unmatched integer status returns an empty network array.",
            "headers": {"Access-Control-Allow-Origin": {"schema": {"type": "string"}}},
            "content": {"application/json": {"schema": {"$ref": format!("#/components/schemas/{schema}")}}}
        }));
        if count > 0 {
            responses.insert("403".into(), json!({
                "description": "The requested system, project or target does not exist. This legacy status does not require authentication.",
                "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Error"},
                    "examples": {
                        "system": {"value": {"error": "System not found"}},
                        "project": {"value": {"error": "Project not found"}},
                        "target": {"value": {"error": "Target not found"}}
                    }
                }}
            }));
        }
        if count == 4 {
            responses.insert("400".into(), json!({
                "description": "The status is not an integer.",
                "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Error"},
                    "example": {"error": "Invalid status provided"}
                }}
            }));
        }
        responses.insert("429".into(), json!({
            "description": "Deployment-specific sustained rate or burst allowance exceeded. Back off before retrying.",
            "headers": {"Retry-After": {"description": "Delay in seconds before retrying.", "schema": {"type": "string"}}}
        }));
        responses.insert("503".into(), json!({"description": "Service capacity temporarily unavailable. Retry with backoff."}));
        paths.insert((*path).into(), json!({"get": {
            "operationId": operation,
            "summary": summary,
            "description": "Public read-only access. HEAD and OPTIONS are supported. Names are discovered from the parent resource and may grow dynamically.",
            "tags": ["Network"],
            "security": [],
            "parameters": parameters,
            "responses": responses
        }}));
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "JCB Network API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Public availability directory for JCB repository mirrors. Names and records grow dynamically. No authentication is required. A status of 1 denotes online and 0 denotes offline; integer status filters are matched exactly. System and project paths use the trailing slashes shown in the specification."
        },
        "servers": [{"url": "/", "description": "This installation"}],
        "tags": [{"name": "Network", "description": "Discover available repository mirrors"}],
        "security": [],
        "paths": paths,
        "components": {
            "parameters": {
                "system": {"name": "system", "in": "path", "required": true, "description": "System name discovered from the directory.", "schema": {"type": "string", "minLength": 1}, "example": "community"},
                "project": {"name": "project", "in": "path", "required": true, "description": "Project name within the selected system.", "schema": {"type": "string", "minLength": 1}, "example": "jcb"},
                "target": {"name": "target", "in": "path", "required": true, "description": "Target name within the selected project.", "schema": {"type": "string", "minLength": 1}, "example": "super-powers"},
                "status": {"name": "status", "in": "path", "required": true, "description": "Exact integer status match; 1 is online and 0 is offline. Other integer values are accepted and return any matching records.", "schema": {"type": "integer"}, "example": 1}
            },
            "schemas": {
                "NetworkDirectory": {
                    "type": "object", "required": ["systems"],
                    "properties": {"systems": {"type": "array", "items": {"$ref": "#/components/schemas/System"}}}
                },
                "System": {
                    "type": "object", "required": ["system", "projects"],
                    "properties": {"system": {"type": "string"}, "projects": {"type": "array", "items": {"$ref": "#/components/schemas/Project"}}}
                },
                "Project": {
                    "type": "object", "required": ["project", "targets"],
                    "properties": {"project": {"type": "string"}, "targets": {"type": "array", "items": {"$ref": "#/components/schemas/Target"}}}
                },
                "Target": {
                    "type": "object", "required": ["target", "network"],
                    "properties": {"target": {"type": "string"}, "network": {"type": "array", "items": {"$ref": "#/components/schemas/NetworkNode"}}},
                    "example": {"target": "super-powers", "network": [{"id": 28, "name": "codeberg.org/joomla/super-powers", "url": "https://codeberg.org/joomla/super-powers", "status": 1}]}
                },
                "NetworkNode": {
                    "type": "object", "required": ["id", "name", "url", "status"],
                    "properties": {
                        "id": {"oneOf": [{"type": "integer"}, {"type": "string"}]},
                        "name": {"type": "string"},
                        "url": {"type": "string", "description": "Repository URL as supplied by the monitoring system."},
                        "status": {"type": "integer", "format": "int64"}
                    }
                },
                "Error": {"type": "object", "required": ["error"], "properties": {"error": {"type": "string"}}}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_only_public_routes_without_authentication() {
        let document = openapi();
        let paths = document["paths"].as_object().unwrap();
        assert_eq!(paths.len(), NETWORK_PATHS.len());
        for path in NETWORK_PATHS {
            let operation = &paths[*path]["get"];
            assert_eq!(operation["security"], json!([]));
            assert!(operation["responses"]["200"].is_object());
        }
        let serialized = document.to_string().to_lowercase();
        for private_word in ["webhook", "admin", "authorization", "token"] {
            assert!(!serialized.contains(private_word));
            assert!(!html().to_lowercase().contains(private_word));
        }
        assert_eq!(document["servers"][0]["url"], "/");
    }

    #[test]
    fn filter_is_integer_without_an_incorrect_binary_enum() {
        let document = openapi();
        let status = &document["components"]["parameters"]["status"]["schema"];
        assert_eq!(status["type"], "integer");
        assert!(status.get("enum").is_none());
        assert_eq!(document["components"]["schemas"]["Target"]["example"]["network"][0]["status"], 1);
        assert!(html().contains("/openapi.json"));
        assert!(!html().contains("api.joomlacomponentbuilder.com"));
    }
}
