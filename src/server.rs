//! Lightweight loopback HTTP server: JSON metrics API plus a static dashboard.

use crate::json::{obj, parse, to_string, Value};
use crate::paths;
use crate::providers::{self, HostSnapshot};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 19527;

pub fn serve(port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| format!("Cannot listen on 127.0.0.1:{}: {}", port, error))?;
    println!("ag-pulse dashboard listening on http://127.0.0.1:{}", port);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    let _ = handle(stream);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream) -> Result<(), String> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 128 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    let params = parse_query(&query);

    if method == "OPTIONS" {
        return respond(&mut stream, 204, "text/plain", "", true);
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(&mut stream, 200, "text/html; charset=utf-8", DASHBOARD, false),
        ("GET", "/api/health") => {
            let payload = obj(vec![
                ("ok", true.into()),
                ("version", env!("CARGO_PKG_VERSION").into()),
                ("pid", (std::process::id() as f64).into()),
            ]);
            respond(&mut stream, 200, "application/json; charset=utf-8", &to_string(&payload), true)
        }
        ("GET", "/api/metrics") => {
            let host = params.get("host").map(String::as_str).unwrap_or("all");
            let conversation = params.get("conversation").map(String::as_str).filter(|value| !value.is_empty());
            let preferences = crate::preferences::read(&paths::preferences_file());
            let payload = if host == "all" {
                let hosts = obj(vec![
                    ("agy", providers::collect_snapshot("agy", conversation).to_json()),
                    ("codex", providers::collect_snapshot("codex", conversation).to_json()),
                ]);
                obj(vec![
                    ("hosts", hosts),
                    ("preferences", preferences),
                    ("serverTime", providers::iso_now().into()),
                ])
            } else {
                let snapshot: HostSnapshot = providers::collect_snapshot(host, conversation);
                let mut items = match snapshot.to_json() {
                    Value::Obj(items) => items,
                    _ => Vec::new(),
                };
                items.push(("preferences".to_string(), preferences));
                Value::Obj(items)
            };
            let body = to_string(&payload);
            let result = respond(&mut stream, 200, "application/json; charset=utf-8", &body, true);
            providers::release_memory();
            result
        }
        ("GET", "/api/conversations") => {
            let hosts = obj(vec![
                ("agy", Value::Arr(providers::list_conversations("agy", 6))),
                ("codex", Value::Arr(providers::list_conversations("codex", 6))),
            ]);
            let payload = obj(vec![("hosts", hosts), ("serverTime", providers::iso_now().into())]);
            let body = to_string(&payload);
            let result = respond(&mut stream, 200, "application/json; charset=utf-8", &body, true);
            providers::release_memory();
            result
        }
        ("GET", "/api/preferences") => {
            let payload = crate::preferences::read(&paths::preferences_file());
            respond(&mut stream, 200, "application/json; charset=utf-8", &to_string(&payload), true)
        }
        ("POST", "/api/preferences") => {
            let input = match parse(&String::from_utf8_lossy(&body)) {
                Ok(value) => value,
                Err(error) => {
                    let payload = obj(vec![("error", format!("Invalid JSON: {}", error).into())]);
                    return respond(&mut stream, 400, "application/json; charset=utf-8", &to_string(&payload), true);
                }
            };
            match crate::preferences::update(&paths::preferences_file(), &input) {
                Ok(value) => respond(&mut stream, 200, "application/json; charset=utf-8", &to_string(&value), true),
                Err(error) => {
                    let payload = obj(vec![("error", error.into())]);
                    respond(&mut stream, 400, "application/json; charset=utf-8", &to_string(&payload), true)
                }
            }
        }
        ("GET", "/favicon.ico") => respond(&mut stream, 204, "text/plain", "", true),
        _ => respond(&mut stream, 404, "text/plain; charset=utf-8", "Not found", true),
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str, cors: bool) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        status,
        reason,
        content_type,
        body.as_bytes().len()
    );
    if cors {
        head.push_str("Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(|error| error.to_string())?;
    stream.write_all(body.as_bytes()).map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(value) => {
                        out.push(value);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const DASHBOARD: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Antigrative Dashboard · Multi-Host</title>
<style>
:root{color-scheme:dark;--bg:#0a0c11;--card:#11141c;--line:#202634;--text:#e8ecf5;--muted:#8892a6;--accent:#6ee7c8;--accent2:#8ab4ff;--warn:#f6c177;--danger:#f2777a}
*{box-sizing:border-box}
body{margin:0;background:radial-gradient(1200px 600px at 20% -10%,#172033 0,transparent 60%),var(--bg);color:var(--text);font:13px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;min-height:100vh}
header{display:flex;align-items:center;justify-content:space-between;padding:18px 24px;border-bottom:1px solid var(--line);position:sticky;top:0;background:rgba(10,12,17,.86);backdrop-filter:blur(12px);z-index:5}
h1{font-size:15px;font-weight:600;margin:0;letter-spacing:.2px}
h1 span{color:var(--muted);font-weight:400;margin-left:8px;font-size:12px}
.status{display:flex;align-items:center;gap:7px;color:var(--muted);font-size:12px}
.dot{width:7px;height:7px;border-radius:50%;background:var(--danger);box-shadow:0 0 0 3px rgba(242,119,122,.15)}
.dot.live{background:var(--accent);box-shadow:0 0 0 3px rgba(110,231,200,.15)}
main{padding:22px 24px 48px;display:grid;gap:18px;grid-template-columns:repeat(auto-fit,minmax(340px,1fr));max-width:1500px;margin:0 auto}
.card{background:linear-gradient(180deg,rgba(255,255,255,.02),transparent 40%),var(--card);border:1px solid var(--line);border-radius:14px;padding:16px 18px 18px;position:relative;overflow:hidden}
.card.offline{opacity:.62}
.head{display:flex;align-items:center;justify-content:space-between;margin-bottom:12px}
.host{display:flex;align-items:center;gap:9px;font-size:13px;font-weight:600}
.badge{font-size:10px;text-transform:uppercase;letter-spacing:.6px;color:var(--muted);border:1px solid var(--line);padding:2px 7px;border-radius:999px}
.badge.live{color:var(--accent);border-color:rgba(110,231,200,.35);background:rgba(110,231,200,.08)}
.hero{display:flex;align-items:flex-end;justify-content:space-between;gap:16px;padding:6px 0 14px;border-bottom:1px solid var(--line)}
.rate{font-size:44px;font-weight:600;letter-spacing:-.03em;line-height:1;font-variant-numeric:tabular-nums}
.rate small{font-size:13px;font-weight:500;color:var(--muted);letter-spacing:0;margin-left:5px}
.rate-label{color:var(--muted);font-size:11px;margin-bottom:6px}
.ring{--pct:0;width:64px;height:64px;border-radius:50%;background:conic-gradient(var(--accent2) calc(var(--pct)*1%),#232a3a 0);display:grid;place-items:center;flex:none}
.ring i{width:48px;height:48px;border-radius:50%;background:var(--card);display:grid;place-items:center;font-style:normal;font-size:12px;font-weight:600;font-variant-numeric:tabular-nums}
.grid{display:grid;grid-template-columns:repeat(3,1fr);gap:10px;padding:13px 0}
.stat{background:#0d1017;border:1px solid #1a2030;border-radius:9px;padding:8px 10px;min-width:0}
.stat b{display:block;font-size:14px;font-weight:600;font-variant-numeric:tabular-nums;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.stat span{display:block;color:var(--muted);font-size:10px;margin-top:2px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.quotas{display:flex;flex-direction:column;gap:9px;padding-top:4px}
.quota{display:grid;grid-template-columns:52px 1fr 74px;align-items:center;gap:10px;font-size:11px;color:var(--muted)}
.quota .track{height:6px;border-radius:999px;background:#1b2231;overflow:hidden}
.quota .fill{height:100%;border-radius:999px;background:linear-gradient(90deg,var(--accent2),var(--accent));width:0}
.quota b{text-align:right;color:var(--text);font-weight:600;font-variant-numeric:tabular-nums}
.empty{color:var(--muted);font-size:11px;padding:10px 0 2px}
.sessions{margin-top:8px;padding-top:10px;border-top:1px solid var(--line)}
.sessions h3{margin:0 0 6px;font-size:10px;color:var(--muted);font-weight:500;text-transform:uppercase;letter-spacing:.6px}
.session{display:grid;grid-template-columns:minmax(0,1fr) auto auto;gap:8px;align-items:center;width:100%;background:#0d1017;border:1px solid #1a2030;border-radius:8px;padding:6px 9px;font-size:11px;color:var(--muted);cursor:pointer;text-align:left}
.session:hover,.session.active{border-color:var(--accent2);color:var(--text)}
.session.active{background:rgba(138,180,255,.08)}
.session .s-title{min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;color:var(--text)}
.session .s-meta{font-variant-numeric:tabular-nums;color:var(--text);white-space:nowrap}
.session .s-time{font-variant-numeric:tabular-nums;white-space:nowrap}
.session .est{color:var(--warn)}
footer{max-width:1500px;margin:0 auto;padding:0 24px 28px;color:var(--muted);font-size:11px}
code{color:#b9c4d8}
</style>
</head>
<body>
<header>
  <h1>Antigrative Dashboard <span>multi-host · agy / codex</span></h1>
  <div class="status"><i class="dot" id="dot"></i><span id="conn">connecting…</span></div>
</header>
<main id="grid"></main>
<footer>数据源：Codex 会话日志与 Antigravity 历史快照。CLI：<code>ag-pulse status</code> · 接口：<code>/api/metrics</code> · <code>/api/conversations</code></footer>
<script>
const hosts=[['agy','Antigravity'],['codex','Codex']];
const conv={agy:null,codex:null};
let sessions={agy:[],codex:[]};
const fmt=n=>typeof n==='number'&&isFinite(n)?n.toLocaleString('en-US',{maximumFractionDigits:1}):'—';
const compact=n=>{if(typeof n!=='number'||!isFinite(n))return '—';const u=[[1e9,'B'],[1e6,'M'],[1e3,'K']];for(const [b,s] of u){if(n>=b)return (n/b).toFixed(n/b<10?2:1)+s;}return String(Math.round(n));};
const esc=v=>String(v??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
function sessionRows(hostId,list,active){
  const rows=(list||[]).map(s=>{
    const est=s.estimated===true;
    const rate=s.latestTps==null?'—':(est?'~':'')+fmt(s.latestTps)+' tok/s';
    const time=String(s.updatedAt||'').slice(11,19);
    return `<button class="session ${s.id===active?'active':''}" data-host="${hostId}" data-id="${esc(s.id)}" title="${esc(s.title||s.id)}"><span class="s-title">${esc(s.title||s.model||s.id)}</span><span class="s-meta">${rate}</span><span class="s-time">${time}</span></button>`;
  });
  const reset=`<button class="session ${active===null?'active':''}" data-host="${hostId}" data-id="latest"><span class="s-title">最新会话</span><span class="s-meta"></span><span class="s-time"></span></button>`;
  return `<div class="sessions"><h3>会话（点击切换窗口数据）</h3>${reset}${rows.join('')}</div>`;
}
function card(host,label,data,list,active){
  const d=data||{},speed=d.speed||null,groups=Array.isArray(d.groups)?d.groups:[];
  const online=d.connection==='live'||d.connection==='stale';
  const ctx=speed?.context||{};
  const pct=typeof ctx.usedFraction==='number'?Math.round(ctx.usedFraction*100):0;
  const latest=speed&&typeof speed.latestTps==='number'?speed.latestTps:(speed&&typeof speed.tps==='number'?speed.tps:null);
  const estimated=Boolean(speed&&speed.estimated===true);
  const prefix=estimated?'~':'';
  const cache=speed?.cache||{},tokens=speed?.tokens||{};
  const quotas=groups.map(g=>{
    const w=g.windows||{};
    return ['5h','weekly'].map(k=>{
      const b=w[k];if(!b||b.available===false)return '';
      const rem=typeof b.remaining==='number'?b.remaining:null;
      return `<div class="quota"><span>${k==='5h'?'5h':'Weekly'}</span><span class="track"><i class="fill" style="width:${rem===null?0:rem*100}%"></i></span><b>${rem===null?'—':(rem*100).toFixed(0)+'%'}</b></div>`;
    }).join('');
  }).join('');
  return `<section class="card ${online?'':'offline'}">
    <div class="head"><div class="host"><span class="badge ${d.connection==='live'?'live':''}">${esc(d.connection||'offline')}</span>${esc(label)}</div><span class="badge">${esc(d.error||((estimated?'estimated · ':'')+(speed?.model||'—')))}</span></div>
    <div class="hero"><div><div class="rate-label">Latest speed</div><div class="rate">${latest===null?'—':prefix+fmt(latest)}<small>tok/s</small></div></div>
      <div class="ring" style="--pct:${pct}"><i>${pct}%</i></div></div>
    <div class="grid">
      <div class="stat"><b>${fmt(speed?.tps)}</b><span>Session TPS</span></div>
      <div class="stat"><b>${fmt(speed?.peakTps)}</b><span>Peak TPS</span></div>
      <div class="stat"><b>${speed&&typeof speed.ttftSeconds==='number'?fmt(speed.ttftSeconds)+'s':'—'}</b><span>Avg TTFT</span></div>
      <div class="stat"><b>${typeof cache.hitRate==='number'?(cache.hitRate*100).toFixed(1)+'%':'—'}</b><span>Cache hit</span></div>
      <div class="stat"><b>${compact(tokens.totalTokens)}</b><span>Total tokens</span></div>
      <div class="stat"><b>${speed?fmt(speed.requests):'—'}</b><span>Requests</span></div>
    </div>
    <div class="quotas">${quotas||'<div class="empty">该宿主暂无可读取的配额窗口</div>'}</div>
    ${sessionRows(host,list,active)}
  </section>`;
}
const grid=document.getElementById('grid');
grid.addEventListener('click',e=>{
  const row=e.target.closest('.session');
  if(!row)return;
  conv[row.dataset.host]=row.dataset.id==='latest'?null:row.dataset.id;
  tick();
});
async function tick(){
  try{
    const [all,lists]=await Promise.all([
      fetch('/api/metrics?host=all',{cache:'no-store'}).then(r=>r.json()),
      fetch('/api/conversations',{cache:'no-store'}).then(r=>r.json()).catch(()=>({hosts:{}})),
    ]);
    for(const [id] of hosts){
      if(!conv[id])continue;
      try{
        const res=await fetch(`/api/metrics?host=${id}&conversation=${encodeURIComponent(conv[id])}`,{cache:'no-store'});
        if(res.ok)all.hosts[id]=await res.json();
      }catch(e){}
    }
    sessions=lists.hosts||sessions;
    grid.innerHTML=hosts.map(([id,label])=>card(id,label,all.hosts?.[id],sessions[id],conv[id])).join('');
    const anyLive=hosts.some(([id])=>all.hosts?.[id]?.connection==='live');
    document.getElementById('dot').className='dot'+(anyLive?' live':'');
    document.getElementById('conn').textContent=anyLive?'live · updated '+new Date().toLocaleTimeString():'waiting for a host session';
  }catch(e){
    document.getElementById('conn').textContent='dashboard offline';
  }
}
tick();setInterval(tick,2000);
</script>
</body>
</html>"##;
