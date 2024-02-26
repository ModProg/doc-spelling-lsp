use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Config {
    pub server: Server,
}

#[derive(Serialize, Deserialize, SmartDefault, Debug, Clone)]
#[serde(tag = "type")]
pub enum Server {
    #[default]
    Embedded {
        /// Optional location to put embedded server.
        ///
        /// Default is:
        ///
        /// | Platform | Value                                                                        |
        /// | -------- | ---------------------------------------------------------------------------- |
        /// | Linux    | `$XDG_DATA_HOME`/language-tool-lsp or `$HOME`/.local/share/language-tool-lsp |
        /// | macOS    | `$HOME`/Library/Application Support/language-tool-lsp                        |
        /// | Windows  | `{FOLDERID_RoamingAppData}`\language-tool-lsp                                |
        location: Option<PathBuf>,
        port: Option<u16>,
    },
    Online {
        // TODO
    },
    Local {
        // TODO
    },
}
