#![allow(unused)]
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::task::Poll;
use std::thread;

// TODO remove anyhow from a lib maybe :D
use anyhow::{bail, Context as _};
use crossbeam_channel::{Sender, TryRecvError};
use derive_more::Display;
use extend::ext;
use forr::forr;
use futures::future::BoxFuture;
use futures::{stream, FutureExt, SinkExt, StreamExt};
use log::{error, info, warn};
use lsp_server::{Connection, IoThreads, Message, RequestId, Response, ResponseError};
use lsp_types::notification::{DidChangeTextDocument, Notification, PublishDiagnostics};
use lsp_types::request::Request;
use lsp_types::{Diagnostic, InitializeParams, PublishDiagnosticsParams, ServerCapabilities, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::{JoinHandle, JoinSet};

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("no fallible serialization used")
}

fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    Ok(serde_json::from_value(value)?)
}

pub struct Builder<Options = ()> {
    connection: Connection,
    threads: IoThreads,
    server_capabilities: ServerCapabilities,
    options: Options,
}

impl Builder {
    pub fn stdio() -> Self {
        let (connection, threads) = Connection::stdio();

        Self {
            connection,
            threads,
            server_capabilities: ServerCapabilities::default(),
            options: (),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // JSON-RPC
    /// Invalid JSON was received by the server.
    ///
    /// An error occurred on the server while parsing the JSON text.
    ParseError = -32700,
    /// The JSON sent is not a valid Request object.
    InvalidRequest = -32600,
    /// The method does not exist / is not available.
    MethodNotFound = -32601,
    /// Invalid method parameter(s).
    InvalidParams = -32602,
    /// Internal JSON-RPC error.
    InternalError = -32603,

    // LSP
    /// Error code indicating that a server received a notification or
    /// request before the server has received the `initialize` request.
    ServerNotInitialized = -32002,
    #[allow(clippy::enum_variant_names)]
    UnknownErrorCode = -32001,
    /// A request failed, but it was syntactically correct, e.g. the
    /// method name was known, and the parameters were valid. The error
    /// message should contain human-readable information about why
    /// the request failed.
    RequestFailed = -32803,
    /// The server cancelled the request. This error code should
    /// only be used for requests that explicitly support being
    /// server cancellable.
    ServerCancelled = -32802,
    /// The server detected that the content of a document got
    /// modified outside normal conditions. A server should
    /// NOT send this error code if it detects a content change
    /// in it unprocessed messages. The result even computed
    /// on an older state might still be useful for the client.
    ///
    /// If a client decides that a result is not of any use anymore
    /// the client should cancel the request.
    ContentModified = -32801,
    /// The client has canceled a request and a server as detected
    /// the cancel.
    RequestCancelled = -32800,
}

#[derive(Debug)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub data: Option<Value>,
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            code,
            message,
            data,
        } = self;
        write!(f, "{code:?}({}): {message}", *code as i32);
        if f.alternate() {
            if let Some(data) = data {
                writeln!(
                    f,
                    "\n{}",
                    serde_json::to_string_pretty(data)
                        .expect("serde_json::Value can be serialized to json")
                );
            }
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<Error> for ResponseError {
    fn from(error: Error) -> ResponseError {
        let Error {
            code,
            message,
            data,
        } = error;
        ResponseError {
            code: code as i32,
            message,
            data,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid_params(format!(
            "{error}\nset environment variable `RUST_LOG=lsp_server=debug` to log messages."
        ))
    }
}

forr::forr! { casing($name:s, $variant:C) in [
    parse error, invalid request, method not found, invalid params, internal error,
    server not initialized, unknown error code, request failed,
    server cancelled, content modified, request cancelled
] $:
    impl Error {
        pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
            Self {
                code,
                message: message.into(),
                data: None,
            }
        }

        $(pub fn $name(message: impl Into<String>) -> Self {
            Self::new(ErrorCode::$variant, message)
        })*
    }

    pub trait Context<T> {
        $(fn $name(self, message: impl Display) -> Result<T, Error>;)*
    }

    impl<T, E: Display> Context<T> for Result<T, E> {
        $(fn $name(self, message: impl Display) -> Result<T, Error> {
            self.map_err(|e| Error::$name(format!("{message}:\n{e}")))
        })*
    }

    impl<T> Context<T> for Option<T> {
        $(fn $name(self, message: impl Display) -> Result<T, Error> {
            self.ok_or_else(|| Error::$name(message.to_string()))
        })*
    }

    $(#[macro_export]
    macro_rules! $name {
        () => {
            $crate::lsp::Error::$name("")
        };
        ($($fmt:tt)*) => {
            $crate::lsp::Error::$name(format!($($fmt)*))
        };
    })*
}

impl<Options> Builder<Options> {
    pub fn server_capabilities(mut self, capabilties: ServerCapabilities) -> Self {
        self.server_capabilities = capabilties;
        self
    }

    // TODO
    #[allow(unused)]
    pub fn options<O>(self, options: O) -> Builder<O> {
        let Self {
            connection,
            threads,
            server_capabilities,
            ..
        } = self;

        Builder {
            connection,
            threads,
            server_capabilities,
            options,
        }
    }

    pub async fn launch<T: LanguageServer<Options>>(self) -> anyhow::Result<()> {
        let Self {
            connection,
            threads,
            server_capabilities,
            options,
        } = self;

        let params = connection.initialize(to_value(server_capabilities))?;
        let params = from_value(params).context("deserializing initialization parameters")?;

        let imp = T::initialize(
            params,
            Client {
                sender: connection.sender.clone(),
            },
            options,
        )
        .await?;
        let imp = Arc::new(imp);

        let c_receiver = connection.receiver.clone();
        let (c_sender, mut receiver) = unbounded_channel();
        thread::spawn(move || {
            loop {
                c_sender.send(c_receiver.recv().unwrap()).unwrap();
            }
        });
        let runner = {
            let sender = connection.sender.clone();
            let imp = imp.clone();
            tokio::spawn(async move {
                let mut notifications = JoinSet::<()>::new();
                // TODO request abortion
                // let requests = HashMap::<RequestId, JoinHandle<()>>::new();

                while let Some(message) = receiver.recv().await {
                    info!("got message");
                    let imp = imp.clone();
                    let sender = sender.clone();
                    match message {
                        Message::Request(request) => {
                            use lsp_types::request::*;
                            match request.method.as_str() {
                                Shutdown::METHOD => return Ok(request),
                                _ => notifications.spawn(async move {
                                    let (result, error) = imp
                                        .handle_request(request.method, request.params)
                                        .await
                                        .split();
                                    sender.send(Message::Response(Response {
                                        id: request.id,
                                        result,
                                        error: error.map(|e| lsp_server::ResponseError {
                                            code: 0,
                                            message: e.to_string(),
                                            data: None,
                                        }),
                                    }));
                                }),
                            };
                        }

                        Message::Response(_) => todo!(),
                        Message::Notification(notification) => {
                            notifications.spawn(async move {
                                imp.handle_notification(notification.method, notification.params)
                                    .await;
                            });
                        }
                    }
                }
                bail!("channel disconnected prematurely")
            })
        };

        let shutdown_req = runner.await??;
        Arc::try_unwrap(imp)
            .ok()
            .expect("all futures are completed or aborted")
            .shutdown()
            .await?;
        assert!(
            connection.handle_shutdown(&shutdown_req)?,
            "should only return on shutdown_req"
        );
        threads.join().context("joining io threads")?;
        Ok(())
    }
}

#[ext]
impl<T, E> Result<T, E> {
    fn split(self) -> (Option<T>, Option<E>) {
        match self {
            Ok(o) => (Some(o), None),
            Err(e) => (None, Some(e)),
        }
    }
}

#[derive(Clone)]
pub struct Client {
    sender: Sender<Message>,
}

impl Client {
    pub fn publish_diagnostics(&self, uri: Url, diagnostics: Vec<Diagnostic>) {
        self.send_notification::<PublishDiagnostics>(PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        });
    }

    pub fn send_notification<N: Notification>(&self, params: N::Params) {
        self.sender
            .send(Message::Notification(lsp_server::Notification {
                method: N::METHOD.to_owned(),
                params: to_value(params),
            }))
            .unwrap();
        info!("send diagnostics");
    }
}

#[async_trait::async_trait]
#[allow(unused)] // avoid `_` in all unimplemented handlers
pub trait LanguageServer<Options = ()>: Sized + Send + Sync + 'static {
    // lifecycle
    async fn initialize(params: InitializeParams, client: Client, options: Options)
    -> Result<Self>;
    async fn shutdown(self) -> Result<()>;

    // misc
    async fn handle_request(&self, method: String, params: Value) -> Result<Value> {
        forr! {($request:ty, $method:ty) in [
            (CodeActionRequest, code_action), (ExecuteCommand, execute_command),
        ] $:
            match method.as_str() {
                $(lsp_types::request::$request::METHOD => self.$method(from_value(params)?).await.map(to_value),)*
                // TODO
                _ => self.unknown_request(method, params).await,
            }
        }
    }
    async fn unknown_request(&self, method: String, params: Value) -> Result<Value> {
        error!("unkown request method: `{method}`");
        Err(method_not_found!("unkown request method: `{method}`"))
    }
    async fn handle_notification(&self, method: String, params: Value) {
        info!("handling {method:?} {params:?}");
        forr! {($request:ty, $method:ty) in [
            (DidChangeTextDocument, did_change), (DidOpenTextDocument, did_open), (DidSaveTextDocument, did_save)
        ] $:
            match method.as_str() {
                $(lsp_types::notification::$request::METHOD => match from_value(params) {
                    Ok(params) => self.$method(params).await,
                    Err(e) => error!("{e}"),
                })*,
                _ => self.unknown_notification(method.clone(), params).await,
            }
        };
        info!("handled {method:?}");
    }

    async fn unknown_notification(&self, method: String, params: Value) {
        error!("unkown notification method: `{method}`");
    }

    // notifications
    async fn did_change(&self, params: lsp_types::DidChangeTextDocumentParams) {}
    async fn did_open(&self, params: lsp_types::DidOpenTextDocumentParams) {}
    async fn did_save(&self, params: lsp_types::DidSaveTextDocumentParams) {}

    // requests
    async fn code_action(
        &self,
        params: lsp_types::CodeActionParams,
    ) -> Result<Option<Vec<lsp_types::CodeActionOrCommand>>> {
        warn!("Got a textDocument/codeAction request, but it is not implemented");
        Err(method_not_found!())
    }
    async fn execute_command(
        &self,
        params: lsp_types::ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        warn!("Got a workspace/executeCommand request, but it is not implemented");
        Err(method_not_found!())
    }
}
