//! Read-only, token-addressed loopback preview. It never accepts browser actions.
#[cfg(target_os = "linux")]
use crate::core::*;
use serde_json::{Value, json};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

#[derive(Default)]
struct Frame {
    metadata: Value,
    jpeg: Vec<u8>,
}
pub(crate) struct BrowserView {
    pub url: String,
    frame: Arc<Mutex<Frame>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for BrowserView {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl BrowserView {
    #[cfg(target_os = "linux")]
    pub async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let token = id("view");
        let url = format!("http://{}/{token}/", listener.local_addr()?);
        let frame = Arc::new(Mutex::new(Frame::default()));
        let state = frame.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    connection=listener.accept() => {
                        let Ok((mut socket,_))=connection else {break};
                        if connections.len()>=8 {continue;}
                        let state=state.clone();let token=token.clone();
                        connections.spawn(async move {
                            let _=tokio::time::timeout(Duration::from_secs(3),async {
                                let mut bytes=Vec::new();let mut buffer=[0;1024];
                                while bytes.len()<8192 && !bytes.windows(4).any(|b|b==b"\r\n\r\n") {
                                    let n=socket.read(&mut buffer).await?;
                                    if n==0 {return Ok::<(),std::io::Error>(());}
                                    bytes.extend_from_slice(&buffer[..n]);
                                }
                                let request=String::from_utf8_lossy(&bytes);
                                let mut first=request.lines().next().unwrap_or("").split_whitespace();
                                let method=first.next().unwrap_or("");
                                let path=first.next().unwrap_or("").split('?').next().unwrap_or("");
                                let route=path.strip_prefix(&format!("/{token}/"));
                                let (status,mime,body)=match (method,route) {
                                    ("GET",Some("")) => (200,"text/html; charset=utf-8",include_bytes!("browser_view.html").to_vec()),
                                    ("GET",Some("state")) => (200,"application/json",state.lock().await.metadata.to_string().into_bytes()),
                                    ("GET",Some("frame")) => (200,"image/jpeg",state.lock().await.jpeg.clone()),
                                    _ => (404,"text/plain",b"Not found".to_vec()),
                                };
                                let header=format!("HTTP/1.1 {status} Response\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; img-src 'self'; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\nConnection: close\r\n\r\n",body.len());
                                socket.write_all(header.as_bytes()).await?;
                                socket.write_all(&body).await?;
                                socket.shutdown().await
                            }).await;
                        });
                    },
                    _=connections.join_next(), if !connections.is_empty()=>{},
                }
            }
        });
        Ok(Self { url, frame, task })
    }
    pub async fn update(&self, data: &Value, jpeg: Vec<u8>, revision: u64, action: &str) {
        *self.frame.lock().await = Frame {
            jpeg,
            metadata: json!({"revision":revision,"title":data["title"],"url":data["url"],"pointer":data["pointer"],"viewport":data["viewport"],"action":action}),
        };
    }
}
