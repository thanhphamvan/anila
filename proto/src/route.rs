//! How the host room classifies a text frame without understanding it.

use serde::Deserialize;

use crate::Tab;

/// `t` values the room handles itself. Every other frame is routed.
pub const HOP_TYPES: &[&str] = &[
    "hello",
    "welcome",
    "status",
    "challenge",
    "auth",
    "tab_gone",
    "ping",
    "pong",
    "heartbeat",
    "watch",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Handle in the room.
    Hop,
    /// Forward unchanged to the other side of `tab`.
    Routed(Tab),
}

#[derive(Deserialize)]
struct Peek<'a> {
    #[serde(borrow)]
    t: &'a str,
    tab: Option<Tab>,
}

/// Reads only `t` and `tab`. `error` is a hop frame when it has no `tab`
/// (a room error) and routed when it has one (a service error).
pub fn classify(text: &str) -> Option<Route> {
    let peek: Peek = serde_json::from_str(text).ok()?;
    if HOP_TYPES.contains(&peek.t) {
        return Some(Route::Hop);
    }
    match (peek.t, peek.tab) {
        (_, Some(tab)) => Some(Route::Routed(tab)),
        ("error", None) => Some(Route::Hop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(
            classify(r#"{"t":"hello","proto":1,"tab":9}"#),
            Some(Route::Hop)
        );
        assert_eq!(classify(r#"{"t":"tab_gone","tab":9}"#), Some(Route::Hop));
        assert_eq!(
            classify(r#"{"t":"list","tab":9,"req":1}"#),
            Some(Route::Routed(9))
        );
        assert_eq!(
            classify(r#"{"t":"error","code":"limit","msg":""}"#),
            Some(Route::Hop)
        );
        assert_eq!(
            classify(r#"{"t":"error","tab":3,"code":"exists","msg":""}"#),
            Some(Route::Routed(3))
        );
        assert_eq!(classify(r#"{"t":"list"}"#), None);
        assert_eq!(classify("not json"), None);
    }
}
