//! Loopback browser panel. Mutations require a same-origin JSON host request.
use crate::core::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

#[async_trait]
pub(crate) trait BrowserHost: Send + Sync {
    async fn call(&self, request: Value) -> Result<Value>;
}
static PORTS: OnceLock<StdMutex<BTreeSet<u16>>> = OnceLock::new();
pub(crate) fn protected_port(port: u16) -> bool {
    PORTS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .contains(&port)
}
#[derive(Default)]
struct Frame {
    metadata: Value,
    jpeg: Vec<u8>,
}
pub(crate) struct BrowserView {
    pub url: String,
    frame: Arc<Mutex<Frame>>,
    task: tokio::task::JoinHandle<()>,
    port: u16,
}
impl Drop for BrowserView {
    fn drop(&mut self) {
        self.task.abort();
        PORTS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&self.port);
    }
}
impl BrowserView {
    pub async fn start() -> Result<Self> {
        Self::start_with_host(None).await
    }
    pub async fn start_with_host(host: Option<Arc<dyn BrowserHost>>) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let origin = format!("http://127.0.0.1:{port}");
        let token = id("view");
        let url = format!("{origin}/{token}/");
        PORTS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(port);
        let frame = Arc::new(Mutex::new(Frame::default()));
        let state = frame.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    connection=listener.accept()=>{
                        let Ok((mut socket,_))=connection else{break};if connections.len()>=8{continue;}
                        let state=state.clone();let token=token.clone();let host=host.clone();let origin=origin.clone();
                        connections.spawn(async move {let _=tokio::time::timeout(Duration::from_secs(55),async {
                            let mut bytes=Vec::new();let mut buffer=[0;4096];
                            let header_end=loop {
                                if let Some(pos)=bytes.windows(4).position(|b|b==b"\r\n\r\n"){break pos+4;}
                                if bytes.len()>8192{return Ok::<(),std::io::Error>(());}
                                let n=socket.read(&mut buffer).await?;if n==0{return Ok(());}bytes.extend_from_slice(&buffer[..n]);
                            };
                            let header=String::from_utf8_lossy(&bytes[..header_end]).to_string();
                            let mut first=header.lines().next().unwrap_or("").split_whitespace();
                            let method=first.next().unwrap_or("");let path=first.next().unwrap_or("").split('?').next().unwrap_or("");
                            let route=path.strip_prefix(&format!("/{token}/"));
                            let headers:std::collections::BTreeMap<_,_>=header.lines().skip(1).filter_map(|s|s.split_once(':')).map(|(k,v)|(k.to_ascii_lowercase(),v.trim())).collect();
                            let (status,mime,body)=match (method,route) {
                                ("GET",Some(""))=>(200,"text/html; charset=utf-8",include_bytes!("browser_view.html").to_vec()),
                                ("GET",Some("frame"))=>(200,"image/jpeg",state.lock().await.jpeg.clone()),
                                ("GET",Some("state"))=>{
                                    let mut data=state.lock().await.metadata.clone();if !data.is_object(){data=json!({});}
                                    if let Some(host)=&host {
                                        match host.call(json!({"op":"status"})).await {Ok(value)=>data["host"]=value,Err(e)=>data["host"]=json!({"error":e.message})}
                                    }
                                    (200,"application/json",data.to_string().into_bytes())
                                },
                                ("POST",Some("control")) if host.is_some() && headers.get("origin")==Some(&origin.as_str()) && headers.get("x-werk-browser")==Some(&"1") && headers.get("content-type").is_some_and(|h|h.starts_with("application/json")) && !headers.contains_key("transfer-encoding")=>{
                                    let length=headers.get("content-length").and_then(|s|s.parse::<usize>().ok()).unwrap_or(0);
                                    if length==0 || length>32768{return Ok(());}
                                    while bytes.len()<header_end+length {let n=socket.read(&mut buffer).await?;if n==0{return Ok(());}bytes.extend_from_slice(&buffer[..n]);}
                                    match serde_json::from_slice::<Value>(&bytes[header_end..header_end+length]) {
                                        Ok(request)=>match host.as_ref().unwrap().call(request).await {
                                            Ok(value)=>(200,"application/json",value.to_string().into_bytes()),
                                            Err(e)=>(409,"application/json",json!({"error":e.message,"code":e.code}).to_string().into_bytes())
                                        },
                                        Err(_)=>(400,"application/json",br#"{"error":"Invalid JSON"}"#.to_vec()),
                                    }
                                },
                                _=>(404,"text/plain",b"Not found".to_vec()),
                            };
                            let header=format!("HTTP/1.1 {status} Response\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; img-src 'self'; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\nConnection: close\r\n\r\n",body.len());
                            socket.write_all(header.as_bytes()).await?;socket.write_all(&body).await?;socket.shutdown().await
                        }).await;});
                    },
                    _=connections.join_next(),if !connections.is_empty()=>{},
                }
            }
        });
        Ok(Self {
            url,
            frame,
            task,
            port,
        })
    }
    pub async fn update(&self, data: &Value, jpeg: Vec<u8>, revision: u64, action: &str) {
        *self.frame.lock().await = Frame {
            jpeg,
            metadata: json!({"revision":revision,"title":data["title"],"url":data["url"],"pointer":data["pointer"],"viewport":data["viewport"],"action":action,"tabs":data["tabs"],"controls":data["controls"]}),
        };
    }
}
