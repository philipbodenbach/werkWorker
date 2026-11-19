#![allow(dead_code)]
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
    pub raw: String,
}
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl Response {
    pub fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }
}
pub struct Server {
    pub origin: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        Self::with_body_delay(handler, None).await
    }
    /// Delay individual chat response chunks, leaving discovery responses immediate.
    pub async fn with_body_delay(
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
        delay: Option<(usize, std::time::Duration)>,
    ) -> Self {
        Self::with_chat_delays(handler, None, delay).await
    }
    pub async fn with_chat_delays(
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
        header_delay: Option<std::time::Duration>,
        delay: Option<(usize, std::time::Duration)>,
    ) -> Self {
        Self::listen(
            move |request| protocol_response(request, handler(request)),
            header_delay,
            delay,
        )
        .await
    }
    pub async fn raw(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        Self::listen(handler, None, None).await
    }
    async fn listen(
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
        header_delay: Option<std::time::Duration>,
        delay: Option<(usize, std::time::Duration)>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local HTTP test listener");
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let records = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                // Each timed SSE write must reach the client at its scheduled interval.
                socket.set_nodelay(true).unwrap();
                let handler = handler.clone();
                let records = records.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0; 4096];
                    let (header_end, length) = loop {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                        if data.len() > 2 * 1024 * 1024 {
                            return;
                        }
                        if let Some(i) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&data[..i]);
                            let len = headers
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|s| s.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            break (i + 4, len);
                        }
                    };
                    while data.len() < header_end + length {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                    }
                    let raw = String::from_utf8_lossy(&data).to_string();
                    let mut first = raw.lines().next().unwrap().split_whitespace();
                    let request = Request {
                        method: first.next().unwrap().into(),
                        path: first.next().unwrap().into(),
                        body: serde_json::from_slice(&data[header_end..header_end + length])
                            .unwrap_or(Value::Null),
                        raw,
                    };
                    records.lock().unwrap().push(request.clone());
                    let response = handler(&request);
                    let timed_body = delay.filter(|_| request.path == "/werk/v1/chat/completions");
                    // The server streams an unknown-length SSE body. HTTP chunk boundaries
                    // keep timed writes observable even when the TCP stack coalesces bytes.
                    let chunked = timed_body.is_some()
                        && response.headers.iter().any(|(name, value)| {
                            name.eq_ignore_ascii_case("content-type")
                                && value.starts_with("text/event-stream")
                        });
                    let framing = if chunked {
                        "Transfer-Encoding: chunked".to_owned()
                    } else {
                        format!("Content-Length: {}", response.body.len())
                    };
                    let mut header = format!(
                        "HTTP/1.1 {} Test\r\n{framing}\r\nConnection: close\r\n",
                        response.status
                    );
                    for (k, v) in response.headers {
                        header.push_str(&format!("{k}: {v}\r\n"));
                    }
                    header.push_str("\r\n");
                    if request.path == "/werk/v1/chat/completions"
                        && let Some(pause) = header_delay
                    {
                        tokio::time::sleep(pause).await;
                    }
                    let _ = socket.write_all(header.as_bytes()).await;
                    if let Some((size, pause)) = timed_body {
                        for chunk in response.body.chunks(size) {
                            tokio::time::sleep(pause).await;
                            let result = if chunked {
                                let mut frame = format!("{:x}\r\n", chunk.len()).into_bytes();
                                frame.extend_from_slice(chunk);
                                frame.extend_from_slice(b"\r\n");
                                socket.write_all(&frame).await
                            } else {
                                socket.write_all(chunk).await
                            };
                            if result.is_err() {
                                break;
                            }
                        }
                        if chunked {
                            let _ = socket.write_all(b"0\r\n\r\n").await;
                        }
                    } else {
                        let _ = socket.write_all(&response.body).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            origin,
            requests,
            task,
        }
    }
}
// Test scenarios supply the original completion payload or SSE bytes. Encode the
// Werk HTTP representation here so all deadlines, error cases and tools use it.
fn protocol_response(request: &Request, mut response: Response) -> Response {
    if !matches!(
        request.path.as_str(),
        "/werk/v1/chat/completions" | "/werk/v1/models"
    ) {
        return response;
    }
    response.headers.extend([
        ("x-werk-protocol-version".into(), "1.0".into()),
        ("x-werk-request-id".into(), "req_fixture".into()),
    ]);
    if response.status == 200 && request.body["stream"] == true {
        response
            .headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
        response
            .headers
            .push(("content-type".into(), "text/event-stream".into()));
        return response;
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
        let mut envelope = json!({"protocol":{"major":1,"minor":0},"request_id":"req_fixture"});
        if response.status < 300 {
            envelope["data"] = value;
        } else {
            let err = &value["error"];
            let mut details = err.get("details").cloned().unwrap_or_else(|| json!({}));
            details["inference_code"] = err["code"].clone();
            for name in [
                "context_length",
                "prompt_tokens",
                "max_tokens",
                "token_count_method",
            ] {
                if let Some(v) = err.get(name) {
                    details[name] = v.clone();
                }
            }
            envelope["error"] = json!({"code":"invalid_request","message":err["message"].as_str().unwrap_or("Rejected"),
                "retryable":matches!(response.status,429 | 500..=599),"details":details});
        }
        response.body = envelope.to_string().into_bytes();
    }
    response
}
pub fn control(mut data: Value) -> Response {
    if let Some(caps) = data.get_mut("capabilities").and_then(Value::as_array_mut) {
        caps.extend([
            json!({"id":"api.models.list","status":"supported","detail":"catalog","operations":["list"]}),
            json!({"id":"api.chat.completions","status":"supported","detail":"chat","operations":["complete","stream"]}),
        ]);
    }
    let mut r = Response::json(
        json!({"protocol":{"major":1,"minor":0},"request_id":"req_test","data":data}),
    );
    r.headers.extend([
        ("x-werk-protocol-version".into(), "1.0".into()),
        ("x-werk-request-id".into(), "req_test".into()),
    ]);
    r
}
pub fn werk(request: &Request) -> Response {
    match request.path.as_str() {
        "/werk/v1/info" => control(
            json!({"service":"werk1112","service_version":"fixture","protocol":{"major":1,"minor":0},"active_backend":"fixture","limits":{"max_request_bytes":1048576}}),
        ),
        "/werk/v1/capabilities" => control(
            json!({"capabilities":[{"id":"runtime.state.prefix_cache","status":"externally_managed","detail":"automatic backend reuse only","operations":["automatic_reuse"]},{"id":"runtime.pd.prefill","status":"metadata_only","detail":"not executable","operations":["prefill"]},{"id":"api.tools","status":"supported","detail":"Werk native media tools","operations":["list","tasks","call"]}]}),
        ),
        "/werk/v1/tasks" => control(
            json!({"object":"werk.capabilities","models":[{"id":"fixture-model","available_tasks":["text_generation","image_generation"],"task_statuses":{"image_generation":{"status":"available"}}}]}),
        ),
        "/werk/v1/models" => Response::json(
            json!({"object":"list","data":[{"id":"fixture-model","object":"model"}]}),
        ),
        "/v1/parameters?task=text-generation" => {
            Response::json(json!({"task":"text-generation","parameters":[]}))
        }
        "/werk/v1/tools" => control(
            json!({"object":"werk.tools","tools":[{"type":"function","function":{"name":"image_generation","description":"Image task","parameters":{"type":"object","properties":{"model":{"type":"string"},"prompt":{"type":"string"}},"required":["model","prompt"],"additionalProperties":false}}}]}),
        ),
        "/werk/v1/chat/completions" => Response::json(
            json!({"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":1}}),
        ),
        _ => Response {
            status: 404,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    }
}
