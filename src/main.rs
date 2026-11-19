//! ag-pulse: ultra-low-memory multi-host metrics collector and CDP bridge.

mod cdp;
mod json;
mod paths;
mod preferences;
mod providers;
mod server;

use json::to_string;
use providers::HostSnapshot;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const HOSTS: [&str; 2] = ["agy", "codex"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ag-pulse: {}", error);
            1
        }
    };
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<i32, String> {
    let command = args.first().map(String::as_str).unwrap_or("status");
    let options = Options::parse(&args[1..]);
    match command {
        "status" => status(&options),
        "serve" => {
            server::serve(options.port)?;
            Ok(0)
        }
        "daemon" => daemon(&options),
        "inject" => inject(&options),
        "launch" => launch(&options),
        "version" | "--version" | "-V" => {
            println!("ag-pulse {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(0)
        }
        other => Err(format!("unknown command '{}'; run ag-pulse help", other)),
    }
}

struct Options {
    host: String,
    conversation: Option<String>,
    port: u16,
    json: bool,
    foreground: bool,
    subcommand: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Options {
        let mut options = Options {
            host: "all".to_string(),
            conversation: None,
            port: server::DEFAULT_PORT,
            json: false,
            foreground: false,
            subcommand: None,
        };
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "--host" => {
                    index += 1;
                    if let Some(value) = args.get(index) {
                        options.host = value.clone();
                    }
                }
                "--conversation" => {
                    index += 1;
                    options.conversation = args.get(index).cloned();
                }
                "--port" => {
                    index += 1;
                    if let Some(value) = args.get(index).and_then(|v| v.parse().ok()) {
                        options.port = value;
                    }
                }
                "--json" => options.json = true,
                "--foreground" => options.foreground = true,
                other if !other.starts_with('-') && options.subcommand.is_none() => {
                    options.subcommand = Some(other.to_string());
                }
                _ => {}
            }
            index += 1;
        }
        options
    }
}

fn collect(host: &str, conversation: Option<&str>) -> HostSnapshot {
    providers::collect_snapshot(host, conversation)
}

fn status(options: &Options) -> Result<i32, String> {
    let hosts: Vec<&str> = if options.host == "all" {
        HOSTS.to_vec()
    } else if HOSTS.contains(&options.host.as_str()) {
        vec![options.host.as_str()]
    } else {
        return Err(format!("unknown host '{}'", options.host));
    };
    let snapshots: Vec<HostSnapshot> = hosts
        .iter()
        .map(|host| collect(host, options.conversation.as_deref()))
        .collect();
    if options.json {
        let value = json::obj(vec![(
            "hosts",
            json::Value::Obj(
                snapshots
                    .iter()
                    .map(|snapshot| (snapshot.host.clone(), snapshot.to_json()))
                    .collect(),
            ),
        )]);
        println!("{}", to_string(&value));
        return Ok(0);
    }
    println!(
        "{:<9} {:<6} {:>11} {:>11} {:>10} {:>9} {:>6}  {}",
        "HOST", "STATE", "LATEST", "SESSION", "PEAK", "CACHE", "CTX", "DETAIL"
    );
    for snapshot in &snapshots {
        let speed = snapshot.speed.as_ref();
        let latest = speed.and_then(|s| s.latest_tps.or(s.tps));
        let session = speed.and_then(|s| s.tps);
        let peak = speed.and_then(|s| s.peak_tps);
        let cache = speed.and_then(|s| s.cache.hit_rate);
        let ctx = speed.and_then(|s| s.context.used_tokens.zip(s.context.max_tokens)).map(|(used, max)| used / max);
        println!(
            "{:<9} {:<6} {:>11} {:>11} {:>10} {:>9} {:>6}  {}",
            snapshot.host,
            snapshot.connection,
            format_rate(latest),
            format_rate(session),
            format_rate(peak),
            cache.map(|value| format!("{:.1}%", value * 100.0)).unwrap_or_else(|| "—".to_string()),
            ctx.map(|value| format!("{:.0}%", value * 100.0)).unwrap_or_else(|| "—".to_string()),
            snapshot
                .speed
                .as_ref()
                .and_then(|s| s.model.clone())
                .or_else(|| snapshot.error.clone())
                .unwrap_or_else(|| "—".to_string())
        );
    }
    Ok(0)
}

fn format_rate(value: Option<f64>) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{:.1} tok/s", value),
        _ => "—".to_string(),
    }
}

fn daemon(options: &Options) -> Result<i32, String> {
    match options.subcommand.as_deref() {
        Some("stop") => return daemon_stop(),
        Some("status") => return daemon_status(),
        Some("start") | None => {
            if options.foreground {
                return daemon_foreground(options.port);
            }
            return daemon_start(options.port);
        }
        Some(other) => return Err(format!("unknown daemon action '{}'", other)),
    }
}

fn daemon_start(port: u16) -> Result<i32, String> {
    if let Some(pid) = running_pid() {
        println!("ag-pulse daemon already running (pid {})", pid);
        return Ok(0);
    }
    let directory = paths::run_dir();
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let log_dir = paths::log_file().parent().map(PathBuf::from);
    if let Some(log_dir) = log_dir {
        fs::create_dir_all(log_dir).map_err(|error| error.to_string())?;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file())
        .map_err(|error| error.to_string())?;
    let error_log = log.try_clone().map_err(|error| error.to_string())?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    // Detach from the controlling terminal; a plain spawn receives SIGHUP
    // when the invoking shell exits.
    let mut command = std::process::Command::new("nohup");
    command
        .arg(executable)
        .arg("daemon")
        .arg("--foreground")
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(error_log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        unsafe {
            command.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|error| format!("cannot start daemon: {}", error))?;
    fs::write(paths::pid_file(), child.id().to_string()).map_err(|error| error.to_string())?;
    println!("ag-pulse daemon started (pid {}) on http://127.0.0.1:{}", child.id(), port);
    Ok(0)
}

fn daemon_foreground(port: u16) -> Result<i32, String> {
    let directory = paths::run_dir();
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    fs::write(paths::pid_file(), std::process::id().to_string()).map_err(|error| error.to_string())?;
    println!("ag-pulse daemon {} on http://127.0.0.1:{}", std::process::id(), port);
    thread::spawn(move || {
        if let Err(error) = server::serve(port) {
            eprintln!("server stopped: {}", error);
        }
    });
    thread::spawn(|| inject_loop("codex"));
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

fn inject_loop(host: &'static str) {
    let mut last_error = String::new();
    loop {
        match cdp::run_host(host) {
            Ok(()) => {
                last_error.clear();
            }
            Err(error) => {
                if error != last_error {
                    eprintln!("[{}] {}", host, error);
                    last_error = error;
                }
            }
        }
        thread::sleep(Duration::from_secs(5));
    }
}

fn daemon_stop() -> Result<i32, String> {
    let pid = match running_pid() {
        Some(pid) => pid,
        None => {
            println!("ag-pulse daemon is not running");
            let _ = fs::remove_file(paths::pid_file());
            return Ok(0);
        }
    };
    let status = std::process::Command::new("kill").arg(pid.to_string()).status();
    match status {
        Ok(status) if status.success() => {
            let _ = fs::remove_file(paths::pid_file());
            println!("ag-pulse daemon stopped (pid {})", pid);
            Ok(0)
        }
        _ => Err(format!("could not stop daemon pid {}", pid)),
    }
}

fn daemon_status() -> Result<i32, String> {
    match running_pid() {
        Some(pid) => {
            println!("ag-pulse daemon running (pid {})", pid);
            Ok(0)
        }
        None => {
            println!("ag-pulse daemon is not running");
            Ok(1)
        }
    }
}

fn running_pid() -> Option<u32> {
    let text = fs::read_to_string(paths::pid_file()).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if alive {
        Some(pid)
    } else {
        None
    }
}

fn inject(options: &Options) -> Result<i32, String> {
    let host = match options.host.as_str() {
        "all" | "codex" => "codex",
        "agy" => "agy",
        other => return Err(format!("unknown host '{}'", other)),
    };
    cdp::inject_once(host)?;
    println!("[{}] injected", host);
    Ok(0)
}

fn launch(options: &Options) -> Result<i32, String> {
    let port = if options.port == server::DEFAULT_PORT { 19528 } else { options.port };
    match options.host.as_str() {
        "codex" => {
            let status = std::process::Command::new("open")
                .args(["-na", "ChatGPT", "--args", &format!("--remote-debugging-port={}", port)])
                .status()
                .map_err(|error| format!("cannot launch Codex: {}", error))?;
            Ok(if status.success() { 0 } else { 1 })
        }
        other => Err(format!("launch supports codex, not '{}'", other)),
    }
}

fn print_help() {
    println!(
        "ag-pulse {}\n\
         \n\
         USAGE\n\
           ag-pulse status [--host all|agy|codex] [--conversation ID] [--json]\n\
           ag-pulse serve [--port 19527]\n\
           ag-pulse daemon [start|stop|status] [--port 19527]\n\
           ag-pulse inject [--host codex|agy|all]\n\
           ag-pulse launch --host codex [--port 19528]\n",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_hosts_and_retired_commands() {
        assert_eq!(HOSTS, ["agy", "codex"]);
        for command in ["status", "inject", "launch"] {
            let args = [command, "--host", "qwenwork"].map(str::to_owned);
            assert!(run(&args).is_err());
        }
    }
}
