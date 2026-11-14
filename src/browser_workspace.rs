//! Static workspace pages served inside the existing browser broker. No host
//! listener or filesystem mount is exposed to the page; reads use the pinned
//! native Workspace backend. This does not enable a native browser launcher.
use crate::{core::*, runtime::Workspace};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub(crate) const ORIGIN: &str = "http://workspace.invalid";
const FILE_LIMIT: usize = 4 * 1024 * 1024;

pub(crate) struct Entry {
    pub path: String,
    pub url: reqwest::Url,
}

pub(crate) fn entry(path: &str) -> Result<Entry> {
    let path = crate::policy::workspace_path(path)?;
    let path = if path == "." {
        "index.html".to_owned()
    } else if path.ends_with('/') {
        format!("{path}index.html")
    } else {
        path.to_owned()
    };
    let mut url = reqwest::Url::parse(ORIGIN).unwrap();
    url.path_segments_mut()
        .unwrap()
        .clear()
        .extend(path.split('/'));
    Ok(Entry { path, url })
}

/// Only the sandbox's /workspace alias is accepted, never a host file URL.
pub(crate) fn file_entry(value: &str) -> Result<Option<Entry>> {
    let Ok(url) = reqwest::Url::parse(value) else {
        return Ok(None);
    };
    if url.scheme() != "file" {
        return Ok(None);
    }
    let path = url.to_file_path().map_err(|_| {
        WorkerError::new(
            "path_denied",
            "Use file:///workspace/... or browser.open with a relative path",
        )
    })?;
    let path = path
        .to_str()
        .ok_or_else(|| WorkerError::new("path_denied", "Expected a UTF-8 workspace path"))?;
    if !path.starts_with("/workspace/") {
        return Err(WorkerError::new(
            "path_denied",
            "Local browser files must be inside /workspace; use browser.open with path relative to the workspace",
        ));
    }
    let mut result = entry(path)?;
    result.url.set_query(url.query());
    result.url.set_fragment(url.fragment());
    Ok(Some(result))
}

pub(crate) async fn validate(entry: &Entry, workspace: &Workspace) -> Result<String> {
    Ok(hash(workspace.read(&entry.path, FILE_LIMIT).await?))
}

pub(crate) fn navigation(value: &str, local: bool) -> Result<String> {
    match file_entry(value)? {
        Some(entry) if local => Ok(entry.url.to_string()),
        Some(_) => Err(WorkerError::new(
            "browser_workspace",
            "Open this workspace page with browser.open first",
        )),
        None => Ok(value.to_owned()),
    }
}

fn request_path(url: &reqwest::Url) -> Result<String> {
    // URL's file-path conversion percent-decodes once, preserving Unicode and
    // literal percent signs. Workspace rejects decoded traversal/backslashes.
    let file = reqwest::Url::parse(&format!("file://{}", url.path()))
        .map_err(|_| WorkerError::new("path_denied", "Invalid local URL"))?;
    let path = file
        .to_file_path()
        .map_err(|_| WorkerError::new("path_denied", "Invalid local path"))?;
    let path = path
        .to_str()
        .ok_or_else(|| WorkerError::new("path_denied", "Expected UTF-8 path"))?
        .trim_start_matches('/');
    let path = if path.is_empty() {
        "index.html".to_owned()
    } else if path.ends_with('/') {
        format!("{path}index.html")
    } else {
        path.to_owned()
    };
    crate::policy::relative_path(&path)?;
    Ok(path)
}

fn mime(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub(crate) async fn serve(workspace: &Workspace, message: &Value) -> Result<Value> {
    let url = reqwest::Url::parse(message["url"].as_str().unwrap_or(""))
        .map_err(|_| WorkerError::new("invalid_url", "Invalid local URL"))?;
    if url.origin().ascii_serialization() != ORIGIN {
        return Err(WorkerError::new("browser_scope", "Not a workspace URL"));
    }
    let method = message["method"].as_str().unwrap_or("");
    if !matches!(method, "GET" | "HEAD") {
        return Ok(json!({"status":405,"headers":[["allow","GET, HEAD"]],"body":""}));
    }
    let path = request_path(&url)?;
    let (status, bytes) = match workspace.read(&path, FILE_LIMIT).await {
        Ok(bytes) => (200, bytes),
        Err(e) if e.code == "not_found" => (404, b"Workspace file not found".to_vec()),
        Err(e) => return Err(e),
    };
    Ok(json!({"status":status,"headers":[
        ["content-type",if status == 200 { mime(&path) } else { "text/plain" }],
        ["cache-control","no-store"], ["x-content-type-options","nosniff"],
        ["cross-origin-resource-policy","same-origin"]
    ],"body":if method == "HEAD" { String::new() } else { STANDARD.encode(bytes) }}))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_broker_handles_assets_without_escaping_or_writing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Grüße 100%.html"), "<title>Local</title>").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("outside")).unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let page = entry("Grüße 100%.html").unwrap();
        let response = serve(&workspace, &json!({"url":page.url.as_str(),"method":"GET"}))
            .await
            .unwrap();
        assert_eq!(response["status"], 200);
        assert_eq!(
            STANDARD.decode(response["body"].as_str().unwrap()).unwrap(),
            b"<title>Local</title>"
        );
        assert!(response["headers"].to_string().contains("text/html"));
        let alias = file_entry("file:///workspace/Gr%C3%BC%C3%9Fe%20100%25.html?v=1#section")
            .unwrap()
            .unwrap();
        assert_eq!(alias.path, page.path);
        assert_eq!(alias.url.query(), Some("v=1"));
        assert_eq!(alias.url.fragment(), Some("section"));
        assert_eq!(
            serve(
                &workspace,
                &json!({"url":page.url.as_str(),"method":"HEAD"})
            )
            .await
            .unwrap()["body"],
            ""
        );
        assert_eq!(
            serve(
                &workspace,
                &json!({"url":page.url.as_str(),"method":"POST","body":"changed"})
            )
            .await
            .unwrap()["status"],
            405
        );
        assert_eq!(
            serve(
                &workspace,
                &json!({"url":format!("{ORIGIN}/missing"),"method":"GET"})
            )
            .await
            .unwrap()["status"],
            404
        );
        for path in ["outside", "%2e%2e%2fetc/passwd", "%5cetc%5cpasswd", "%00"] {
            assert!(
                serve(
                    &workspace,
                    &json!({"url":format!("{ORIGIN}/{path}"),"method":"GET"})
                )
                .await
                .is_err(),
                "{path}"
            );
        }
        for path in ["../outside", "/etc/passwd"] {
            assert!(entry(path).is_err());
        }
        for url in [
            "file:///etc/passwd",
            "file://host/workspace/index.html",
            "file:///workspace/%2e%2e/etc/passwd",
        ] {
            assert!(file_entry(url).is_err(), "{url}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Grüße 100%.html")).unwrap(),
            "<title>Local</title>"
        );
    }
}
