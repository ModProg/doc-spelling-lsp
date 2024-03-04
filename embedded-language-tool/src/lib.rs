use std::env::current_exe;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::{env, io};

use anyhow::Context;
use intentional::Assert;
use thiserror::Error;
use zip::ZipArchive;

// TODO use buildscript to extract some information from zip

#[inline(never)]
pub fn language_tool_binary() -> &'static [u8] {
    include_bytes!("../LanguageTool-stable.zip")
}

const ONLY_EXTRACT: &str = "LTEX_LSP_RUST_EXTRACT_IN_THIS_PROCESS";

struct ServerBinary(ZipArchive<Cursor<&'static [u8]>>);

impl ServerBinary {
    fn new() -> Self {
        Self(
            ZipArchive::new(Cursor::new(language_tool_binary()))
                .assert("embedded zip file should be valid"),
        )
    }

    fn extract(mut self, dir: impl AsRef<Path>) -> anyhow::Result<()> {
        if self.already_extracted(&dir) {
            Ok(())
        } else {
            self.0.extract(&dir).with_context(|| {
                format!("extracting server binary at {}", dir.as_ref().display())
            })?;
            Ok(())
        }
    }

    fn root_dir(&self) -> &str {
        // get the path of the root dir, this could also be solved by having said root
        // dir name as a string constant to avoid need to load the file.
        self.0
            .file_names()
            .next()
            .map(std::path::Path::new)
            .assert("embedded server should contain files")
            .components()
            .next()
            .assert("files in server should have a root component")
            .as_os_str()
            .to_str()
            .assert("paths in embedded server should be valid utf8")
    }

    fn already_extracted(&self, dir: impl AsRef<Path>) -> bool {
        dir.as_ref().join(self.root_dir()).exists()
    }

    fn executabe_path(&self, dir: impl AsRef<Path>) -> PathBuf {
        dir.as_ref()
            .join(self.root_dir())
            .join("languagetool-server.jar")
    }
}

fn already_extracted(dir: impl AsRef<Path>) -> bool {
    let zip_file = ZipArchive::new(Cursor::new(language_tool_binary()))
        .assert("embedded zip file should be valid");
    // get the path of the root dir, this could also be solved by having said root
    // dir name as a string constant to avoid need to load the file.
    let root = zip_file
        .file_names()
        .next()
        .map(Path::new)
        .assert("embedded server should contain files")
        .components()
        .next()
        .assert("files in server should have a root component");
    dir.as_ref().join(root).exists()
}

pub fn handle_extraction() {
    if let Ok(path) = env::var(ONLY_EXTRACT) {
        if let Err(e) = ServerBinary::new().extract(path) {
            eprintln!("{e:?}");
        };
        exit(0);
    }
}

#[derive(Error, Debug)]
pub enum ExtractionError {
    #[error("getting current executable:\n{0}")]
    GettingCurrentExecutable(io::Error),
    #[error("running embedded extraction:\n{0}")]
    RunningExecutable(io::Error),
    #[error("did not successfully extract embedded server:\n{0}")]
    ErrorExtracting(String),
}

pub fn extract(location: &Path) -> Result<PathBuf, ExtractionError> {
    let server_binary = ServerBinary::new();
    if !already_extracted(location) {
        let command =
            Command::new(current_exe().map_err(ExtractionError::GettingCurrentExecutable)?)
                .env(ONLY_EXTRACT, location)
                .output()
                .map_err(ExtractionError::RunningExecutable)?;
        if !command.status.success() {
            return Err(ExtractionError::ErrorExtracting(String::from_utf8_lossy(&command.stderr).to_string()))
        }
    }
    Ok(server_binary.executabe_path(location))
}
