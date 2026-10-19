use std::collections::HashMap;

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

const INITIALIZE: &str = "initialize";
const INITIALIZED: &str = "notifications/initialized";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientLine {
    Request {
        id: Value,
        method: String,
    },
    Notification {
        method: String,
    },
    /// A `notifications/initialized` the replay will send; never queued.
    CacheOnly,
    Untracked,
}

/// The daemon connection as the main loop sees it; decides how client lines are handled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    #[default]
    Up,
    Down,
    Reconnecting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptRequest {
    pub id: Value,
    pub line: Vec<u8>,
}

/// Follows the JSON-RPC traffic just enough to reconnect a client mid-session.
#[derive(Debug, Default)]
pub struct Tracker {
    in_flight: HashMap<String, (Value, String)>,
    initialised: bool,
    initialize: Option<Vec<u8>>,
    initialized: Option<Vec<u8>>,
    kept_first: Option<KeptRequest>,
    link: Link,
    replay_started: bool,
}

/// Only the members the tracker reads; `IgnoredAny` skips multi-MB params and results unparsed.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default, deserialize_with = "present")]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default, deserialize_with = "present")]
    result: Option<IgnoredAny>,
    #[serde(default, deserialize_with = "present")]
    error: Option<IgnoredAny>,
}

/// Maps a member that is present, even as `null`, to `Some`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

impl Envelope {
    /// Parses a JSON object line; batch arrays (which serde would read as a struct) and
    /// anything malformed give `None`.
    fn parse(line: &[u8]) -> Option<Self> {
        if line.trim_ascii_start().first() != Some(&b'{') {
            return None;
        }
        serde_json::from_slice(line).ok()
    }
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

/// The id of a JSON-RPC request line.
pub fn request_id(line: &[u8]) -> Option<Value> {
    match Envelope::parse(line)? {
        Envelope {
            id,
            method: Some(_),
            ..
        } => id,
        _ => None,
    }
}

/// How `line` answers request `id`; `None` if it is any other line.
pub fn response_outcome(line: &[u8], id: &Value) -> Option<Outcome> {
    let Envelope {
        id: Some(got),
        method: None,
        result,
        error,
    } = Envelope::parse(line)?
    else {
        return None;
    };
    if id_key(&got) != id_key(id) {
        return None;
    }
    match (result, error) {
        (_, Some(_)) => Some(Outcome::Error),
        (Some(_), None) => Some(Outcome::Success),
        (None, None) => None,
    }
}

fn counts_for_lifecycle(method: &str) -> bool {
    !matches!(method, "ping" | "server/discover")
}

impl Tracker {
    pub fn on_client_line(&mut self, line: &[u8]) -> ClientLine {
        let Some(Envelope {
            id,
            method: Some(method),
            ..
        }) = Envelope::parse(line)
        else {
            return ClientLine::Untracked;
        };
        let Some(id) = id else {
            if method == INITIALIZED {
                self.initialized = Some(line.to_vec());
                if self.link == Link::Reconnecting && !self.replay_started {
                    return ClientLine::CacheOnly;
                }
            }
            return ClientLine::Notification { method };
        };
        self.in_flight
            .insert(id_key(&id), (id.clone(), method.clone()));
        if !self.initialised {
            if method == INITIALIZE {
                self.initialize = Some(line.to_vec());
            }
            if self.kept_first.is_none() && counts_for_lifecycle(&method) {
                self.kept_first = Some(KeptRequest {
                    id: id.clone(),
                    line: line.to_vec(),
                });
            }
        }
        ClientLine::Request { id, method }
    }

    pub fn on_daemon_line(&mut self, line: &[u8]) {
        let Some(Envelope {
            id: Some(id),
            method: None,
            result,
            error,
        }) = Envelope::parse(line)
        else {
            return;
        };
        let Some((_, method)) = self.in_flight.remove(&id_key(&id)) else {
            return;
        };
        if self.initialised {
            return;
        }
        if result.is_some() && counts_for_lifecycle(&method) {
            self.initialised = true;
            self.kept_first = None;
        } else if error.is_some() && self.kept_first.as_ref().is_some_and(|k| k.id == id) {
            self.kept_first = None;
        }
    }

    #[must_use]
    pub const fn initialised(&self) -> bool {
        self.initialised
    }

    /// Entering any link state starts it with no replay under way.
    pub const fn set_link(&mut self, link: Link) {
        self.link = link;
        self.replay_started = false;
    }

    /// From here on a `notifications/initialized` is queued: the replay has read the cache.
    pub const fn mark_replay_started(&mut self) {
        self.replay_started = true;
    }

    pub fn take_in_flight(&mut self) -> Vec<Value> {
        self.in_flight.drain().map(|(_, (id, _))| id).collect()
    }

    #[allow(dead_code, reason = "used by Task 13")]
    pub fn take_in_flight_except(&mut self, keep: &Value) -> Vec<Value> {
        let keep = id_key(keep);
        self.in_flight
            .extract_if(|key, _| *key != keep)
            .map(|(_, (id, _))| id)
            .collect()
    }

    #[allow(dead_code, reason = "used by Task 13")]
    #[must_use]
    pub fn is_in_flight(&self, id: &Value) -> bool {
        self.in_flight.contains_key(&id_key(id))
    }

    #[must_use]
    pub fn cached_initialize(&self) -> Option<&[u8]> {
        self.initialize.as_deref()
    }

    #[must_use]
    pub fn cached_initialized(&self) -> Option<&[u8]> {
        self.initialized.as_deref()
    }

    #[allow(dead_code, reason = "used by Task 13")]
    #[must_use]
    pub const fn kept_first(&self) -> Option<&KeptRequest> {
        self.kept_first.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn line(v: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(v).unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn request(id: &Value, method: &str) -> Vec<u8> {
        line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}}))
    }

    fn result(id: &Value) -> Vec<u8> {
        line(&json!({"jsonrpc": "2.0", "id": id, "result": {}}))
    }

    fn error(id: &Value) -> Vec<u8> {
        line(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "bad"}}))
    }

    fn notification(method: &str) -> Vec<u8> {
        line(&json!({"jsonrpc": "2.0", "method": method}))
    }

    #[test]
    fn request_recorded_and_cleared() {
        let mut t = Tracker::default();
        assert_eq!(
            t.on_client_line(&request(&json!(7), "tools/list")),
            ClientLine::Request {
                id: json!(7),
                method: "tools/list".into()
            }
        );
        assert!(t.is_in_flight(&json!(7)));
        t.on_daemon_line(&result(&json!(7)));
        assert!(!t.is_in_flight(&json!(7)));
        assert!(t.take_in_flight().is_empty());
    }

    #[test]
    fn notification_is_not_recorded() {
        let mut t = Tracker::default();
        assert_eq!(
            t.on_client_line(&notification("notifications/cancelled")),
            ClientLine::Notification {
                method: "notifications/cancelled".into()
            }
        );
        assert!(t.take_in_flight().is_empty());
    }

    #[test]
    fn tracker_distinguishes_numeric_and_string_ids() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "tools/list"));
        t.on_client_line(&request(&json!("1"), "tools/list"));
        t.on_daemon_line(&result(&json!("1")));
        assert!(t.is_in_flight(&json!(1)));
        assert!(!t.is_in_flight(&json!("1")));
        assert_eq!(t.take_in_flight(), vec![json!(1)]);
    }

    #[test]
    fn batch_array_and_garbage_are_untracked() {
        let mut t = Tracker::default();
        for garbage in [
            &b"[{\"id\":1,\"method\":\"x\"}]\n"[..],
            b"not json\n",
            b"\xff\xfe{\n",
            b"{\"id\":1,\"method\":5}\n",
        ] {
            assert_eq!(t.on_client_line(garbage), ClientLine::Untracked);
        }
        assert!(t.take_in_flight().is_empty());
        assert!(t.kept_first().is_none());
    }

    #[test]
    fn daemon_batch_array_does_not_clear() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "initialize"));
        t.on_daemon_line(b"[{\"id\":1,\"result\":{}}]\n");
        assert!(t.is_in_flight(&json!(1)));
        assert!(!t.initialised());
    }

    #[test]
    fn initialize_result_marks_initialised() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "initialize"));
        assert!(!t.initialised());
        t.on_daemon_line(&result(&json!(1)));
        assert!(t.initialised());
        assert!(t.kept_first().is_none());
    }

    #[test]
    fn null_result_marks_initialised() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "initialize"));
        t.on_daemon_line(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n");
        assert!(t.initialised());
    }

    #[test]
    fn initialize_error_does_not() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "initialize"));
        t.on_daemon_line(&error(&json!(1)));
        assert!(!t.initialised());
        assert!(!t.is_in_flight(&json!(1)));
    }

    #[test]
    fn ping_result_does_not_initialise() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "ping"));
        t.on_client_line(&request(&json!(2), "server/discover"));
        t.on_daemon_line(&result(&json!(1)));
        t.on_daemon_line(&result(&json!(2)));
        assert!(!t.initialised());
        assert!(t.kept_first().is_none());
    }

    #[test]
    fn unrecorded_result_does_not_initialise() {
        let mut t = Tracker::default();
        t.on_daemon_line(&result(&json!(9)));
        assert!(!t.initialised());
    }

    #[test]
    fn tools_call_result_initialises_inline() {
        let mut t = Tracker::default();
        let call = request(&json!(3), "tools/call");
        t.on_client_line(&call);
        assert_eq!(
            t.kept_first(),
            Some(&KeptRequest {
                id: json!(3),
                line: call
            })
        );
        t.on_daemon_line(&result(&json!(3)));
        assert!(t.initialised());
        assert!(t.kept_first().is_none());
    }

    #[test]
    fn kept_first_replaced_only_after_error() {
        let mut t = Tracker::default();
        let a = request(&json!("a"), "tools/call");
        let b = request(&json!("b"), "tools/call");
        let c = request(&json!("c"), "tools/call");
        t.on_client_line(&a);
        t.on_client_line(&b);
        assert_eq!(t.kept_first().map(|k| &k.line), Some(&a));
        t.on_daemon_line(&error(&json!("a")));
        assert!(!t.initialised());
        t.on_client_line(&c);
        assert_eq!(t.kept_first().map(|k| &k.line), Some(&c));
    }

    #[test]
    fn error_for_other_request_keeps_kept_first() {
        let mut t = Tracker::default();
        let a = request(&json!("a"), "tools/call");
        t.on_client_line(&a);
        t.on_client_line(&request(&json!("b"), "tools/call"));
        t.on_daemon_line(&error(&json!("b")));
        assert_eq!(t.kept_first().map(|k| &k.line), Some(&a));
    }

    #[test]
    fn latest_initialize_cached_until_initialised() {
        let mut t = Tracker::default();
        let first = request(&json!(1), "initialize");
        let second = request(&json!(2), "initialize");
        let third = request(&json!(3), "initialize");
        t.on_client_line(&first);
        assert_eq!(t.cached_initialize(), Some(&first[..]));
        t.on_daemon_line(&error(&json!(1)));
        t.on_client_line(&second);
        assert_eq!(t.cached_initialize(), Some(&second[..]));
        t.on_daemon_line(&result(&json!(2)));
        t.on_client_line(&third);
        assert_eq!(t.cached_initialize(), Some(&second[..]));
    }

    #[test]
    fn initialized_notification_cached_after_initialise() {
        let mut t = Tracker::default();
        let init = request(&json!(1), "initialize");
        let initialized = notification("notifications/initialized");
        t.on_client_line(&init);
        t.on_daemon_line(&result(&json!(1)));
        assert!(t.cached_initialized().is_none());
        t.on_client_line(&initialized);
        assert_eq!(t.cached_initialized(), Some(&initialized[..]));
        assert_eq!(t.cached_initialize(), Some(&init[..]));
    }

    #[test]
    fn initialized_cache_only_while_reconnecting_before_replay() {
        let mut t = Tracker::default();
        let initialized = notification("notifications/initialized");
        t.set_link(Link::Reconnecting);
        assert_eq!(t.on_client_line(&initialized), ClientLine::CacheOnly);
        assert_eq!(t.cached_initialized(), Some(&initialized[..]));
        t.mark_replay_started();
        assert_eq!(
            t.on_client_line(&initialized),
            ClientLine::Notification {
                method: "notifications/initialized".into()
            }
        );
    }

    #[test]
    fn initialized_not_cache_only_when_up_or_down() {
        let mut t = Tracker::default();
        let initialized = notification("notifications/initialized");
        for link in [Link::Up, Link::Down] {
            t.set_link(link);
            assert!(matches!(
                t.on_client_line(&initialized),
                ClientLine::Notification { .. }
            ));
        }
        t.set_link(Link::Reconnecting);
        assert_eq!(
            t.on_client_line(&notification("notifications/cancelled")),
            ClientLine::Notification {
                method: "notifications/cancelled".into()
            }
        );
    }

    #[test]
    fn set_link_clears_replay_started() {
        let mut t = Tracker::default();
        t.set_link(Link::Reconnecting);
        t.mark_replay_started();
        t.set_link(Link::Reconnecting);
        assert_eq!(
            t.on_client_line(&notification("notifications/initialized")),
            ClientLine::CacheOnly
        );
    }

    #[test]
    fn request_while_reconnecting_is_tracked() {
        let mut t = Tracker::default();
        t.set_link(Link::Reconnecting);
        t.on_client_line(&request(&json!(4), "tools/list"));
        assert!(t.is_in_flight(&json!(4)));
    }

    #[test]
    fn request_id_reads_requests_only() {
        assert_eq!(
            request_id(&request(&json!("x"), "initialize")),
            Some(json!("x"))
        );
        assert_eq!(request_id(&notification("notifications/initialized")), None);
        assert_eq!(request_id(&result(&json!(1))), None);
        assert_eq!(request_id(b"garbage\n"), None);
    }

    #[test]
    fn response_outcome_matches_id_and_kind() {
        assert_eq!(
            response_outcome(&result(&json!(1)), &json!(1)),
            Some(Outcome::Success)
        );
        assert_eq!(
            response_outcome(&error(&json!(1)), &json!(1)),
            Some(Outcome::Error)
        );
        assert_eq!(response_outcome(&result(&json!("1")), &json!(1)), None);
        assert_eq!(
            response_outcome(&request(&json!(1), "ping"), &json!(1)),
            None
        );
        assert_eq!(
            response_outcome(&notification("notifications/message"), &json!(1)),
            None
        );
    }

    #[test]
    fn take_in_flight_except_keeps_one() {
        let mut t = Tracker::default();
        t.on_client_line(&request(&json!(1), "tools/call"));
        t.on_client_line(&request(&json!(2), "ping"));
        assert_eq!(t.take_in_flight_except(&json!(1)), vec![json!(2)]);
        assert!(t.is_in_flight(&json!(1)));
    }
}
