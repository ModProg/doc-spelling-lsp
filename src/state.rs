use std::collections::HashSet;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct State {
    pub disabled_rules: HashSet<String>,
    pub dictionary: HashSet<String>,
}
