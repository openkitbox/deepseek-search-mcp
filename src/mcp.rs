use std::{collections::HashMap, io::ErrorKind, sync::Arc};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{deepseek::DeepSeekClient, error::AppError, tools};

pub const PROTOCOL_VERSION: &str = "2026-07-28";
const SERVER_NAME: &str = "deepseek-search-mcp";
const SERVER_TITLE: &str = "DeepSeek Search MCP";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_INPUT_LINE_BYTES: usize = 1024 * 1024;
const RESPONSE_CHANNEL_CAPACITY: usize = 64;
const MAX_CONCURRENT_TOOLS: usize = 8;

#[derive(Clone)]
pub struct McpServer {
    client: DeepSeekClient,
}

impl McpServer {
    pub fn new(client: DeepSeekClient) -> Self {
        Self { client }
    }

    pub async fn run<R, W>(&self, reader: R, writer: W) -> Result<(), AppError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = BufReader::new(reader);
        let (responses, mut response_rx) = mpsc::channel::<String>(RESPONSE_CHANNEL_CAPACITY);
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(response) = response_rx.recv().await {
                writer
                    .write_all(response.as_bytes())
                    .await
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                writer.flush().await?;
            }
            Ok::<(), std::io::Error>(())
        });
        let cancellations = Arc::new(Mutex::new(HashMap::<RequestId, CancellationToken>::new()));
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_TOOLS));
        let mut tool_tasks = JoinSet::new();
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        let mut shutdown_requested = false;
        loop {
            let line = tokio::select! {
                _ = &mut shutdown => {
                    shutdown_requested = true;
                    break;
                },
                line = read_bounded_line(&mut reader) => line.map_err(io_error)?,
            };
            let Some(line) = line else { break };
            let request = match line {
                BoundedLine::TooLong => {
                    send(&responses, error_response(None, -32600, "Invalid Request")).await?;
                    continue;
                }
                BoundedLine::Complete(line) => parse_line(&line),
            };
            let request = match request {
                Ok(request) => request,
                Err(response) => {
                    send(&responses, response).await?;
                    continue;
                }
            };
            let Some(request) = request else { continue };
            if request.method == "notifications/cancelled" {
                cancel_request(&cancellations, request.params.as_ref()).await;
                continue;
            }
            if request.id.is_none() {
                continue;
            }
            let Some(id) = request.id.clone() else {
                continue;
            };
            let requires_modern_metadata = request.method == "server/discover"
                || matches!(request.method.as_str(), "tools/list" | "tools/call")
                    && has_metadata(request.params.as_ref());
            if requires_modern_metadata && !has_valid_modern_metadata(request.params.as_ref()) {
                send(
                    &responses,
                    error_response(Some(id.value()), -32602, "Invalid modern protocol metadata"),
                )
                .await?;
                continue;
            }
            if request.method == "tools/call" {
                let permit = match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        let response = response_for_tool(
                            id,
                            Err(AppError::Tool {
                                detail: "tool concurrency limit reached".to_owned(),
                            }),
                        );
                        send(&responses, response).await?;
                        continue;
                    }
                };
                let cancellation = CancellationToken::new();
                cancellations
                    .lock()
                    .await
                    .insert(id.clone(), cancellation.clone());
                let client = self.client.clone();
                let responses = responses.clone();
                let cancellations = Arc::clone(&cancellations);
                tool_tasks.spawn(async move {
                    let _permit = permit;
                    let result = dispatch_tool(client, request.params, cancellation).await;
                    let response = response_for_tool(id.clone(), result);
                    let send_result = send(&responses, response).await;
                    cancellations.lock().await.remove(&id);
                    send_result
                });
            } else {
                let response = dispatch_request(&request);
                send(&responses, response).await?;
            }
        }
        if shutdown_requested {
            cancel_all(&cancellations).await;
        }
        while let Some(result) = tool_tasks.join_next().await {
            result.map_err(|error| AppError::Transport {
                detail: error.to_string(),
            })??;
        }
        drop(responses);
        writer_task
            .await
            .map_err(|error| AppError::Transport {
                detail: error.to_string(),
            })?
            .map_err(io_error)
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            let _ = signal.recv().await;
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[derive(Debug, Eq, PartialEq)]
enum BoundedLine {
    Complete(Vec<u8>),
    TooLong,
}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<BoundedLine>, std::io::Error>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if too_long {
                Ok(Some(BoundedLine::TooLong))
            } else if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(BoundedLine::Complete(line)))
            };
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let chunk = &buffer[..newline];
            if !too_long {
                if line.len().saturating_add(chunk.len()) <= MAX_INPUT_LINE_BYTES {
                    line.extend_from_slice(chunk);
                } else {
                    too_long = true;
                }
            }
            reader.consume(newline + 1);
            return Ok(Some(if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Complete(line)
            }));
        }
        if !too_long {
            if line.len().saturating_add(buffer.len()) <= MAX_INPUT_LINE_BYTES {
                line.extend_from_slice(buffer);
            } else {
                too_long = true;
            }
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RequestId {
    String(String),
    Number(String),
    Null,
}

impl RequestId {
    fn value(&self) -> Value {
        match self {
            Self::String(value) => Value::String(value.clone()),
            Self::Number(value) => serde_json::from_str(value).unwrap_or(Value::Null),
            Self::Null => Value::Null,
        }
    }
}

struct Request {
    id: Option<RequestId>,
    method: String,
    params: Option<Value>,
}

fn parse_line(line: &[u8]) -> Result<Option<Request>, Value> {
    let line =
        std::str::from_utf8(line).map_err(|_| error_response(None, -32600, "Invalid Request"))?;
    let value: Value =
        serde_json::from_str(line).map_err(|_| error_response(None, -32700, "Parse error"))?;
    let Some(object) = value.as_object() else {
        return Err(error_response(None, -32600, "Invalid Request"));
    };
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned()))
        || object.get("method").and_then(Value::as_str).is_none()
    {
        return Err(error_response(
            request_id(object.get("id")),
            -32600,
            "Invalid Request",
        ));
    }
    let id = match object.get("id") {
        None => None,
        Some(value) => {
            Some(parse_id(value).ok_or_else(|| error_response(None, -32600, "Invalid Request"))?)
        }
    };
    Ok(Some(Request {
        id,
        method: object["method"].as_str().unwrap_or_default().to_owned(),
        params: object.get("params").cloned(),
    }))
}

fn parse_id(value: &Value) -> Option<RequestId> {
    match value {
        Value::String(value) => Some(RequestId::String(value.clone())),
        Value::Number(value) => Some(RequestId::Number(value.to_string())),
        Value::Null => Some(RequestId::Null),
        _ => None,
    }
}

fn request_id(value: Option<&Value>) -> Option<Value> {
    value.cloned().filter(|value| parse_id(value).is_some())
}

async fn cancel_request(
    cancellations: &Mutex<HashMap<RequestId, CancellationToken>>,
    params: Option<&Value>,
) {
    let Some(id) = params
        .and_then(|params| params.get("requestId"))
        .and_then(parse_id)
    else {
        return;
    };
    if let Some(token) = cancellations.lock().await.get(&id) {
        token.cancel();
    }
}

async fn cancel_all(cancellations: &Mutex<HashMap<RequestId, CancellationToken>>) {
    let tokens = cancellations
        .lock()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for token in tokens {
        token.cancel();
    }
}

fn dispatch_request(request: &Request) -> Value {
    match request.method.as_str() {
        "initialize" => success(
            request.id.as_ref(),
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {"name": SERVER_NAME, "title": SERVER_TITLE, "version": SERVER_VERSION},
                "capabilities": {"tools": {}},
                "instructions": tools::SERVER_INSTRUCTIONS
            }),
        ),
        "server/discover" => success(
            request.id.as_ref(),
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {"name": SERVER_NAME, "title": SERVER_TITLE, "version": SERVER_VERSION},
                "capabilities": {"tools": {}},
                "instructions": tools::SERVER_INSTRUCTIONS
            }),
        ),
        "tools/list" => success(
            request.id.as_ref(),
            json!({"tools": [tools::search_tool(), tools::fetch_tool()]}),
        ),
        _ => error_response(
            request.id.as_ref().map(RequestId::value),
            -32601,
            "Method not found",
        ),
    }
}

fn has_valid_modern_metadata(params: Option<&Value>) -> bool {
    let Some(meta) = params
        .and_then(Value::as_object)
        .and_then(|params| params.get("_meta"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    meta.get("protocolVersion").and_then(Value::as_str) == Some(PROTOCOL_VERSION)
        && meta.get("clientInfo").is_some_and(Value::is_object)
        && meta.get("clientCapabilities").is_some_and(Value::is_object)
}

fn has_metadata(params: Option<&Value>) -> bool {
    params
        .and_then(Value::as_object)
        .is_some_and(|params| params.contains_key("_meta"))
}

async fn dispatch_tool(
    client: DeepSeekClient,
    params: Option<Value>,
    cancellation: CancellationToken,
) -> Result<String, AppError> {
    let params = params.unwrap_or_else(|| json!({}));
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Tool {
            detail: "tool name is required".to_owned(),
        })?;
    let empty_arguments = Value::Object(Default::default());
    let arguments = params.get("arguments").unwrap_or(&empty_arguments);
    match name {
        tools::SEARCH_TOOL_NAME => {
            let request = tools::validate_search_args(arguments)?;
            let need_content = request.need_content.unwrap_or(false);
            let query = request.query.clone();
            let response = client.search(request, &cancellation).await?;
            tools::normalize_search_response(&query, response, need_content)
        }
        tools::FETCH_TOOL_NAME => {
            let request = tools::validate_fetch_args(arguments)?;
            let response = client.fetch(request, &cancellation).await?;
            tools::normalize_fetch_response(response)
        }
        _ => Err(AppError::Tool {
            detail: "unknown tool".to_owned(),
        }),
    }
}

fn response_for_tool(id: RequestId, result: Result<String, AppError>) -> Value {
    match result {
        Ok(text) => success(
            Some(&id),
            json!({"content": [{"type": "text", "text": text}], "isError": false}),
        ),
        Err(error) => success(
            Some(&id),
            json!({"content": [{"type": "text", "text": error.model_message()}], "isError": true}),
        ),
    }
}

fn success(id: Option<&RequestId>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.map(RequestId::value).unwrap_or(Value::Null), "result": result})
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": {"code": code, "message": message}})
}

async fn send(sender: &mpsc::Sender<String>, response: Value) -> Result<(), AppError> {
    if response.is_null() {
        return Ok(());
    }
    let mut text = serde_json::to_string(&response).map_err(|_| AppError::Mcp {
        detail: "failed to serialize response".to_owned(),
    })?;
    text.push('\n');
    sender.send(text).await.map_err(|_| AppError::Transport {
        detail: "response writer stopped".to_owned(),
    })
}

fn io_error(error: std::io::Error) -> AppError {
    if error.kind() == ErrorKind::UnexpectedEof {
        AppError::Transport {
            detail: "unexpected EOF".to_owned(),
        }
    } else {
        AppError::Transport {
            detail: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, Instant},
    };

    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Mutex, Semaphore};
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{method, path},
    };

    use super::{
        BoundedLine, MAX_INPUT_LINE_BYTES, McpServer, PROTOCOL_VERSION, RequestId, SERVER_NAME,
        SERVER_TITLE, cancel_all, read_bounded_line,
    };
    use crate::{config::Config, deepseek::DeepSeekClient};

    fn client(endpoint: &str) -> DeepSeekClient {
        let config = Config::from_value(Some("test-secret")).expect("test config");
        DeepSeekClient::new_for_test(&config, endpoint.to_owned()).expect("test client")
    }

    struct NotifyingResponder {
        request_started: Arc<tokio::sync::Notify>,
    }

    impl Respond for NotifyingResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.request_started.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(json!({"results":[]}))
                .set_delay(Duration::from_secs(5))
        }
    }

    async fn run(server: DeepSeekClient, lines: &[Value]) -> Vec<Value> {
        let (mut input_writer, input_reader) = tokio::io::duplex(16 * 1024);
        let (output_writer, mut output_reader) = tokio::io::duplex(64 * 1024);
        let input = lines
            .iter()
            .map(|value| serde_json::to_string(value).expect("request JSON") + "\n")
            .collect::<String>();
        let producer = tokio::spawn(async move {
            input_writer
                .write_all(input.as_bytes())
                .await
                .expect("write requests");
            input_writer.shutdown().await.expect("close input");
        });
        McpServer::new(server)
            .run(input_reader, output_writer)
            .await
            .expect("server run");
        producer.await.expect("producer task");
        let mut bytes = Vec::new();
        output_reader
            .read_to_end(&mut bytes)
            .await
            .expect("read responses");
        String::from_utf8(bytes)
            .expect("UTF-8 responses")
            .lines()
            .map(|line| serde_json::from_str(line).expect("response JSON"))
            .collect()
    }

    fn modern_meta() -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientInfo": {"name": "ox", "version": "test"},
            "clientCapabilities": {}
        })
    }

    #[tokio::test]
    async fn discovery_returns_modern_server_metadata_and_capabilities() {
        let responses = run(
            client("http://127.0.0.1:1"),
            &[json!({
                "jsonrpc": "2.0",
                "id": "discover",
                "method": "server/discover",
                "params": {"_meta": modern_meta()}
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"],
            "deepseek-search-mcp"
        );
        assert_eq!(
            responses[0]["result"]["serverInfo"]["title"],
            "DeepSeek Search MCP"
        );
        assert!(responses[0]["result"]["serverInfo"]["version"].is_string());
        assert!(responses[0]["result"]["capabilities"]["tools"].is_object());
        assert_eq!(
            responses[0]["result"]["instructions"],
            crate::tools::SERVER_INSTRUCTIONS
        );
    }

    #[tokio::test]
    async fn legacy_initialize_returns_server_capabilities() {
        let responses = run(
            client("http://127.0.0.1:1"),
            &[json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})],
        )
        .await;

        assert_eq!(responses[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(responses[0]["result"]["serverInfo"]["title"], SERVER_TITLE);
        assert!(responses[0]["result"]["serverInfo"]["version"].is_string());
        assert!(responses[0]["result"]["capabilities"]["tools"].is_object());
        assert_eq!(
            responses[0]["result"]["instructions"],
            crate::tools::SERVER_INSTRUCTIONS
        );
    }

    #[tokio::test]
    async fn discovery_rejects_missing_or_wrong_modern_metadata() {
        let responses = run(
            client("http://127.0.0.1:1"),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}),
                json!({"jsonrpc":"2.0","id":2,"method":"server/discover","params":{"_meta":{"protocolVersion":"unknown","clientInfo":{},"clientCapabilities":{}}}}),
                json!({"jsonrpc":"2.0","id":3,"method":"server/discover","params":{"_meta":{"protocolVersion":PROTOCOL_VERSION,"clientInfo":"bad","clientCapabilities":{}}}}),
                json!({"jsonrpc":"2.0","id":4,"method":"server/discover","params":[]}),
            ],
        )
        .await;

        assert_eq!(responses.len(), 4);
        for response in responses {
            assert_eq!(response["error"]["code"], -32602);
        }
    }

    #[tokio::test]
    async fn legacy_tools_list_without_metadata_returns_tools() {
        let responses = run(
            client("http://127.0.0.1:1"),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":modern_meta()}}),
            ],
        )
        .await;

        let tools = responses[0]["result"]["tools"]
            .as_array()
            .expect("tools array");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(tools[1]["name"], "fetch");
        assert_eq!(
            responses[1]["result"]["tools"],
            responses[0]["result"]["tools"]
        );
    }

    #[tokio::test]
    async fn legacy_tools_call_without_metadata_executes_tool() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
            .expect(1)
            .mount(&mock)
            .await;
        let responses = run(
            client(&mock.uri()),
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": "rust"}}
            })],
        )
        .await;

        assert_eq!(responses[0]["result"]["isError"], false);
        mock.verify().await;
    }

    #[tokio::test]
    async fn tools_list_and_call_reject_invalid_metadata_without_legacy_fallback() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
            .expect(0)
            .mount(&mock)
            .await;
        let responses = run(
            client(&mock.uri()),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{},"name":"search","arguments":{"query":"rust"}}}),
            ],
        )
        .await;

        assert_eq!(responses[0]["error"]["code"], -32602);
        assert_eq!(responses[1]["error"]["code"], -32602);
        mock.verify().await;
    }
    #[tokio::test]
    async fn invalid_arguments_return_tool_error_without_http_request() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
            .expect(0)
            .mount(&mock)
            .await;
        let responses = run(
            client(&mock.uri()),
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":modern_meta(),"name":"search","arguments":{"query":""}}})],
        )
        .await;
        assert_eq!(responses[0]["result"]["isError"], true);
        mock.verify().await;
    }

    #[tokio::test]
    async fn tool_call_returns_normalized_result_and_unknown_tool_is_error() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
            .expect(1)
            .mount(&mock)
            .await;
        let responses = run(
            client(&mock.uri()),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":modern_meta(),"name":"search","arguments":{"query":"rust"}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":modern_meta(),"name":"missing","arguments":{}}}),
            ],
        )
        .await;
        assert_eq!(responses.len(), 2);
        let by_id = responses
            .iter()
            .map(|response| (response["id"].clone(), response))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(by_id[&json!(1)]["result"]["isError"], false);
        assert_eq!(by_id[&json!(2)]["result"]["isError"], true);
        mock.verify().await;
    }

    #[tokio::test]
    async fn malformed_invalid_and_unknown_requests_use_jsonrpc_errors() {
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, mut output_reader) = tokio::io::duplex(4096);
        let producer = tokio::spawn(async move {
            input_writer
                .write_all(b"{bad json}\n")
                .await
                .expect("malformed");
            input_writer
                .write_all(b"[]\n")
                .await
                .expect("invalid request");
            input_writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"nope\"}\n")
                .await
                .expect("unknown method");
            input_writer.shutdown().await.expect("close input");
        });
        McpServer::new(client("http://127.0.0.1:1"))
            .run(input_reader, output_writer)
            .await
            .expect("server run");
        producer.await.expect("producer");
        let mut bytes = Vec::new();
        output_reader
            .read_to_end(&mut bytes)
            .await
            .expect("responses");
        let responses: Vec<Value> = String::from_utf8(bytes)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON"))
            .collect();
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[1]["error"]["code"], -32600);
        assert_eq!(responses[2]["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn unknown_initialize_and_notifications_preserve_jsonrpc_behavior() {
        let responses = run(
            client("http://127.0.0.1:1"),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize/unknown","params":{}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}),
            ],
        )
        .await;

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn concurrent_calls_keep_ids_and_each_response_is_a_complete_line() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"results":[]}))
                    .set_delay(Duration::from_millis(30)),
            )
            .expect(2)
            .mount(&mock)
            .await;
        let responses = run(
            client(&mock.uri()),
            &[
                json!({"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"_meta":modern_meta(),"name":"search","arguments":{"query":"a"}}}),
                json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"_meta":modern_meta(),"name":"search","arguments":{"query":"b"}}}),
            ],
        )
        .await;
        assert_eq!(responses.len(), 2);
        let ids = responses
            .iter()
            .map(|response| response["id"].clone())
            .collect::<Vec<_>>();
        assert!(ids.contains(&json!(7)));
        assert!(ids.contains(&json!("a")));
        mock.verify().await;
    }

    #[tokio::test]
    async fn eof_waits_for_slow_tool_call_and_returns_its_response() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"results":[]}))
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let started = Instant::now();
        let responses = run(
            client(&mock.uri()),
            &[json!({"jsonrpc":"2.0","id":"slow","method":"tools/call","params":{"_meta":modern_meta(),"name":"search","arguments":{"query":"slow"}}})],
        )
        .await;

        assert!(started.elapsed() >= Duration::from_millis(80));
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], "slow");
        assert_eq!(responses[0]["result"]["isError"], false);
        mock.verify().await;
    }

    #[tokio::test]
    async fn cancellation_notification_cancels_call_and_notifications_write_nothing() {
        let mock = MockServer::start().await;
        let request_started = Arc::new(tokio::sync::Notify::new());
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(NotifyingResponder {
                request_started: Arc::clone(&request_started),
            })
            .expect(1)
            .mount(&mock)
            .await;
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, mut output_reader) = tokio::io::duplex(4096);
        let endpoint = mock.uri();
        let task = tokio::spawn(async move {
            McpServer::new(client(&endpoint))
                .run(input_reader, output_writer)
                .await
                .expect("server run");
        });
        input_writer.write_all(format!("{}\n", serde_json::json!({"jsonrpc":"2.0","id":"cancel-me","method":"tools/call","params":{"_meta":modern_meta(),"name":"search","arguments":{"query":"slow"}}})).as_bytes()).await.expect("call");
        tokio::time::timeout(Duration::from_secs(1), request_started.notified())
            .await
            .expect("request observed");
        input_writer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":\"cancel-me\"}}\n").await.expect("cancel");
        input_writer.shutdown().await.expect("close input");
        task.await.expect("server task");
        let mut bytes = Vec::new();
        output_reader
            .read_to_end(&mut bytes)
            .await
            .expect("responses");
        let responses: Vec<Value> = String::from_utf8(bytes)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON"))
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], "cancel-me");
        assert_eq!(responses[0]["result"]["isError"], true);
        mock.verify().await;
    }

    #[tokio::test]
    async fn bounded_line_rejects_oversized_input_and_keeps_following_lines() {
        let mut input = vec![b'x'; MAX_INPUT_LINE_BYTES + 1];
        input.push(b'\n');
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let mut reader = tokio::io::BufReader::new(input.as_slice());

        assert_eq!(
            read_bounded_line(&mut reader)
                .await
                .expect("oversized line read"),
            Some(BoundedLine::TooLong)
        );
        assert!(matches!(
            read_bounded_line(&mut reader)
                .await
                .expect("following line read"),
            Some(BoundedLine::Complete(_))
        ));
        assert!(
            read_bounded_line(&mut reader)
                .await
                .expect("EOF read")
                .is_none()
        );
    }

    #[tokio::test]
    async fn shutdown_helper_cancels_every_running_tool() {
        let map = Mutex::new(HashMap::from([
            (RequestId::String("a".to_owned()), CancellationToken::new()),
            (RequestId::String("b".to_owned()), CancellationToken::new()),
        ]));
        let tokens = map.lock().await.values().cloned().collect::<Vec<_>>();
        cancel_all(&map).await;
        assert!(tokens.iter().all(CancellationToken::is_cancelled));
    }

    #[test]
    fn tool_permit_rejection_is_immediate_when_capacity_is_exhausted() {
        let semaphore = Semaphore::new(1);
        let permit = semaphore.try_acquire().expect("first permit");
        assert!(semaphore.try_acquire().is_err());
        drop(permit);
        assert!(semaphore.try_acquire().is_ok());
    }

    #[test]
    fn cancellation_token_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CancellationToken>();
    }
}
