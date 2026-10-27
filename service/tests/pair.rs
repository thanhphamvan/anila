//! Acceptance tests for `anila_service::pair` (task T34). Do not edit.

use anila_proto::HostId;
use anila_service::config::Config;
use anila_service::pair::*;

fn host(s: &str) -> HostId {
    HostId::new(s).unwrap()
}

#[test]
fn default_names() {
    assert_eq!(default_name("phams-mac-mini"), host("phams-mac-mini"));
    assert_eq!(
        default_name("Phams-Mac-mini.local"),
        host("phams-mac-mini-local")
    );
    assert_eq!(default_name("My MacBook Pro (2)"), host("my-macbook-pro-2"));
    assert_eq!(default_name("--build__box--"), host("build-box"));
    assert_eq!(
        default_name("Đà-Lạt"),
        host("l-t"),
        "each non-ASCII run becomes one dash"
    );
    assert_eq!(default_name("..."), host("mac"));
    assert_eq!(default_name(""), host("mac"));
    let long = format!("{}-{}", "a".repeat(31), "b".repeat(10));
    assert_eq!(
        default_name(&long),
        host(&"a".repeat(31)),
        "cut to 32, trailing dash removed"
    );
}

const EXISTING: &str = "\
# my config
host   = \"my-mac\"
worker = \"https://old.example.com\"
# host = \"commented\"
key_file = \"~/k\"

[[preset]]
id    = \"shell\"
label = \"Shell\"
";

#[test]
fn rewrites_only_host_and_worker() {
    let out = rewrite_config(
        EXISTING,
        &host("h7abcdefghijklmn"),
        Some("https://anila.example.com"),
    );
    assert_eq!(
        out,
        EXISTING
            .replace("host   = \"my-mac\"", "host = \"h7abcdefghijklmn\"")
            .replace(
                "worker = \"https://old.example.com\"",
                "worker = \"https://anila.example.com\""
            )
    );
    let c = Config::parse(&out).unwrap();
    assert_eq!(c.host.as_str(), "h7abcdefghijklmn");
    assert_eq!(c.worker, "https://anila.example.com");
    assert_eq!(c.presets.len(), 1);
}

#[test]
fn keeps_worker_when_not_given() {
    let out = rewrite_config(EXISTING, &host("h2"), None);
    assert!(out.contains("worker = \"https://old.example.com\""));
    assert!(out.contains("# host = \"commented\""), "comments untouched");
    assert_eq!(Config::parse(&out).unwrap().host.as_str(), "h2");
}

#[test]
fn adds_missing_keys_before_the_first_table() {
    let text = "grace_secs = 60\n\n[[preset]]\nid = \"a\"\nlabel = \"A\"\n";
    let out = rewrite_config(text, &host("h3"), Some("https://w.example.com"));
    let first_table = out.find("[[preset]]").unwrap();
    assert!(out.find("host = \"h3\"").unwrap() < first_table);
    assert!(out.find("worker = \"https://w.example.com\"").unwrap() < first_table);
    assert!(out.starts_with("grace_secs = 60\n"));
    let c = Config::parse(&out).unwrap();
    assert_eq!((c.host.as_str(), c.grace_secs), ("h3", 60));
    assert_eq!(c.presets[0].id, "a", "the preset table keeps its keys");

    let out = rewrite_config("", &host("h4"), Some("https://w.example.com"));
    assert_eq!(Config::parse(&out).unwrap().host.as_str(), "h4");
}

#[test]
fn ignores_host_inside_tables() {
    let text = "host = \"old\"\nworker = \"https://w.example.com\"\n\n[[preset]]\nid = \"a\"\nlabel = \"host = x\"\n";
    let out = rewrite_config(text, &host("h5"), None);
    assert!(out.contains("label = \"host = x\""));
    assert_eq!(out.matches("host = \"h5\"").count(), 1);
}

#[test]
fn new_config_parses() {
    let out = new_config(&host("h6"), "https://w.example.com");
    let c = Config::parse(&out).unwrap();
    assert_eq!(c.host.as_str(), "h6");
    assert_eq!(c.worker, "https://w.example.com");
    assert!(!c.presets.is_empty(), "keeps the example presets");
}
