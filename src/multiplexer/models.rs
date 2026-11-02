//! Backend identifiers are validated once, before they can become command targets.
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::fmt;

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn herdr_id(value: &str, kind: char) -> bool {
    let Some((workspace, local)) = value.split_once(':') else {
        return false;
    };
    workspace.strip_prefix('w').is_some_and(digits) && local.strip_prefix(kind).is_some_and(digits)
}
macro_rules! string_id {
    ($name:ident, $description:literal, $valid:expr) => {
        #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
        #[serde(try_from = "String")]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = anyhow::Error;
            fn try_from(value: String) -> Result<Self> {
                ensure!(($valid)(&value), concat!("invalid ", $description));
                Ok(Self(value))
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}
string_id!(HerdrPaneId, "Herdr pane ID", |value: &str| herdr_id(
    value, 'p'
));
string_id!(HerdrTabId, "Herdr tab ID", |value: &str| herdr_id(
    value, 't'
));
string_id!(HerdrWorkspaceId, "Herdr workspace ID", |value: &str| value
    .strip_prefix('w')
    .is_some_and(digits));
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchTarget {
    HerdrPane(HerdrPaneId),
}
impl fmt::Display for LaunchTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HerdrPane(id) => id.fmt(formatter),
        }
    }
}
#[derive(Debug)]
pub struct LaunchResponse {
    pub target: LaunchTarget,
}
