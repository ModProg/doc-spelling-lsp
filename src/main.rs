#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc, clippy::wildcard_imports)]

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env::{self};
use std::fs::File;
use std::process::{Child, Command};
use std::sync::{Arc, OnceLock};

use derive_more::{Display, FromStr};
use extend::ext;
use languagetool_rust::ServerClient;
use log::error;
use serde_json::Value;
use state::State;
use tokio::sync::{watch, Mutex};
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, DocumentChanges, InitializeResult, OneOf,
    OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, Url,
};
use tower_lsp::{async_trait, lsp_types, Client, LanguageServer, LspService, Server};

use self::diagnostic::diagnose;

mod config;
mod diagnostic;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log_file = env::var("RUST_LOG_FILE").map(|file| File::create(file).unwrap());
    env_logger::builder()
        .target(if let Ok(log_file) = log_file {
            env_logger::Target::Pipe(Box::new(log_file))
        } else {
            env_logger::Target::Stderr
        })
        .build();
    embedded_language_tool::handle_extraction();
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Lsp::new);
    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

#[derive(Debug)]
struct InitializedLsp {
    ltex_server: Mutex<Option<Child>>,
    documents: Arc<Mutex<HashMap<Url, String>>>,
    diagnose: watch::Sender<HashSet<Url>>,
    state: watch::Sender<state::State>,
}

struct Lsp {
    client: Client,
    initialized: OnceLock<InitializedLsp>,
}

impl Lsp {
    fn new(client: Client) -> Self {
        Self {
            client,
            initialized: OnceLock::new(),
        }
    }

    fn unwrap(&self) -> &InitializedLsp {
        self.initialized.get().expect("LSP not yet initialized")
    }
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

    fn invalid_params(self, message: impl Into<Cow<'static, str>>) -> Result<T> {
        self.ok_or_else(|| Error::invalid_params(message.into()))
    }
}

impl Lsp {
    fn publish_diagnostics(&self, uri: Url) {
        self.unwrap().diagnose.send_modify(|s| _ = s.insert(uri));
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

#[derive(Display, FromStr)]
enum WorkspaceCommand {
    AddToDictionary,
}

impl WorkspaceCommand {
    fn options() -> Vec<String> {
        vec![Self::AddToDictionary.to_string()]
    }
}

#[async_trait]
impl LanguageServer for Lsp {
    async fn initialize(&self, params: lsp_types::InitializeParams) -> Result<InitializeResult> {
        let config: config::Config = params
            .initialization_options
            .map(serde_json::from_value)
            .transpose()
            .invalid_params("error deserializing config:")?
            .unwrap_or_default();

        let (ltex_server, ltex_client) = match config.server {
            config::Server::Embedded { location, config } => {
                let location = &if let Some(location) = location.clone() {
                    location
                } else {
                    directories::BaseDirs::new()
                        .internal_error("unale to find data dir from environment")?
                        .data_dir()
                        .join("language")
                };
                let server_executable = match embedded_language_tool::extract(location) {
                    Ok(o) => o,
                    Err(e) => return Err(Error::internal_error().message(e.to_string())),
                };
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

        let documents: Arc<Mutex<HashMap<Url, String>>> = Arc::default();
        let (diagnose_sender, mut diagnose_recv) = watch::channel(HashSet::new());
        let (state_sender, state_recv) = watch::channel(State::default());
        state_sender.send(state::update(state_recv.clone(), &config.state)?).unwrap();

        {
            let documents = documents.clone();
            let mut document = String::new();
            let mut state = state_recv.borrow().clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                loop {
                    diagnose_recv
                        .changed()
                        .await
                        .expect("we should not drop the sender");
                    let tasks = diagnose_recv.borrow_and_update().clone();
                    for uri in tasks {
                        let documents = documents.lock().await;
                        documents
                            .get(&uri)
                            .expect("we should have just inserted it")
                            .clone_into(&mut document);
                        state_recv.borrow().clone_into(&mut state);
                        drop(documents);

                        match diagnose(&document, &ltex_client, &state).await {
                            Err(e) => error!("{e:?}"),
                            Ok(diags) => {
                                client.publish_diagnostics(uri, diags, None).await;
                            }
                        };
                    }
                }
            });
        }
        self.initialized
            .set(InitializedLsp {
                ltex_server: Mutex::new(ltex_server),
                documents,
                state: state_sender,
                diagnose: diagnose_sender,
            })
            .expect("initialize should only be called once");

        Ok(InitializeResult {
            capabilities: {
                use lsp_types::*;
                ServerCapabilities {
                    // TODO: support partial updates
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::FULL,
                    )),
                    code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                    execute_command_provider: Some(ExecuteCommandOptions {
                        commands: WorkspaceCommand::options(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            },
            server_info: None,
        })
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some(mut ltex_server) = self.unwrap().ltex_server.lock().await.take() {
            _ = ltex_server.kill();
        }
        Ok(())
    }

    async fn did_open(&self, params: lsp_types::DidOpenTextDocumentParams) {
        let mut documents = self.unwrap().documents.lock().await;
        documents.insert(params.text_document.uri.clone(), params.text_document.text);
        drop(documents);
        self.publish_diagnostics(params.text_document.uri);
    }

    async fn did_save(&self, params: lsp_types::DidSaveTextDocumentParams) {
        self.publish_diagnostics(params.text_document.uri);
    }

    async fn did_change(&self, mut params: lsp_types::DidChangeTextDocumentParams) {
        // TODO verify this is full document
        let mut documents = self.unwrap().documents.lock().await;
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
                .filter_map(move |diagnostic| {
                    let meta: diagnostic::Meta =
                        serde_json::from_value(diagnostic.data.as_ref()?.clone()).ok()?;
                    Some(
                        meta.replacements
                            .into_iter()
                            .map({
                                let uri = uri.clone();
                                move |value| {
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
                            .chain(meta.missspelled.map(|word| {
                                lsp_types::CodeActionOrCommand::Command(lsp_types::Command {
                                    title: format!("Add `{word}` to dictionary"),
                                    command: WorkspaceCommand::AddToDictionary.to_string(),
                                    arguments: Some(vec![
                                        serde_json::to_value(word)
                                            .expect("string can be serialized"),
                                    ]),
                                })
                            })),
                    )
                })
                .flatten()
                .collect(),
        ))
    }

    async fn execute_command(
        &self,
        mut params: lsp_types::ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        match WorkspaceCommand::from_str(&params.command) {
            Ok(WorkspaceCommand::AddToDictionary) => {
                let word: String = serde_json::from_value(
                    params
                        .arguments
                        .pop()
                        .invalid_params("AddToDictionary requires argument")?,
                )
                .invalid_params("AddToDictionary expects string argument")?;
                self.unwrap()
                    .state
                    .send_if_modified(|state| state.dictionary.insert(word));
                self.unwrap().diagnose.send_modify(|_| {});
            }
            Err(_) => {
                return Err(Error::invalid_request()
                    .message(format!("unkown workspace command: `{}`", params.command)));
            }
        };
        Ok(None)
    }
}
