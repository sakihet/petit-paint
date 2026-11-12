//! Runs `petit-paint mcp` and talks to it over stdio as an MCP client would.

use rmcp::serde_json::{self, Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    last_id: u64,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_petit-paint"))
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("petit-paint starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            last_id: 0,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Sends a request and returns its result.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.last_id += 1;
        let id = self.last_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).expect("a JSON-RPC response");
        assert_eq!(response["id"], id, "{response}");
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method} failed: {response}"))
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": arguments}))
    }

    fn initialize(&mut self) -> Value {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "petit-paint-test", "version": "0"},
            }),
        );
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        result
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // The server stops when its input closes; kill it if it has not
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn draws_views_and_saves_over_stdio() {
    let mut server = Server::start();
    let info = server.initialize();
    assert_eq!(info["serverInfo"]["name"], "petit-paint");
    assert!(info["capabilities"]["tools"].is_object());

    let tools = server.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in [
        "new_image",
        "open_image",
        "save_image",
        "view_image",
        "draw",
        "draw_stroke",
        "draw_shape",
        "fill",
        "draw_text",
        "undo",
    ] {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }

    let created = server.call(
        "new_image",
        json!({"width": 64, "height": 48, "background": "#ffffff"}),
    );
    assert_eq!(text(&created), "Created image 1, 64×48");
    let drew = server.call(
        "draw",
        json!({"image": 1, "items": [
            {"type": "shape", "shape": "rectangle", "points": [[8, 8], [56, 40]], "fill": "#ff0000"},
            {"type": "stroke", "points": [[0, 24], [64, 24]], "color": "#0000ff"},
        ]}),
    );
    assert_eq!(text(&drew), "Drew 2 items on image 1");
    assert_eq!(
        text(&server.call("pick_color", json!({"image": 1, "point": [12, 12]}))),
        "#ff0000ff"
    );

    let viewed = server.call("view_image", json!({"image": 1}));
    let picture = &viewed["content"][1];
    assert_eq!(picture["type"], "image");
    assert_eq!(picture["mimeType"], "image/png");
    assert!(!picture["data"].as_str().unwrap().is_empty());

    let dir = std::env::temp_dir().join(format!("petit-paint-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("drawing.png");
    server.call("save_image", json!({"image": 1, "path": path}));
    let saved = image::open(&path).unwrap().into_rgba8();
    assert_eq!(saved.dimensions(), (64, 48));
    assert_eq!(saved.get_pixel(12, 12).0, [255, 0, 0, 255]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reports_bad_input_as_tool_errors() {
    let mut server = Server::start();
    server.initialize();
    let missing = server.call("view_image", json!({"image": 9}));
    assert_eq!(missing["isError"], true);
    assert!(text(&missing).contains("no image 9"), "{missing}");

    server.call("new_image", json!({"width": 10, "height": 10}));
    let bad = server.call(
        "draw",
        json!({"image": 1, "items": [
            {"type": "stroke", "points": [[1, 1]], "color": "red"},
        ]}),
    );
    assert_eq!(bad["isError"], true);
    assert!(text(&bad).starts_with("item 1:"), "{bad}");
}
