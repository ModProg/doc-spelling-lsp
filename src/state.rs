use std::collections::{BTreeSet, HashSet};
use std::fs;

use log::error;
use serde::{Deserialize, Serialize};

use crate::lsp::Context;
use crate::{Result, config};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
#[must_use]
pub struct State {
    pub disabled_rules: BTreeSet<String>,
    pub dictionary: HashSet<String>,
    pub language: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            disabled_rules: BTreeSet::new(),
            dictionary: HashSet::new(),
            language: "en-US".into(),
        }
    }
}

pub fn update(
    mut state: tokio::sync::watch::Receiver<State>,
    state_config: &config::State,
) -> Result<State> {
    let state_location = if let Some(location) = state_config.location.clone() {
        if location.is_dir() {
            location.join("state.json")
        } else {
            location
        }
    } else {
        let state_location = directories::BaseDirs::new()
            .expect("should be able to find home directory")
            .config_dir()
            .join("doc-spelling-lsp");
        fs::create_dir_all(&state_location).internal_error("unable to create state location")?;
        let state_location = state_location.join("state.json");
        if !state_location.exists() {
            fs::write(
                &state_location,
                serde_json::to_string(&State::default()).expect("state can be serialized"),
            )
            .internal_error(format!(
                "unable to write state at `{}`",
                state_location.display()
            ))?;
        }
        state_location
    };
    {
        let state_location = state_location.clone();
        // update state on disk
        tokio::spawn(async move {
            loop {
                if state.changed().await.is_err() {
                    break;
                }
                if let Err(e) = fs::write(
                    &state_location,
                    serde_json::to_string(&state.borrow().clone())
                        .expect("state should be serializable"),
                ) {
                    error!("{e:?}");
                };
            }
        });
    }
    serde_json::from_slice(&fs::read(&state_location).internal_error(format!(
        "unable to read from state location: `{}`",
        state_location.display()
    ))?)
    .internal_error("unable to deserialize state")
}
