use std::collections::HashMap;
use std::path::PathBuf;

use crowd::visit_map;
use derive_more::FromStr;
use serde::de::value::MapAccessDeserializer;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// List of directories to look for compiled tree-sitter grammars, i.e.
    /// `.so` or `.dll` files.
    ///
    /// Examples paths:
    /// - `/usr/lib/helix/runtime/grammars`, the grammars shipped with helix
    /// - `/usr/lib/tree_sitter`, e.g. Arch Linux installs grammars here.
    pub grammars: Vec<PathBuf>,
    #[serde(flatten)]
    pub languages: HashMap<String, Language>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct Language {
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub r#type: NodeType,
    pub query: String,
    #[serde(deserialize_with = "deserialize_transform_map", default)]
    pub transform: HashMap<String, Vec<Transform>>,
}

fn deserialize_transform_map<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<String, Vec<Transform>>, D::Error> {
    deserializer.deserialize_map(visit_map!(|mut a| -> HashMap<String, Vec<Transform>> {
        let mut map = HashMap::with_capacity(a.size_hint().unwrap_or(0));
        while let Some(key) = a.next_key()? {
            let value = crowd::deserialize_with_visit!(a.next_value()?, Vec<Transform>, "list, map or string",
                seq(mut a) {
                    let mut list = Vec::new();
                    while let Some(value) = crowd::deserialize_option_with_visit!(a.next_element()?,
                        Transform, "map or string",
                        str(v) {
                            v.parse().map_err(serde::de::Error::custom)
                        },
                        map(a) {
                            Transform::deserialize(MapAccessDeserializer::new(a))
                        },
                    ) {
                        list.push(value);
                    }
                    Ok(list)
                },
                str(v) {
                    Ok(vec![v.parse().map_err(serde::de::Error::custom)?])
                },
                map(a) {
                    Ok(vec![Transform::deserialize(MapAccessDeserializer::new(a))?])
                },
            );
            map.insert(key, value);
        }
        Ok(map)
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum NodeType {
    Text,
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Transform {
    pub regex: String,
    pub replace: String,
    pub flags: Vec<Flag>,
}

impl FromStr for Transform {
    type Err = String;

    fn from_str(original: &str) -> Result<Self, Self::Err> {
        if !original.starts_with('/') {
            return Err(format!(
                "invalid transform, does not start with `/`: `{original}`"
            ));
        }
        let s = &original[1..];
        let mut regex = String::new();
        let mut replace = String::new();
        let mut flags = Vec::new();
        let mut escape = false;
        #[allow(clippy::items_after_statements)]
        #[derive(Copy, Clone)]
        enum State {
            Regex,
            Replace,
            Flags,
        }
        let mut state = State::Regex;
        let mut push = |c, state| {
            match state {
                State::Regex => regex.push(c),
                State::Replace => replace.push(c),
                State::Flags => flags.push(Flag::try_from(c)?),
            }
            Ok::<_, String>(())
        };
        for char in s.chars() {
            if escape {
                if char != '/' {
                    push('\\', state)?;
                }
                push(char, state)?;
                escape = false;
            } else if char == '\\' {
                escape = true;
            } else if char == '/' {
                match state {
                    State::Regex => state = State::Replace,
                    State::Replace => state = State::Flags,
                    State::Flags => {
                        return Err(format!(
                            "invalid transform, contains unexpected 4th `/` (to escape use \
                             `\\/`): `{s}`"
                        ));
                    }
                }
            } else {
                push(char, state)?;
            }
        }

        Ok(Self {
            regex,
            replace,
            flags,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Flag {
    Break,
    Multiline,
}

impl TryFrom<char> for Flag {
    type Error = String;

    fn try_from(value: char) -> Result<Self, Self::Error> {
        match value {
            'b' => Ok(Self::Break),
            'm' => Ok(Self::Multiline),
            other => Err(format!("`{other}` is not a valid flag. Valid flags: b, m")),
        }
    }
}
