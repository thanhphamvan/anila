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

/// A tmux session name: `^[A-Za-z0-9_-]{1,32}$`.
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
                check(&s, $allowed)?;
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
