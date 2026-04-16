use std::fmt;

use schemars::JsonSchema;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize};

/// Configure Jest plugin rules.
///
/// See [eslint-plugin-jest](https://github.com/jest-community/eslint-plugin-jest)'s
/// configuration for a full reference.
#[derive(Debug, Clone, Deserialize, Serialize, Default, JsonSchema, PartialEq, Eq)]
pub struct JestPluginSettings {
    /// Jest version — accepts a number (`29`) or a semver string (`"29.1.0"` or `"v29.1.0"`),
    /// storing only the major version.
    #[serde(default)]
    pub version: JestVersion,
}

#[derive(Debug, Clone, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JestVersion(pub usize);

impl<'de> Deserialize<'de> for JestVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionVisitor;

        impl Visitor<'_> for VersionVisitor {
            type Value = JestVersion;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("Expecter Jest version as a number or string")
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JestVersion(
                    usize::try_from(v)
                        .map_err(|_| E::custom(format!("Invalid Jest version integer: {v:?}")))?,
                ))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v < 0 {
                    return Err(E::custom(format!("Jest version cannot be negative: {v:?}")));
                }

                Ok(JestVersion(
                    usize::try_from(v)
                        .map_err(|_| E::custom(format!("Invalid Jest version integer: {v:?}")))?,
                ))
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
                    .map(JestVersion)
                    .ok_or_else(|| E::custom(format!("Invalid Jest version string: {v:?}")))
            }
        }

        deserializer.deserialize_any(VersionVisitor)
    }
}

impl Default for JestVersion {
    fn default() -> Self {
        Self(29)
    }
}

impl std::ops::Deref for JestVersion {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
