//! `generate_image` — one configured OpenAI GPT Image 2 request that writes a
//! PNG into the current project.

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;
use wisp_llm::ToolSchema;
use wisp_tools::{Tool, ToolEnv, ToolEvent, ToolResult};

const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_BASE64_BYTES: usize = 70 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 50 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

pub struct GenerateImageTool {
    api_url: String,
    api_key: String,
    model: String,
    proxy: Option<String>,
}

impl GenerateImageTool {
    pub fn new(api_url: String, api_key: String, model: String, proxy: Option<String>) -> Self {
        Self {
            api_url,
            api_key,
            model,
            proxy,
        }
    }

    fn api_root(&self) -> String {
        let base = self.api_url.trim().trim_end_matches('/');
        if let Some(root) = base.strip_suffix("/images/generations") {
            root.to_string()
        } else if base == "https://api.openai.com" {
            format!("{base}/v1")
        } else {
            base.to_string()
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/images/generations",
            self.api_root().trim_end_matches('/')
        )
    }

    fn model_endpoint(&self) -> String {
        format!(
            "{}/models/{}",
            self.api_root().trim_end_matches('/'),
            self.model.trim()
        )
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        let mut builder = reqwest::Client::builder()
            .user_agent("wisp-science")
            .timeout(Duration::from_secs(300));
        match self.proxy.as_deref().map(str::trim) {
            None | Some("") => {}
            Some("none") => builder = builder.no_proxy(),
            Some(proxy) => {
                builder = builder.proxy(
                    reqwest::Proxy::all(proxy)
                        .map_err(|error| format!("invalid image-generation proxy: {error}"))?,
                );
            }
        }
        builder
            .build()
            .map_err(|error| format!("image-generation HTTP client: {error}"))
    }

    async fn generate(&self, prompt: &str, size: &str, quality: &str) -> Result<Vec<u8>, String> {
        if !self.model.trim().eq_ignore_ascii_case("gpt-image-2") {
            return Err("only OpenAI gpt-image-2 is supported".into());
        }
        if self.api_key.trim().is_empty() {
            return Err("the assigned image-generation model has no API key".into());
        }
        let mut response = self
            .client()?
            .post(self.endpoint())
            .bearer_auth(self.api_key.trim())
            .json(&json!({
                "model": "gpt-image-2",
                "prompt": prompt,
                "n": 1,
                "size": size,
                "quality": quality,
                "output_format": "png",
            }))
            .send()
            .await
            .map_err(|error| format!("image-generation request failed: {error}"))?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body_limit = if status.is_success() {
            MAX_BASE64_BYTES + MAX_ERROR_BYTES
        } else {
            MAX_ERROR_BYTES
        };
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("image-generation response failed: {error}"))?
        {
            if body.len().saturating_add(chunk.len()) > body_limit {
                return Err("image-generation response is too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(2_048)]).into());
            let request_id = (!request_id.is_empty())
                .then(|| format!(" (request {request_id})"))
                .unwrap_or_default();
            return Err(format!(
                "OpenAI image API returned {}{request_id}: {message}",
                status.as_u16()
            ));
        }
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| format!("invalid image-generation response: {error}"))?;
        let encoded = value
            .pointer("/data/0/b64_json")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "image-generation response has no data[0].b64_json".to_string())?;
        if encoded.len() > MAX_BASE64_BYTES {
            return Err("generated image response is too large".into());
        }
        let image = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("generated image is not valid base64: {error}"))?;
        if image.len() > MAX_IMAGE_BYTES {
            return Err("generated PNG is too large".into());
        }
        if !image.starts_with(PNG_SIGNATURE) {
            return Err("image-generation response is not a PNG".into());
        }
        Ok(image)
    }

    /// Validate credentials and access without creating a billable image.
    ///
    /// Image-only models cannot be sent to Responses or Chat Completions. The
    /// OpenAI model metadata route provides a lightweight authenticated probe.
    pub async fn validate_model_access(&self) -> Result<(), String> {
        if !self.model.trim().eq_ignore_ascii_case("gpt-image-2") {
            return Err("only OpenAI gpt-image-2 is supported".into());
        }
        if self.api_key.trim().is_empty() {
            return Err("the assigned image-generation model has no API key".into());
        }
        let mut response = self
            .client()?
            .get(self.model_endpoint())
            .bearer_auth(self.api_key.trim())
            .send()
            .await
            .map_err(|error| format!("image-generation validation failed: {error}"))?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("image-generation validation response failed: {error}"))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
                return Err("image-generation validation response is too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(2_048)]).into());
            return Err(format!(
                "OpenAI model API returned {}: {message}",
                status.as_u16()
            ));
        }
        let id = serde_json::from_slice::<Value>(&body)
            .map_err(|error| format!("invalid OpenAI model response: {error}"))?
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !id.eq_ignore_ascii_case("gpt-image-2") {
            return Err(format!(
                "OpenAI returned model '{}' while validating gpt-image-2",
                if id.is_empty() { "(missing)" } else { &id }
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for GenerateImageTool {
    fn name(&self) -> &str {
        "generate_image"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "generate_image",
            "Generate one PNG with the configured OpenAI gpt-image-2 model and save it inside the project. This is the Scientific Illustrator's PNG image-model mode. Call it when the user explicitly asks for PNG, gpt-image-2, generate_image, or image-model generation. Also call it for a Scientific Illustrator image request that names no format or method, because the presence of this tool means an image-generation model is configured. Do not call it when the user explicitly asks for SVG, vector, an editable figure, or direct SVG generation; create and visually verify that SVG directly instead. Use a project-relative path under figures/.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Complete self-contained visual brief derived from the user's request and relevant project context"
                    },
                    "path": {
                        "type": "string",
                        "description": "Project-relative output path in the form figures/<descriptive-name>.png"
                    },
                    "size": {
                        "type": "string",
                        "enum": ["auto", "1024x1024", "1536x1024", "1024x1536"],
                        "description": "Output dimensions (default auto)"
                    },
                    "quality": {
                        "type": "string",
                        "enum": ["auto", "low", "medium", "high"],
                        "description": "Rendering quality (default auto)"
                    }
                },
                "required": ["prompt", "path"]
            }),
        )
    }

    fn preview(&self, args: &Value) -> String {
        args.get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    async fn run(&self, args: &Value, env: &dyn ToolEnv) -> ToolResult {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if prompt.is_empty() {
            return ToolResult::fail("generate_image error: prompt is required");
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return ToolResult::fail(format!(
                "generate_image error: prompt exceeds {MAX_PROMPT_BYTES} bytes"
            ));
        }
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !path
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("png"))
        {
            return ToolResult::fail("generate_image error: path must end in .png");
        }
        if let Err(error) = wisp_tools::safety::validate_relative_pattern(path) {
            return ToolResult::fail(format!("generate_image {path} error: {error}"));
        }
        if std::path::Path::new(path).parent() != Some(std::path::Path::new("figures")) {
            return ToolResult::fail(
                "generate_image error: path must be a file directly under figures/",
            );
        }
        if let Err(error) = std::fs::create_dir_all(env.project_root().join("figures")) {
            return ToolResult::fail(format!(
                "generate_image {path} error: cannot create figures directory: {error}"
            ));
        }
        let real = match wisp_tools::safety::validate_file_path(env.project_root(), path) {
            Ok(path) => path,
            Err(error) => {
                return ToolResult::fail(format!("generate_image {path} error: {error}"));
            }
        };
        let size = args.get("size").and_then(Value::as_str).unwrap_or("auto");
        if !matches!(size, "auto" | "1024x1024" | "1536x1024" | "1024x1536") {
            return ToolResult::fail("generate_image error: unsupported size");
        }
        let quality = args
            .get("quality")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        if !matches!(quality, "auto" | "low" | "medium" | "high") {
            return ToolResult::fail("generate_image error: unsupported quality");
        }
        let image = match self.generate(prompt, size, quality).await {
            Ok(image) => image,
            Err(error) => return ToolResult::fail(format!("generate_image error: {error}")),
        };
        if let Err(error) = std::fs::write(&real, &image) {
            return ToolResult::fail(format!("generate_image {path} error: {error}"));
        }
        env.emit(ToolEvent::FileChanged { path: path.into() }).await;
        ToolResult::ok(format!(
            "Generated {} byte PNG at {path}. Include it in the final answer as a Markdown image.",
            image.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn schema_keeps_explicit_svg_requests_out_of_raster_mode() {
        let tool = GenerateImageTool::new(
            "https://api.openai.com/v1".into(),
            "sk-test".into(),
            "gpt-image-2".into(),
            Some("none".into()),
        );
        let description = tool.schema().function.description;

        assert!(description.contains("Do not call it when the user explicitly asks for SVG"));
        assert!(description.contains("explicitly asks for PNG"));
        assert!(description.contains("names no format or method"));
        assert!(description.contains("visually verify that SVG directly"));
    }

    struct RecordingEnv {
        root: PathBuf,
        events: Mutex<Vec<ToolEvent>>,
    }

    #[async_trait]
    impl ToolEnv for RecordingEnv {
        fn project_root(&self) -> &Path {
            &self.root
        }

        async fn confirm(&self, _message: &str) -> bool {
            true
        }

        async fn emit(&self, event: ToolEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    async fn serve_once(response_body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or_default();
            while request.len() < header_end + content_length {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/v1"), handle)
    }

    #[tokio::test]
    async fn generates_png_with_the_openai_image_endpoint() {
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let response = json!({"data": [{"b64_json": encoded}]}).to_string();
        let (api_url, request) = serve_once(response).await;
        let root =
            std::env::temp_dir().join(format!("wisp_generate_image_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let env = RecordingEnv {
            root: root.clone(),
            events: Mutex::new(Vec::new()),
        };
        let result = GenerateImageTool::new(
            api_url,
            "sk-test".into(),
            "gpt-image-2".into(),
            Some("none".into()),
        )
        .run(
            &json!({
                "prompt": "A precise scientific pathway diagram",
                "path": "figures/pathway.png",
                "size": "1536x1024",
                "quality": "low"
            }),
            &env,
        )
        .await;

        assert!(result.success, "{}", result.content);
        assert!(std::fs::read(root.join("figures/pathway.png"))
            .unwrap()
            .starts_with(PNG_SIGNATURE));
        assert!(env.events.lock().unwrap().iter().any(
            |event| matches!(event, ToolEvent::FileChanged { path } if path == "figures/pathway.png")
        ));
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /v1/images/generations HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], "gpt-image-2");
        assert_eq!(body["output_format"], "png");
        assert_eq!(body["size"], "1536x1024");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn validates_with_the_model_endpoint_instead_of_a_chat_endpoint() {
        let response = json!({"id": "gpt-image-2", "object": "model"}).to_string();
        let (api_url, request) = serve_once(response).await;

        GenerateImageTool::new(
            api_url,
            "sk-test".into(),
            "gpt-image-2".into(),
            Some("none".into()),
        )
        .validate_model_access()
        .await
        .unwrap();

        let request = request.await.unwrap();
        assert!(request.starts_with("GET /v1/models/gpt-image-2 HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-test"));
        assert!(!request.contains("/responses"));
        assert!(!request.contains("/chat/completions"));
    }

    #[tokio::test]
    async fn rejects_non_png_paths_before_calling_the_api() {
        let root =
            std::env::temp_dir().join(format!("wisp_generate_image_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let env = RecordingEnv {
            root: root.clone(),
            events: Mutex::new(Vec::new()),
        };
        let result = GenerateImageTool::new(
            "https://api.openai.com/v1".into(),
            "sk-test".into(),
            "gpt-image-2".into(),
            Some("none".into()),
        )
        .run(
            &json!({"prompt": "diagram", "path": "figures/pathway.svg"}),
            &env,
        )
        .await;
        assert!(!result.success);
        assert!(result.content.contains("path must end in .png"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_paths_outside_the_figures_directory() {
        let root =
            std::env::temp_dir().join(format!("wisp_generate_image_path_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let env = RecordingEnv {
            root: root.clone(),
            events: Mutex::new(Vec::new()),
        };
        let tool = GenerateImageTool::new(
            "http://127.0.0.1:9/v1".into(),
            "sk-test".into(),
            "gpt-image-2".into(),
            Some("none".into()),
        );

        for path in ["plot.png", "../figures/plot.png", "figures/sub/plot.png"] {
            let result = tool
                .run(&json!({"prompt": "diagram", "path": path}), &env)
                .await;
            assert!(!result.success, "{path} should be rejected");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
