use std::fmt;

use schemars::JsonSchema;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize};

/// Configure Jest plugin rules.
///
/// See [eslint-plugin-jest](https://github.com/jest-community/eslint-plugin-jest)'s
/// configuration for a full reference.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct JestPluginSettings {
    /// Jest version — accepts a number (`29`) or a semver string (`"29.1.0"` or `"v29.1.0"`),
    /// storing only the major version. Default version is 29
    #[serde(default = "default_jest_version", deserialize_with = "jest_version_deserialize")]
    pub version: usize,
}

fn jest_version_deserialize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    struct VersionVisitor;

    impl Visitor<'_> for VersionVisitor {
        type Value = usize;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("Expecter Jest version as a number or string")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            usize::try_from(v)
                .map_err(|_| E::custom(format!("Invalid Jest version integer: {v:?}")))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v < 0 {
                return Err(E::custom(format!("Jest version cannot be negative: {v:?}")));
            }

            usize::try_from(v)
                .map_err(|_| E::custom(format!("Invalid Jest version integer: {v:?}")))
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let skip_v_prefix = usize::from(v.starts_with('v'));

            v.split('v')
                .nth(skip_v_prefix)
                .and_then(|semver| semver.split('.').next())
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or_else(|| E::custom(format!("Invalid Jest version string: {v:?}")))
        }
    }

    deserializer.deserialize_any(VersionVisitor)
}

fn default_jest_version() -> usize {
    29
}

// `default = "fn"` at field level doesn't work, likely on how ancestors deserialization is being done.
// This is likely affecting serde to know if the field is truly non present to use `default = "fn"`.
impl Default for JestPluginSettings {
    fn default() -> Self {
        Self { version: 29 }
    }
}
