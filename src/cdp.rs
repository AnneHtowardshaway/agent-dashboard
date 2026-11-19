//! Loopback Chrome DevTools Protocol bridge.
//! Discovers Electron hosts that expose a remote-debugging port, injects the
//! shared inline widget, and serves metrics/preferences over a runtime binding
//! so no CORS or mixed-content rules can block the renderer.

use crate::json::{obj, parse, to_string, Value};
use crate::paths;
use crate::providers::{self, HostSnapshot};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const INJECT_BUNDLE: &str = include_str!("../compat/inject-bundle.js");

#[allow(dead_code)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub ws_url: String,
    pub url: String,
}

pub fn debug_ports() -> Vec<(u16, String)> {
    let mut found = Vec::new();
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid,command"])
        .output()
        .or_else(|_| std::process::Command::new("ps").args(["-eo", "pid,command"]).output());
    if let Ok(output) = output {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let marker = "--remote-debugging-port=";
            if let Some(index) = line.find(marker) {
                let digits: String = line[index + marker.len()..]
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect();
                if let Ok(port) = digits.parse::<u16>() {
                    if port > 0 && !found.iter().any(|(existing, _)| *existing == port) {
                        found.push((port, line.to_string()));
                    }
                }
            }
        }
    }
    if let Some(extra) = std::env::var_os("AG_PULSE_CDP_PORTS") {
        for part in extra.to_string_lossy().split(',') {
            if let Ok(port) = part.trim().parse::<u16>() {
                match found.iter_mut().find(|(existing, _)| *existing == port) {
                    // 显式端口兼容自定义安装路径，已识别的其他宿主仍按原归属过滤。
                    Some((_, command)) if host_from_command(command).is_none() => command.clear(),
                    Some(_) => {}
                    None => found.push((port, String::new())),
                }
            }
        }
    }
    found
}

fn host_from_command(command: &str) -> Option<&'static str> {
    // 只用可执行文件判断归属，避免将其他应用参数中的宿主名误认作进程身份。
    let executable = command.split(" --").next().unwrap_or("").trim_end();
    if executable.ends_with("/Contents/MacOS/ChatGPT")
        || executable.ends_with("/Contents/MacOS/Codex") {
        Some("codex")
    } else if executable.contains("/Antigravity") && executable.contains(".app/Contents/MacOS/") {
        Some("agy")
    } else {
        None
    }
}

/// 显式配置端口时可能没有进程信息；自动发现时只接受归属明确的进程。
fn accepts_port(host: &str, command: &str) -> bool {
    command.is_empty() || host_from_command(command) == Some(host)
}

fn accepts_page(host: &str, url: &str) -> bool {
    let eligible = url.starts_with("app://") || (host == "codex" && url.starts_with("codex://"));
    eligible && !url.contains("avatar-overlay") && !url.contains("detached-window")
}

pub fn discover_all(host: &str) -> Vec<Target> {
    let mut found: Vec<Target> = Vec::new();
    for (port, command) in debug_ports() {
        if !accepts_port(host, &command) {
            continue;
        }
        let list = match targets(port) {
            Ok(list) => list,
            Err(_) => continue,
        };
        for target in list {
            if target.kind != "page" {
                continue;
            }
            let url = target.url;
            if !accepts_page(host, &url) {
                continue;
            }
            if found.iter().any(|existing| existing.ws_url == target.ws) {
                continue;
            }
            found.push(Target { host: host.to_string(), port, ws_url: target.ws, url });
        }
    }
    found
}

struct PageTarget {
    kind: String,
    url: String,
    ws: String,
}

fn targets(port: u16) -> Result<Vec<PageTarget>, String> {
    let body = http_get(port, "/json/list")?;
    let document = parse(&body).map_err(|error| format!("invalid target list: {}", error))?;
    let items = document.as_arr().ok_or_else(|| "target list is not an array".to_string())?;
    Ok(items
        .iter()
        .filter_map(|item| {
            Some(PageTarget {
                kind: item.get("type")?.as_str()?.to_string(),
                url: item.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                ws: item.get("webSocketDebuggerUrl")?.as_str()?.to_string(),
            })
        })
        .collect())
}

fn http_get(port: u16, path: &str) -> Result<String, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(900))
        .map_err(|error| format!("connect failed: {}", error))?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
        path, port
    );
    stream.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("target endpoint closed early".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1024 * 1024 {
            return Err("target response too large".to_string());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
    if !head.starts_with("http/1.1 200") && !head.starts_with("http/1.0 200") {
        return Err("target endpoint did not return 200".to_string());
    }
    let mut length: Option<usize> = None;
    for line in head.split("\r\n") {
        if let Some(value) = line.strip_prefix("content-length:") {
            length = value.trim().parse().ok();
        }
    }
    let mut body = buffer[header_end..].to_vec();
    if let Some(length) = length {
        while body.len() < length {
            let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(length);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

pub struct Cdp {
    stream: TcpStream,
    next_id: u64,
    events: VecDeque<Value>,
}

impl Cdp {
    pub fn connect(ws_url: &str) -> Result<Cdp, String> {
        let rest = ws_url.strip_prefix("ws://").ok_or_else(|| "only ws:// targets are supported".to_string())?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().map_err(|_| "invalid websocket port".to_string())?),
            None => (authority, 80),
        };
        let address = format!("{}:{}", host, port).parse::<SocketAddr>().map_err(|_| "invalid websocket address".to_string())?;
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(1200))
            .map_err(|error| format!("websocket connect failed: {}", error))?;
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        let key = base64(&random_bytes());
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            path, host, port, key
        );
        stream.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        while !header.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut byte).map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("websocket handshake closed".to_string());
            }
            header.push(byte[0]);
            if header.len() > 16 * 1024 {
                return Err("websocket handshake too large".to_string());
            }
        }
        let head = String::from_utf8_lossy(&header);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(format!("websocket handshake rejected: {}", head.lines().next().unwrap_or("")));
        }
        if std::env::var_os("AG_PULSE_DEBUG").is_some() { eprintln!("handshake ok: {}", head.lines().next().unwrap_or("")); }
        Ok(Cdp { stream, next_id: 1, events: VecDeque::new() })
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = obj(vec![("id", (id as f64).into()), ("method", method.into()), ("params", params)]);
        self.send_text(&to_string(&request))?;
        loop {
            let message = self.read_message(Duration::from_secs(6))?;
            let message = match message {
                Some(message) => message,
                None => continue,
            };
            let document = match parse(&message) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if document.get("id").and_then(Value::as_f64) == Some(id as f64) {
                if let Some(error) = document.get("error") {
                    return Err(to_string(error));
                }
                return Ok(document.get("result").cloned().unwrap_or(Value::Null));
            }
            self.events.push_back(document);
        }
    }

    /// Read one CDP event. Returns Ok(None) on timeout.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Value>, String> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        match self.read_message(timeout) {
            Ok(Some(message)) => Ok(parse(&message).ok()),
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn send_text(&mut self, text: &str) -> Result<(), String> {
        let payload = text.as_bytes();
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x81);
        let mask = random_bytes();
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else if payload.len() <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask[..4]);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        if std::env::var_os("AG_PULSE_DEBUG").is_some() { eprintln!("send {} bytes: {:02x?}", frame.len(), &frame[..frame.len().min(24)]); }
        self.stream.write_all(&frame).map_err(|error| error.to_string())
    }

    fn read_message(&mut self, timeout: Duration) -> Result<Option<String>, String> {
        self.stream.set_read_timeout(Some(timeout)).ok();
        let mut collected: Option<Vec<u8>> = None;
        loop {
            let (fin, opcode, payload) = match self.read_frame() {
                Ok(frame) => frame,
                Err(error) if error == "timeout" || error == "WouldBlock" => return Ok(None),
                Err(error) => return Err(error),
            };
            match opcode {
                0x1 | 0x2 => {
                    if fin {
                        return Ok(Some(String::from_utf8_lossy(&payload).into_owned()));
                    }
                    collected = Some(payload);
                }
                0x0 => {
                    let buffer = collected.get_or_insert_with(Vec::new);
                    buffer.extend_from_slice(&payload);
                    if fin {
                        return Ok(Some(String::from_utf8_lossy(buffer).into_owned()));
                    }
                }
                0x8 => return Err("websocket closed".to_string()),
                0x9 => {
                    self.send_pong(&payload)?;
                }
                0xA => {}
                _ => {}
            }
        }
    }

    fn send_pong(&mut self, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 6);
        frame.push(0x8A);
        let mask = random_bytes();
        frame.push(0x80 | payload.len() as u8);
        frame.extend_from_slice(&mask[..4]);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        self.stream.write_all(&frame).map_err(|error| error.to_string())
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), String> {
        let mut head = [0u8; 2];
        self.stream.read_exact(&mut head).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut {
                "timeout".to_string()
            } else {
                error.to_string()
            }
        })?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let masked = head[1] & 0x80 != 0;
        let mut length = (head[1] & 0x7F) as u64;
        if length == 126 {
            let mut extended = [0u8; 2];
            self.stream.read_exact(&mut extended).map_err(|error| error.to_string())?;
            length = u16::from_be_bytes(extended) as u64;
        } else if length == 127 {
            let mut extended = [0u8; 8];
            self.stream.read_exact(&mut extended).map_err(|error| error.to_string())?;
            length = u64::from_be_bytes(extended);
        }
        if length > 16 * 1024 * 1024 {
            return Err("websocket frame too large".to_string());
        }
        let mut mask = [0u8; 4];
        if masked {
            self.stream.read_exact(&mut mask).map_err(|error| error.to_string())?;
        }
        let mut payload = vec![0u8; length as usize];
        if length > 0 {
            self.stream.read_exact(&mut payload).map_err(|error| error.to_string())?;
        }
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok((fin, opcode, payload))
    }
}

/// Keep every matching page injected. Each target reconnects on its own.
pub fn run_host(host: &str) -> Result<(), String> {
    let targets = discover_all(host);
    if targets.is_empty() {
        return Err(format!("No {} remote-debugging page found", host));
    }
    let mut handles = Vec::new();
    for target in targets {
        handles.push(std::thread::spawn(move || {
            let mut last_error = String::new();
            loop {
                match serve_target(&target.host, &target.ws_url) {
                    Ok(()) => last_error.clear(),
                    Err(error) => {
                        if error != last_error {
                            eprintln!("[{}] {}", target.host, error);
                            last_error = error;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

/// One-shot injection used by the `inject` CLI command.
pub fn inject_once(host: &str) -> Result<(), String> {
    let targets = discover_all(host);
    if targets.is_empty() {
        return Err(format!("No {} remote-debugging page found", host));
    }
    for target in &targets {
        inject_target(host, &target.ws_url)?;
    }
    std::thread::sleep(Duration::from_secs(1));
    Ok(())
}

fn serve_target(host: &str, ws_url: &str) -> Result<(), String> {
    let mut cdp = inject_target(host, ws_url)?;
    loop {
        let event = match cdp.next_event(Duration::from_secs(2)) {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(error) => return Err(error),
        };
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        if method != "Runtime.bindingCalled" {
            continue;
        }
        let params = match event.get("params") {
            Some(value) => value,
            None => continue,
        };
        if params.get("name").and_then(Value::as_str) != Some("__agPulseSend") {
            continue;
        }
        let payload = match params.get("payload").and_then(Value::as_str) {
            Some(value) => value,
            None => continue,
        };
        let request = match parse(payload) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let result = match op {
            "metrics" => metrics(host, &input),
            "preferences" => {
                let file = paths::preferences_file();
                let empty = input.as_arr().map(|items| items.is_empty()).unwrap_or(false);
                if empty || input.is_null() {
                    crate::preferences::read(&file)
                } else {
                    match crate::preferences::update(&file, &input) {
                        Ok(value) => value,
                        Err(error) => obj(vec![("error", error.into())]),
                    }
                }
            }
            _ => obj(vec![("error", format!("Unknown operation '{}'", op).into())]),
        };
        let envelope = obj(vec![("id", id), ("result", result)]);
        let expression = format!("window.__agPulseResolve && window.__agPulseResolve({})", to_string(&Value::from(to_string(&envelope))));
        cdp.call("Runtime.evaluate", obj(vec![("expression", expression.into())]))?;
    }
}

fn inject_target(host: &str, ws_url: &str) -> Result<Cdp, String> {
    let mut cdp = Cdp::connect(ws_url)?;
    cdp.call("Runtime.enable", obj(vec![]))?;
    cdp.call("Page.enable", obj(vec![]))?;
    // A previous daemon may have injected into this page already. Dispose the
    // old instance so the new Runtime binding becomes the only live bridge.
    let _ = cdp.call(
        "Runtime.evaluate",
        obj(vec![(
            "expression",
            "try{window.__agPulseDispose&&window.__agPulseDispose()}catch(e){};window.__agPulseInjected=false".into(),
        )]),
    );
    let _ = cdp.call("Runtime.addBinding", obj(vec![("name", "__agPulseSend".into())]));
    let source = format!("window.__AG_PULSE_HOST__='{}';\n{}", host, INJECT_BUNDLE);
    cdp.call(
        "Page.addScriptToEvaluateOnNewDocument",
        obj(vec![("source", source.clone().into())]),
    )?;
    cdp.call(
        "Runtime.evaluate",
        obj(vec![("expression", source.into()), ("awaitPromise", false.into())]),
    )?;
    println!("[{}] widget injected via {}", host, ws_url);
    Ok(cdp)
}

fn metrics(host: &str, input: &Value) -> Value {
    let conversation = input
        .get("conversationId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && *value != "latest");
    let snapshot: HostSnapshot = providers::collect_snapshot(host, conversation);
    providers::release_memory();
    let mut items = match snapshot.to_json() {
        Value::Obj(items) => items,
        _ => Vec::new(),
    };
    items.push(("preferences".to_string(), crate::preferences::read(&paths::preferences_file())));
    Value::Obj(items)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn random_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut bytes);
        return bytes;
    }
    let seed = providers::now_ms() as u128 ^ (std::process::id() as u128) << 32;
    let mut state = seed;
    for byte in bytes.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *byte = (state >> 33) as u8;
    }
    bytes
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let value = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(value >> 18) as usize & 63] as char);
        out.push(ALPHABET[(value >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(value >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[value as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_discovery_respects_host_and_window() {
        let codex = "12 /Applications/ChatGPT.app/Contents/MacOS/ChatGPT --remote-debugging-port=9222";
        let other = "34 /Applications/Other.app/Contents/MacOS/Other --title=ChatGPT";
        assert!(accepts_port("codex", codex));
        assert!(accepts_port("codex", ""));
        assert!(!accepts_port("codex", other));
        assert!(!accepts_port("agy", codex));
        assert!(accepts_page("codex", "app://-/main"));
        assert!(accepts_page("codex", "codex://main"));
        assert!(!accepts_page("codex", "file:///index.html"));
        assert!(!accepts_page("codex", "app://-/avatar-overlay"));
    }

    #[test]
    fn base64_matches_rfc_examples() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
