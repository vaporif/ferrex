use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names, reason = "field name is the wire format")]
pub struct Hello {
    pub ferrex_hello: u32,
    pub key: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefusalKind {
    Protocol,
    Key,
    Init,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RefusalKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn encode_line(value: &impl Serialize) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    line
}

impl Hello {
    #[must_use]
    pub const fn new(key: String, namespace: String) -> Self {
        Self {
            ferrex_hello: PROTOCOL_VERSION,
            key,
            namespace,
        }
    }

    #[must_use]
    pub fn encode(&self) -> String {
        encode_line(self)
    }

    pub fn decode(line: &str) -> Result<Self, HelloReply> {
        let protocol = |msg: String| HelloReply::refuse(RefusalKind::Protocol, msg);
        let value: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|e| protocol(format!("malformed hello: {e}")))?;
        let version = value
            .get("ferrex_hello")
            .and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(PROTOCOL_VERSION)) {
            let got = version.map_or_else(|| "missing".to_owned(), |v| v.to_string());
            return Err(protocol(format!(
                "protocol version mismatch: proxy sent {got}, daemon speaks {PROTOCOL_VERSION}"
            )));
        }
        serde_json::from_value(value).map_err(|e| protocol(format!("malformed hello: {e}")))
    }

    pub fn check(&self, key: &str) -> Result<(), HelloReply> {
        if self.key == key {
            Ok(())
        } else {
            Err(HelloReply::refuse(
                RefusalKind::Key,
                format!("key mismatch: proxy sent {}, daemon is {key}", self.key),
            ))
        }
    }
}

impl HelloReply {
    #[must_use]
    pub const fn ok() -> Self {
        Self {
            ok: true,
            kind: None,
            error: None,
        }
    }

    #[must_use]
    pub fn refuse(kind: RefusalKind, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            kind: Some(kind),
            error: Some(error.into()),
        }
    }

    #[must_use]
    pub fn encode(&self) -> String {
        encode_line(self)
    }

    pub fn decode(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("malformed hello reply: {e}"))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let hello = Hello::new("k".into(), "ns".into());
        assert_eq!(Hello::decode(&hello.encode()).unwrap(), hello);
    }

    #[test]
    fn ok_reply_encodes_exactly() {
        assert_eq!(HelloReply::ok().encode(), "{\"ok\":true}\n");
    }

    #[test]
    fn refusal_encodes_kind_lowercase() {
        let line = HelloReply::refuse(RefusalKind::Init, "boom").encode();
        assert!(line.contains("\"kind\":\"init\""));
        assert!(line.contains("\"error\":\"boom\""));
    }

    #[test]
    fn version_mismatch_is_protocol() {
        let err = Hello::decode(r#"{"ferrex_hello":2,"key":"k","namespace":"n"}"#).unwrap_err();
        assert_eq!(err.kind, Some(RefusalKind::Protocol));
        let msg = err.error.unwrap();
        assert!(msg.contains('2') && msg.contains('1'));
    }

    #[test]
    fn malformed_line_is_protocol() {
        let err = Hello::decode("not json").unwrap_err();
        assert_eq!(err.kind, Some(RefusalKind::Protocol));
    }

    #[test]
    fn key_mismatch_is_key() {
        let err = Hello::new("mine".into(), "ns".into())
            .check("other")
            .unwrap_err();
        assert_eq!(err.kind, Some(RefusalKind::Key));
        let msg = err.error.unwrap();
        assert!(msg.contains("mine") && msg.contains("other"));
    }

    #[test]
    fn reply_round_trips() {
        let reply = HelloReply::refuse(RefusalKind::Session, "bad ns");
        assert_eq!(HelloReply::decode(&reply.encode()).unwrap(), reply);
        assert!(HelloReply::decode("nope").is_err());
    }
}
