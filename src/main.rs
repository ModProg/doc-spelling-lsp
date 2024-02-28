#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc, clippy::wildcard_imports)]

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env::{self, current_exe};
use std::fmt::Display;
use std::fs::File;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::Context;
use extend::ext;
use intentional::Assert;
use languagetool_rust::check::Replacement;
use languagetool_rust::ServerClient;
use state::State;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, DocumentChanges, InitializeResult, OneOf,
    OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, Url,
};
use tower_lsp::{async_trait, lsp_types, Client, LanguageServer, LspService, Server};
use tracing::{error, instrument};
use zip::ZipArchive;

use self::diagnostic::diagnose;

mod config;
mod diagnostic;
mod state;

const ONLY_EXTRACT: &str = "LTEX_LSP_RUST_EXTRACT_IN_THIS_PROCESS";
// NOTE: This is a Cow, because rustc runs out of memory otherwise.
// TODO: Report rustc issue, seems to only be reproducable with
// `ZipArchive::new`.
const SERVER_BINARY: Cow<'_, [u8]> = Cow::Borrowed(include_bytes!("../LanguageTool-stable.zip"));

struct ServerBinary(ZipArchive<Cursor<Cow<'static, [u8]>>>);

impl ServerBinary {
    fn new() -> Self {
        Self(
            ZipArchive::new(Cursor::new(SERVER_BINARY)).assert("embedded zip file should be valid"),
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
            .map(Path::new)
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
    let zip_file =
        ZipArchive::new(Cursor::new(SERVER_BINARY)).assert("embedded zip file should be valid");
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log_file = env::var("LOG_FILE").map(|file| File::create(file).unwrap());
    let builder = tracing_subscriber::fmt();
    // .with_span_events(FmtSpan::ENTER)
    // .with_span_events(FmtSpan::EXIT);
    if let Ok(log_file) = log_file {
        builder.with_writer(log_file).init();
    } else {
        builder.with_writer(io::stderr).init();
    }
    if let Ok(path) = env::var(ONLY_EXTRACT) {
        ServerBinary::new().extract(path)?;
    } else {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();

        let (service, socket) = LspService::new(Lsp::new);
        Server::new(stdin, stdout, socket).serve(service).await;
    }
    Ok(())
}

#[derive(Debug)]
struct Lsp {
    client: Client,
    ltex_server: Arc<Mutex<Option<Child>>>,
    ltex_client: Arc<Mutex<Option<languagetool_rust::ServerClient>>>,
    documents: Arc<Mutex<HashMap<Url, String>>>,
    diagnose: tokio::sync::watch::Sender<HashSet<Url>>,
    state: tokio::sync::watch::Sender<state::State>,
}

impl Lsp {
    fn new(client: Client) -> Self {
        let ltex_server: Arc<Mutex<Option<Child>>> = Arc::default();
        let ltex_client: Arc<Mutex<Option<languagetool_rust::ServerClient>>> = Arc::default();
        let documents: Arc<Mutex<HashMap<Url, String>>> = Arc::default();
        let (diagnose_sender, mut diagnose_recv) = tokio::sync::watch::channel(HashSet::new());
        let (state_sender, mut state_recv) = tokio::sync::watch::channel(State::default());
        {
            let ltex_client = ltex_client.clone();
            let documents = documents.clone();
            let client = client.clone();
            tokio::spawn(async move {
                loop {
                    diagnose_recv
                        .changed()
                        .await
                        .expect("we should not drop the sender");
                    let tasks = diagnose_recv.borrow_and_update().clone();
                    for uri in tasks {
                        let documents = documents.lock().await;
                        let document = documents
                            .get(&uri)
                            .expect("we should have just inserted it")
                            .clone();
                        drop(documents);

                        let ltex_client = ltex_client.lock().await;
                        let ltex_client = ltex_client.as_ref().expect("was initialized");
                        match diagnose(&document, ltex_client).await {
                            Err(e) => error!("{e:?}"),
                            Ok(diags) => {
                                client.publish_diagnostics(uri, diags, None).await;
                            }
                        };
                    }
                }
            });
        }
        Self {
            client,
            ltex_server,
            ltex_client,
            documents,
            state: state_sender,
            diagnose: diagnose_sender,
        }
    }
}

macro_rules! ensure {
    ($cond:expr, $error:expr) => {
        if !$cond {
            return Err($error);
        }
    };
}

#[ext]
impl<T, E: Display> std::result::Result<T, E> {
    fn internal_error(self, context: impl Display) -> Result<T> {
        self.map_err(|e| Error::internal_error().message(format!("{context}: {e}")))
    }

    fn invalid_params(self, context: impl Display) -> Result<T> {
        self.map_err(|e| Error::invalid_params(format!("{context}: {e}")))
    }
}

#[ext]
impl<T> Option<T> {
    fn internal_error(self, message: impl Into<Cow<'static, str>>) -> Result<T> {
        self.ok_or_else(|| Error::internal_error().message(message.into()))
    }

    fn invalid_request(self, message: impl Into<Cow<'static, str>>) -> Result<T> {
        self.ok_or_else(|| Error::invalid_request().message(message.into()))
    }
}

impl Lsp {
    fn publish_diagnostics(&self, uri: Url) {
        self.diagnose.send_modify(|s| _ = s.insert(uri));
    }
}

fn run_server(
    command: &mut Command,
    config::LocalServer { port, extra_args }: config::LocalServer,
) -> Result<(Option<Child>, ServerClient)> {
    let port = port
        .or_else(portpicker::pick_unused_port)
        .internal_error("unable to find unused port")?
        .to_string();
    let program = command.get_program().to_string_lossy().to_string();
    Ok((
        Some(
            command
                .arg("--port")
                .arg(&port)
                .args(extra_args)
                .spawn()
                .internal_error(format!("spawning language tool server `{program}`"))?,
        ),
        languagetool_rust::ServerClient::new("http://localhost", &port),
    ))
}

#[async_trait]
impl LanguageServer for Lsp {
    #[instrument]
    async fn initialize(&self, params: lsp_types::InitializeParams) -> Result<InitializeResult> {
        let init_options: config::Config = params
            .initialization_options
            .map(serde_json::from_value)
            .transpose()
            .invalid_params("error deserializing config:")?
            .unwrap_or_default();
        let (ltex_server, ltex_client) = match init_options.server {
            config::Server::Embedded { location, config } => {
                let location = &if let Some(location) = location.clone() {
                    location
                } else {
                    directories::BaseDirs::new()
                        .internal_error("unale to find data dir from environment")?
                        .data_dir()
                        .join("language")
                };
                let server_binary = ServerBinary::new();
                if !already_extracted(location) {
                    ensure!(
                        Command::new(current_exe().internal_error("getting current executable")?)
                            .env(ONLY_EXTRACT, location)
                            .status()
                            .internal_error("running embedded extraction")?
                            .success(),
                        Error::internal_error()
                            .message("did not successfully extract embedded server")
                    );
                }
                let server_executable = server_binary.executabe_path(location);
                run_server(
                    Command::new("java")
                        .arg("-cp")
                        .arg(&server_executable)
                        .arg("org.languagetool.server.HTTPServer"),
                    config,
                )?
            }
            config::Server::Online {} => todo!(),
            config::Server::Local { .. } => todo!(),
        };
        *self.ltex_server.lock().await = ltex_server;
        *self.ltex_client.lock().await = Some(ltex_client);
        Ok(InitializeResult {
            capabilities: {
                use lsp_types::*;
                ServerCapabilities {
                    // TODO: support partial updates
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::FULL,
                    )),
                    code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                    ..Default::default()
                }
            },
            server_info: None,
        })
    }

    #[instrument]
    async fn shutdown(&self) -> Result<()> {
        _ = self.ltex_server.lock().await.take().unwrap().kill();
        Ok(())
    }

    #[instrument]
    async fn did_open(&self, params: lsp_types::DidOpenTextDocumentParams) {
        let mut documents = self.documents.lock().await;
        documents.insert(params.text_document.uri.clone(), params.text_document.text);
        drop(documents);
        self.publish_diagnostics(params.text_document.uri);
    }

    #[instrument]
    async fn did_save(&self, params: lsp_types::DidSaveTextDocumentParams) {
        self.publish_diagnostics(params.text_document.uri);
    }

    #[instrument]
    async fn did_change(&self, mut params: lsp_types::DidChangeTextDocumentParams) {
        // TODO verify this is full document
        let mut documents = self.documents.lock().await;
        documents.insert(
            params.text_document.uri.clone(),
            params.content_changes.pop().unwrap().text,
        );
        drop(documents);
        self.publish_diagnostics(params.text_document.uri);
    }

    async fn code_action(
        &self,
        params: lsp_types::CodeActionParams,
    ) -> Result<Option<Vec<lsp_types::CodeActionOrCommand>>> {
        let uri = params.text_document.uri;
        Ok(Some(
            params
                .context
                .diagnostics
                .into_iter()
                .flat_map(move |diagnostic| {
                    let replacements: Vec<Replacement> =
                        serde_json::from_value(diagnostic.data.as_ref().unwrap().clone()).unwrap();
                    replacements.into_iter().map({
                        let uri = uri.clone();
                        move |Replacement { value, .. }| {
                            CodeActionOrCommand::CodeAction(CodeAction {
                                title: format!("replace with `{value}`"),
                                kind: Some(CodeActionKind::QUICKFIX),
                                edit: Some(lsp_types::WorkspaceEdit {
                                    changes: None,
                                    document_changes: Some(DocumentChanges::Edits(vec![
                                        TextDocumentEdit {
                                            text_document:
                                                OptionalVersionedTextDocumentIdentifier {
                                                    uri: uri.clone(),
                                                    version: None,
                                                },
                                            edits: vec![OneOf::Left(lsp_types::TextEdit {
                                                range: diagnostic.range,
                                                new_text: value,
                                            })],
                                        },
                                    ])),
                                    ..Default::default()
                                }),
                                diagnostics: Some(vec![diagnostic.clone()]),
                                ..Default::default()
                            })
                        }
                    })
                })
                .collect(),
        ))
    }
}
