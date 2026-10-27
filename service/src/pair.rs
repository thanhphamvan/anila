//! `anila-service pair`: the machine name it asks for and the config edit
//! it makes afterwards (`docs/multi-tenant.md`).

use anila_proto::HostId;

/// The machine name `pair` asks for by default: `hostname` lowercased, every
/// run of characters outside `[a-z0-9-]` replaced by one `-`, leading and
/// trailing `-` removed, cut to 32 characters (and any trailing `-` removed
/// again). `mac` if nothing is left.
pub fn default_name(hostname: &str) -> HostId {
    let mut name = String::new();
    let mut invalid = false;
    for c in hostname.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            name.push(c);
            invalid = false;
        } else if !invalid {
            name.push('-');
            invalid = true;
        }
    }
    let name = name.trim_matches('-');
    let name = &name[..name.len().min(32)];
    let name = name.trim_end_matches('-');
    HostId::new(if name.is_empty() { "mac" } else { name })
        .expect("normalized hostname is a valid HostId")
}

/// `text` (a `service.toml`) with its top-level `host` set to `host` and, if
/// given, `worker` set to `worker`. Only those lines change: comments, blank
/// lines, other keys and tables stay byte for byte. A key that is missing is
/// added before the first table header (or at the end if there is none).
/// Only top-level `key = value` lines count; a commented-out `# host = …`
/// or a `host` inside a table is left alone.
pub fn rewrite_config(text: &str, host: &HostId, worker: Option<&str>) -> String {
    let host_line = format!("host = \"{host}\"");
    let worker_line = worker.map(|url| {
        format!(
            "worker = \"{}\"",
            url.replace('\\', "\\\\").replace('"', "\\\"")
        )
    });
    let mut out = String::new();
    let mut found_host = false;
    let mut found_worker = false;
    let mut top_level = true;
    for line in text.split_inclusive('\n') {
        if top_level && line.trim().starts_with('[') {
            append_missing(&mut out, &host_line, &worker_line, found_host, found_worker);
            top_level = false;
        }
        let assignment = |key| {
            line.trim()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        };
        let replacement = if top_level && assignment("host") {
            found_host = true;
            Some(host_line.as_str())
        } else if top_level && assignment("worker") {
            found_worker = true;
            worker_line.as_deref()
        } else {
            None
        };
        if let Some(replacement) = replacement {
            out.push_str(replacement);
            if line.ends_with("\r\n") {
                out.push_str("\r\n");
            } else if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    if top_level {
        append_missing(&mut out, &host_line, &worker_line, found_host, found_worker);
    }
    out
}

/// A new `service.toml` for a machine that has none: the example config
/// (`service.example.toml`) with `host` and `worker` filled in.
pub fn new_config(host: &HostId, worker: &str) -> String {
    rewrite_config(include_str!("../service.example.toml"), host, Some(worker))
}

fn append_missing(
    out: &mut String,
    host: &str,
    worker: &Option<String>,
    found_host: bool,
    found_worker: bool,
) {
    for line in [
        (!found_host).then_some(host),
        worker.as_deref().filter(|_| !found_worker),
    ]
    .into_iter()
    .flatten()
    {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unrelated_bytes_and_escapes_worker() {
        let host = HostId::new("h1").unwrap();
        let text = "# host = old\r\nhost = \"old\"\r\nworker = \"https://old\"\r\n\r\n[table]\r\nhost = \"nested\"";
        let worker = "https://example.com/a\\b\"c";
        let out = rewrite_config(text, &host, Some(worker));
        assert_eq!(
            out,
            "# host = old\r\nhost = \"h1\"\r\nworker = \"https://example.com/a\\\\b\\\"c\"\r\n\r\n[table]\r\nhost = \"nested\""
        );
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(parsed["worker"].as_str(), Some(worker));
    }

    #[test]
    fn missing_keys_do_not_join_an_unterminated_line() {
        let host = HostId::new("h1").unwrap();
        assert_eq!(
            rewrite_config("# comment", &host, None),
            "# comment\nhost = \"h1\"\n"
        );
        assert_eq!(
            rewrite_config(
                "hostname = \"keep\"\n[table]\nhost = \"nested\"\n",
                &host,
                None
            ),
            "hostname = \"keep\"\nhost = \"h1\"\n[table]\nhost = \"nested\"\n"
        );
    }
}
