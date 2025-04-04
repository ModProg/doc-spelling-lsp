use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;

#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub state: State,
    pub parsing: crate::parsing::Config,
}

#[derive(Serialize, Deserialize, SmartDefault, Debug, Clone)]
#[serde(tag = "type")]
pub enum Server {
    #[default]
    Embedded {
        /// Location to put embedded server.
        ///
        /// Default is:
        ///
        /// | Platform | Value                                                                      |
        /// | -------- | -------------------------------------------------------------------------- |
        /// | Linux    | `$XDG_DATA_HOME/doc-spelling-lsp` or `$HOME/.local/share/doc-spelling-lsp` |
        /// | macOS    | `$HOME/Library/Application Support/doc-spelling-lsp`                       |
        /// | Windows  | `{FOLDERID_RoamingAppData}\doc-spelling-lsp`                               |
        location: Option<PathBuf>,
        // TODO move in shared struct
        /// Port to host local server.
        ///
        /// Default is a random free port.
        port: Option<u16>,
        /// Extra arguments for invoking local server.
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Online {
        // TODO
    },
    Local {
        #[serde(default = "default_executable")]
        executable: String,
        // TODO move in shared struct
        /// Port to host local server.
        ///
        /// Default is a random free port.
        port: Option<u16>,
        /// Extra arguments for invoking local server.
        #[serde(default)]
        extra_args: Vec<String>,
    },
}

fn default_executable() -> String {
    "languagetool".into()
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct State {
    /// Location to put state, i.e., false positives, disabled rules
    /// and dictionary.
    ///
    /// Default is:
    ///
    /// | Platform | Value                                                                                       |
    /// | -------- | ------------------------------------------------------------------------------------------- |
    /// | Linux    | `$XDG_CONFIG_HOME/doc-spelling-ls/state.json` or `$HOME/.config/doc-spelling-ls/state.json` |
    /// | macOS    | `$HOME/Library/Application Support/doc-spelling-ls/state.json`                              |
    /// | Windows  | `{FOLDERID_RoamingAppData}\doc-spelling-ls/sate.json`                                       |
    pub location: Option<PathBuf>,
}
