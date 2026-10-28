//! Validated names. Validation happens on deserialize, so a bad name never
//! reaches tmux or a Durable Object id.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("name must be 1 to 32 characters")]
    Length,
    #[error("name contains {0:?}, which is not allowed")]
    Char(char),
}

/// Session identifier: a tmux name, or `herdr:` followed by a native name.
/// Native names contain 1–32 ASCII letters, digits, underscores or hyphens.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct SessionName(String);

/// A host machine id, also the host room's name: `^[a-z0-9-]{1,32}$`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct HostId(String);

fn check(s: &str, allowed: impl Fn(char) -> bool) -> Result<(), NameError> {
    if s.is_empty() || s.len() > 32 {
        return Err(NameError::Length);
    }
    match s.chars().find(|c| !allowed(*c)) {
        Some(c) => Err(NameError::Char(c)),
        None => Ok(()),
    }
}

macro_rules! name_type {
    ($ty:ident, $allowed:expr) => {
        impl $ty {
            pub fn new(s: impl Into<String>) -> Result<Self, NameError> {
                let s = s.into();
                check($ty::validation_name(&s), $allowed)?;
                Ok(Self(s))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $ty {
            type Error = NameError;
            fn try_from(s: String) -> Result<Self, NameError> {
                Self::new(s)
            }
        }

        impl From<$ty> for String {
            fn from(n: $ty) -> String {
                n.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

impl SessionName {
    fn validation_name(s: &str) -> &str {
        s.strip_prefix("herdr:").unwrap_or(s)
    }
    pub fn backend(&self) -> SessionBackend {
        if self.0.starts_with("herdr:") {
            SessionBackend::Herdr
        } else {
            SessionBackend::Tmux
        }
    }
    pub fn native_name(&self) -> &str {
        Self::validation_name(&self.0)
    }
    pub fn for_backend(
        name: impl Into<String>,
        backend: SessionBackend,
    ) -> Result<Self, NameError> {
        let name = name.into();
        // Validate native input before adding a namespace; reject nested prefixes.
        check(&name, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-')?;
        Self::new(match backend {
            SessionBackend::Tmux => name,
            SessionBackend::Herdr => format!("herdr:{name}"),
        })
    }
}

impl HostId {
    fn validation_name(s: &str) -> &str {
        s
    }
}

/// Backend selected by a service preset. Omitted values retain tmux behavior.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionBackend {
    #[default]
    Tmux,
    Herdr,
}
impl SessionBackend {
    pub fn is_tmux(&self) -> bool {
        *self == Self::Tmux
    }
}

name_type!(SessionName, |c: char| c.is_ascii_alphanumeric()
    || c == '_'
    || c == '-');
name_type!(HostId, |c: char| c.is_ascii_lowercase()
    || c.is_ascii_digit()
    || c == '-');

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_are_distinct_and_validated() {
        let h = SessionName::for_backend("work", SessionBackend::Herdr).unwrap();
        assert_eq!(h.as_str(), "herdr:work");
        assert_eq!(h.native_name(), "work");
        assert_eq!(h.backend(), SessionBackend::Herdr);
        assert_ne!(h, SessionName::new("work").unwrap());
        for bad in ["herdr:", "herdr:../work", "herdr:herdr:work", "other:work"] {
            assert!(SessionName::new(bad).is_err());
        }
        assert!(SessionName::new(format!("herdr:{}", "a".repeat(32))).is_ok());
        assert!(SessionName::new(format!("herdr:{}", "a".repeat(33))).is_err());
        assert!(HostId::new("herdr:work").is_err());
    }

    #[test]
    fn session_names() {
        assert!(SessionName::new("claude").is_ok());
        assert!(SessionName::new("build_2-x").is_ok());
        assert_eq!(SessionName::new(""), Err(NameError::Length));
        assert_eq!(SessionName::new("a".repeat(33)), Err(NameError::Length));
        assert_eq!(SessionName::new("a;rm"), Err(NameError::Char(';')));
        assert_eq!(SessionName::new("=main"), Err(NameError::Char('=')));
        assert_eq!(SessionName::new("dé"), Err(NameError::Char('é')));
    }

    #[test]
    fn host_ids() {
        assert!(HostId::new("my-mac").is_ok());
        assert_eq!(HostId::new("My-Mac"), Err(NameError::Char('M')));
    }

    #[test]
    fn rejected_on_deserialize() {
        assert!(serde_json::from_str::<SessionName>(r#""ok""#).is_ok());
        assert!(serde_json::from_str::<SessionName>(r#""no spaces""#).is_err());
    }
}
