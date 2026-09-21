//! Hope Agent 自带 MCP 服务端共用的协议协商与 2026 结果封装。
//!
//! 两个 stdio 服务端都必须走这里，避免协议版本、发现结果和
//! `resultType` 兼容规则再次漂移。

use serde_json::{json, Value};

pub const MCP_PROTOCOL_2026_07_28: &str = "2026-07-28";
pub const MCP_PROTOCOL_2025_11_25: &str = "2025-11-25";
pub const MCP_PROTOCOL_2025_06_18: &str = "2025-06-18";
pub const MCP_PROTOCOL_2025_03_26: &str = "2025-03-26";

pub const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_PROTOCOL_2026_07_28,
    MCP_PROTOCOL_2025_11_25,
    MCP_PROTOCOL_2025_06_18,
    MCP_PROTOCOL_2025_03_26,
];

/// 单条 stdio 连接的旧版协商状态。现代协议由每个请求的元数据选择，
/// 不改写连接状态；initialize 只在旧版本之间协商。
#[derive(Debug, Clone, Copy)]
pub struct McpProtocolSession {
    negotiated_version: &'static str,
}

impl Default for McpProtocolSession {
    fn default() -> Self {
        Self {
            negotiated_version: MCP_PROTOCOL_2025_11_25,
        }
    }
}

impl McpProtocolSession {
    pub fn negotiate_initialize(&mut self, params: &Value) -> &'static str {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        self.negotiated_version = requested
            .and_then(|requested| {
                MCP_SUPPORTED_PROTOCOL_VERSIONS
                    .iter()
                    .copied()
                    .find(|supported| {
                        *supported == requested && *supported != MCP_PROTOCOL_2026_07_28
                    })
            })
            .unwrap_or(MCP_PROTOCOL_2025_11_25);
        self.negotiated_version
    }

    /// Per-request metadata overrides only this request, never legacy session state.
    pub fn for_request(&self, params: &Value) -> Result<Self, Value> {
        let Some(meta) = params.get("_meta") else {
            return Ok(*self);
        };
        let version = meta.get("io.modelcontextprotocol/protocolVersion");
        let Some(version) = version else {
            return Ok(*self);
        };
        let Some(requested) = version.as_str() else {
            return Err(json!({"code": -32602, "message": "Invalid protocol version metadata"}));
        };
        let Some(selected) = MCP_SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .find(|version| *version == requested)
        else {
            return Err(
                json!({"code": -32022, "message": "Unsupported protocol version",
                "data": {"supported": MCP_SUPPORTED_PROTOCOL_VERSIONS, "requested": requested}}),
            );
        };
        if selected == MCP_PROTOCOL_2026_07_28
            && !meta
                .get("io.modelcontextprotocol/clientCapabilities")
                .is_some_and(Value::is_object)
        {
            return Err(
                json!({"code": -32602, "message": "Missing or invalid client capabilities"}),
            );
        }
        Ok(Self {
            negotiated_version: selected,
        })
    }

    pub fn is_modern(&self) -> bool {
        self.negotiated_version == MCP_PROTOCOL_2026_07_28
    }

    pub fn complete_list_result(&self, mut result: Value) -> Value {
        if self.is_modern() {
            result["ttlMs"] = json!(0);
            result["cacheScope"] = json!("private");
        }
        self.complete_result(result)
    }

    pub fn negotiated_version(&self) -> &'static str {
        self.negotiated_version
    }

    pub fn complete_result(&self, mut result: Value) -> Value {
        let Some(object) = result.as_object_mut() else {
            return result;
        };
        if self.negotiated_version == MCP_PROTOCOL_2026_07_28 {
            object.insert("resultType".into(), Value::String("complete".into()));
        } else {
            object.remove("resultType");
        }
        result
    }
}

pub fn discover_result(
    capabilities: Value,
    server_name: &str,
    server_version: &str,
    instructions: &str,
) -> Value {
    json!({
        "resultType": "complete",
        "supportedVersions": MCP_SUPPORTED_PROTOCOL_VERSIONS,
        "capabilities": capabilities,
        "instructions": instructions,
        "ttlMs": 0,
        "cacheScope": "private",
        "_meta": {
            "io.modelcontextprotocol/serverInfo": {
                "name": server_name,
                "version": server_version
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_legacy_version_is_preserved_and_omits_result_type() {
        let mut session = McpProtocolSession::default();
        assert_eq!(
            session.negotiate_initialize(&json!({
                "protocolVersion": MCP_PROTOCOL_2025_03_26
            })),
            MCP_PROTOCOL_2025_03_26
        );
        assert!(session
            .complete_result(json!({}))
            .get("resultType")
            .is_none());
    }

    #[test]
    fn unknown_legacy_initialize_falls_back_to_a_legacy_version() {
        let mut session = McpProtocolSession::default();
        assert_eq!(
            session.negotiate_initialize(&json!({ "protocolVersion": "2099-01-01" })),
            MCP_PROTOCOL_2025_11_25
        );
        assert!(session
            .complete_result(json!({}))
            .get("resultType")
            .is_none());
    }
    #[test]
    fn modern_metadata_is_request_local_and_cache_fields_are_versioned() {
        let mut legacy = McpProtocolSession::default();
        legacy.negotiate_initialize(&json!({"protocolVersion": MCP_PROTOCOL_2025_03_26}));
        let modern = legacy
            .for_request(&json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_2026_07_28,
                "io.modelcontextprotocol/clientCapabilities": {}
            }}))
            .unwrap();
        let result = modern.complete_list_result(json!({"tools": []}));
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], 0);
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(legacy.negotiated_version(), MCP_PROTOCOL_2025_03_26);
        assert!(legacy
            .complete_list_result(json!({"tools": []}))
            .get("ttlMs")
            .is_none());
        let unknown = legacy
            .for_request(&json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2099-01-01"
            }}))
            .unwrap_err();
        assert_eq!(unknown["code"], -32022);
        assert_eq!(unknown["data"]["requested"], "2099-01-01");
        assert!(legacy
            .for_request(&json!({"_meta": {
                "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_2026_07_28
            }}))
            .is_err());
    }
}
