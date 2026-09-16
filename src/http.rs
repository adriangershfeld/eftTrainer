//! Localhost HTTP control + introspection server, so the trainer can be driven
//! and its live IL2CPP metadata + process memory queried with curl (Desktop
//! Commander) instead of static analysis.
//!
//! Loopback only (127.0.0.1). Only operations that are safe off the main thread
//! are exposed: metadata walks (static tables), raw memory reads (guarded, no
//! access violation), and feature toggles that set an atomic the main-thread
//! driver acts on. Nothing here invokes managed code.
//!
//! Endpoints (all GET; curl-friendly). Numbers accept 0x-hex or decimal:
//!   /                              help
//!   /status                        backend, domain, module bases, chams state
//!   /module?name=GameAssembly.dll  base/end/size of a loaded module
//!   /class?asm=&ns=&name=          class ptr + field/method counts
//!   /fields?asm=&ns=&name=[&q=]    [{name, offset}] (walks bases), q filters
//!   /methods?asm=&ns=&name=[&q=]   [{name, argc, static, ptr}] (ptr = hook addr)
//!     (class=0x.. may replace asm/ns/name on class/fields/methods)
//!   /read?addr=0x..&len=N          hex + typed interpretations at addr
//!   /chain?addr=0x..&path=0x10,0x8 follow pointer offsets, per-step values
//!   /scan?hex=DEADBEEF | i32= | u32= | i64= | u64= | f32= | f64= | str= | utf16=
//!         [&mod=GameAssembly.dll | &start=&end=][&max=N]   memory search
//!   /chams?on=1 | on=0 | scheme=N  toggle chams / pick scheme (else status)

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::il2cpp;
use crate::runtime::{self, Class, Domain, Object, ScriptRuntime};
use crate::symbols::{self, eft};

const ADDR: &str = "127.0.0.1:28282";
const DEFAULT_SCAN_BYTES: usize = 1 << 30; // 1 GiB
const DEFAULT_SCAN_HITS: usize = 64;
const MAX_READ_LEN: usize = 4096;

pub fn start(domain_raw: usize) -> JoinHandle<()> {
    std::thread::spawn(move || run(domain_raw))
}

fn run(domain_raw: usize) {
    let listener = match TcpListener::bind(ADDR) {
        Ok(l) => l,
        Err(e) => {
            crate::elog!("[http] bind {ADDR} failed: {e}");
            return;
        }
    };
    let _ = listener.set_nonblocking(true);
    crate::elog!("[http] listening on http://{ADDR}");

    // Metadata reads want an attached thread; idempotent.
    if let Some(rt) = runtime::get() {
        unsafe { rt.attach_thread(Domain(domain_raw as *mut c_void)) };
    }

    loop {
        if crate::control::aborted() {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => handle(stream, domain_raw),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    crate::elog!("[http] stopped");
}

// ── request / response plumbing ───────────────────────────────────────────────

fn handle(mut stream: TcpStream, domain_raw: usize) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));

    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16384 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let req = String::from_utf8_lossy(&buf);
    let first = req.lines().next().unwrap_or("");
    let target = first.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let q = parse_query(query);

    let (status, ctype, body) = route(path, &q, domain_raw);
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(url_decode(k), url_decode(v));
    }
    m
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let h = hex_nibble(b[i + 1]);
                let l = hex_nibble(b[i + 2]);
                match (h, l) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ── number / json helpers ─────────────────────────────────────────────────────

fn parse_usize(s: &str) -> Option<usize> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        usize::from_str_radix(h, 16).ok()
    } else {
        s.parse::<usize>().ok()
    }
}

fn parse_i64(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else {
        s.parse::<i64>().ok()
    }
}

/// JSON-escaped, quoted string.
fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn ok(body: String) -> (&'static str, &'static str, String) {
    ("200 OK", "application/json", body)
}
fn err(msg: &str) -> (&'static str, &'static str, String) {
    ("400 Bad Request", "application/json", format!("{{\"error\":{}}}", jstr(msg)))
}

// ── routing ───────────────────────────────────────────────────────────────────

fn route(
    path: &str,
    q: &HashMap<String, String>,
    domain_raw: usize,
) -> (&'static str, &'static str, String) {
    match path {
        "/" => ("200 OK", "text/plain; charset=utf-8", HELP.to_string()),
        "/status" => ep_status(domain_raw),
        "/module" => ep_module(q),
        "/class" => ep_class(q, domain_raw),
        "/fields" => ep_fields(q, domain_raw),
        "/methods" => ep_methods(q, domain_raw),
        "/read" => ep_read(q),
        "/chain" => ep_chain(q),
        "/scan" => ep_scan(q),
        "/gameworld" => ep_gameworld(),
        "/players" => ep_players(domain_raw),
        "/chams" => ep_chams(q),
        "/raid" => ep_raid(q),
        "/stamina" => ep_stamina(q),
        "/speed" => ep_speed(q),
        "/menu" => ep_menu(q),
        _ => err("unknown endpoint (GET / for help)"),
    }
}

const HELP: &str = "eftTrainer HTTP control\n\
/status\n\
/module?name=GameAssembly.dll\n\
/class?asm=Assembly-CSharp&ns=EFT&name=Player\n\
/fields?asm=Assembly-CSharp&ns=EFT&name=Player[&q=substr]\n\
/methods?asm=UnityEngine.CoreModule&ns=UnityEngine&name=Renderer[&q=substr]\n\
  (class=0x.. may replace asm/ns/name)\n\
/read?addr=0x..&len=64\n\
/chain?addr=0x..&path=0x230,0x9d8,0x68\n\
/scan?hex=DEADBEEF | i32= | u32= | i64= | u64= | f32= | f64= | str= | utf16=\n\
      [&mod=GameAssembly.dll | &start=0x..&end=0x..][&max=64]\n\
/gameworld                     live GameWorld pointer (main-thread captured)\n\
/players                       live player pointers from GameWorld\n\
/chams?on=1 | on=0 | scheme=2 | style=flat|glow|mesh\n\
/raid?on=1                     enable framework; returns players (local/bot/pos/screen/dist)\n\
/stamina?on=1 | on=0           infinite stamina\n\
/speed?on=1&mult=1.3           speedhack toggle + multiplier (1.0-1.4)\n\
/menu?visible=1 | visible=0     show/hide the overlay (drives the same toggle as INSERT)\n\
numbers accept 0x-hex or decimal\n";

fn domain(domain_raw: usize) -> Domain {
    Domain(domain_raw as *mut c_void)
}

/// Resolve a class from `class=0x..` or `asm`/`ns`/`name` (ns defaults to "").
unsafe fn resolve_class(
    rt: &dyn ScriptRuntime,
    dom: Domain,
    q: &HashMap<String, String>,
) -> Result<Class, String> {
    if let Some(p) = q.get("class").and_then(|s| parse_usize(s)) {
        return Ok(Class(p as *mut c_void));
    }
    let asm = q.get("asm").ok_or("need asm= or class=")?;
    let name = q.get("name").ok_or("need name= or class=")?;
    let ns = q.get("ns").map(|s| s.as_str()).unwrap_or("");
    let img = unsafe { rt.image(dom, asm) }.ok_or_else(|| format!("assembly not found: {asm}"))?;
    unsafe { rt.class(img, ns, name) }.ok_or_else(|| format!("class not found: {ns}.{name}"))
}

// ── endpoints ─────────────────────────────────────────────────────────────────

fn ep_status(domain_raw: usize) -> (&'static str, &'static str, String) {
    let backend = runtime::get().map(|r| r.backend().name()).unwrap_or("none");
    let ga = unsafe { il2cpp::module_bounds("GameAssembly.dll") };
    let up = unsafe { il2cpp::module_bounds("UnityPlayer.dll") };
    let (con, sch, sname, scount, style) = crate::chams::status();
    let mods = |m: Option<(usize, usize)>| match m {
        Some((b, e)) => format!("{{\"base\":\"{b:#x}\",\"end\":\"{e:#x}\"}}"),
        None => "null".to_string(),
    };
    let (of_last, of_max, of_avg) = crate::menu::onframe_us();
    let (cur_x, cur_y) = crate::menu::cursor();
    ok(format!(
        "{{\"backend\":{},\"domain\":\"{:#x}\",\"gameassembly\":{},\"unityplayer\":{},\
         \"menu\":{{\"visible\":{},\"frames\":{},\"fps\":{},\"cursor\":[{},{}],\
         \"onframe_us\":{{\"last\":{},\"max\":{},\"avg\":{}}}}},\
         \"chams\":{{\"on\":{},\"scheme\":{},\"name\":{},\"count\":{},\"style\":{}}}}}",
        jstr(backend),
        domain_raw,
        mods(ga),
        mods(up),
        crate::menu::is_visible(),
        crate::menu::frame_count(),
        crate::menu::fps(),
        cur_x,
        cur_y,
        of_last,
        of_max,
        of_avg,
        con,
        sch,
        jstr(sname),
        scount,
        jstr(style)
    ))
}

fn ep_module(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    let Some(name) = q.get("name") else { return err("need name=") };
    match unsafe { il2cpp::module_bounds(name) } {
        Some((b, e)) => ok(format!(
            "{{\"name\":{},\"base\":\"{b:#x}\",\"end\":\"{e:#x}\",\"size\":{}}}",
            jstr(name),
            e - b
        )),
        None => err("module not loaded"),
    }
}

fn ep_class(q: &HashMap<String, String>, domain_raw: usize) -> (&'static str, &'static str, String) {
    let Some(rt) = runtime::get() else { return err("runtime not ready") };
    let dom = domain(domain_raw);
    let cls = match unsafe { resolve_class(rt, dom, q) } {
        Ok(c) => c,
        Err(e) => return err(&e),
    };
    let name = unsafe { rt.class_name(cls) };
    let nf = unsafe { rt.dump_fields(cls) }.len();
    let nm = unsafe { rt.dump_methods(cls) }.len();
    ok(format!(
        "{{\"ptr\":\"{:#x}\",\"name\":{},\"fields\":{},\"methods\":{}}}",
        cls.raw() as usize,
        jstr(&name),
        nf,
        nm
    ))
}

fn ep_fields(q: &HashMap<String, String>, domain_raw: usize) -> (&'static str, &'static str, String) {
    let Some(rt) = runtime::get() else { return err("runtime not ready") };
    let dom = domain(domain_raw);
    let cls = match unsafe { resolve_class(rt, dom, q) } {
        Ok(c) => c,
        Err(e) => return err(&e),
    };
    let filt = q.get("q").map(|s| s.to_lowercase());
    let items: Vec<String> = unsafe { rt.dump_fields(cls) }
        .into_iter()
        .filter(|(n, _)| filt.as_ref().map(|f| n.to_lowercase().contains(f)).unwrap_or(true))
        .map(|(n, off)| format!("{{\"name\":{},\"offset\":{},\"offset_hex\":\"{:#x}\"}}", jstr(&n), off, off))
        .collect();
    ok(format!("[{}]", items.join(",")))
}

fn ep_methods(q: &HashMap<String, String>, domain_raw: usize) -> (&'static str, &'static str, String) {
    let Some(rt) = runtime::get() else { return err("runtime not ready") };
    let dom = domain(domain_raw);
    let cls = match unsafe { resolve_class(rt, dom, q) } {
        Ok(c) => c,
        Err(e) => return err(&e),
    };
    let filt = q.get("q").map(|s| s.to_lowercase());
    let items: Vec<String> = unsafe { rt.dump_methods(cls) }
        .into_iter()
        .filter(|(n, ..)| filt.as_ref().map(|f| n.to_lowercase().contains(f)).unwrap_or(true))
        .map(|(n, argc, is_static, ptr)| {
            format!(
                "{{\"name\":{},\"argc\":{},\"static\":{},\"ptr\":\"{:#x}\"}}",
                jstr(&n),
                argc,
                is_static,
                ptr
            )
        })
        .collect();
    ok(format!("[{}]", items.join(",")))
}

fn ep_read(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    let Some(addr) = q.get("addr").and_then(|s| parse_usize(s)) else { return err("need addr=") };
    let mut len = q.get("len").and_then(|s| parse_usize(s)).unwrap_or(64).clamp(1, MAX_READ_LEN);

    // Shrink to a readable window so a bad addr never faults. Fresh
    // VirtualQuery, not the calibration snapshot, since addr is arbitrary.
    while len > 0 && !unsafe { il2cpp::readable_now(addr as *const u8, len) } {
        len /= 2;
    }
    if len == 0 {
        return err("address not readable");
    }
    let slice = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };

    let mut hex = String::with_capacity(len * 2);
    let mut ascii = String::with_capacity(len);
    for &b in slice {
        hex.push_str(&format!("{b:02X}"));
        ascii.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
    }

    let le4 = |o: usize| -> Option<[u8; 4]> { slice.get(o..o + 4)?.try_into().ok() };
    let le8 = |o: usize| -> Option<[u8; 8]> { slice.get(o..o + 8)?.try_into().ok() };
    let mut typed = String::new();
    if let Some(b) = le4(0) {
        typed.push_str(&format!(
            ",\"i32\":{},\"u32\":{},\"f32\":{}",
            i32::from_le_bytes(b),
            u32::from_le_bytes(b),
            f32::from_le_bytes(b)
        ));
    }
    if let Some(b) = le8(0) {
        typed.push_str(&format!(
            ",\"i64\":{},\"u64\":{},\"f64\":{},\"ptr\":\"{:#x}\"",
            i64::from_le_bytes(b),
            u64::from_le_bytes(b),
            f64::from_le_bytes(b),
            u64::from_le_bytes(b)
        ));
    }
    ok(format!(
        "{{\"addr\":\"{addr:#x}\",\"len\":{len},\"hex\":{},\"ascii\":{}{typed}}}",
        jstr(&hex),
        jstr(&ascii)
    ))
}

fn ep_chain(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    let Some(mut cur) = q.get("addr").and_then(|s| parse_usize(s)) else { return err("need addr=") };
    let Some(path) = q.get("path") else { return err("need path=0x10,0x8,..") };
    let offs: Vec<usize> = path.split(',').filter_map(parse_usize).collect();
    if offs.is_empty() {
        return err("empty path");
    }

    let mut steps: Vec<String> = Vec::new();
    for off in offs {
        let at = cur.wrapping_add(off);
        let ptr = unsafe { il2cpp::read_now::<usize>(at as *const u8) };
        let f32v = unsafe { il2cpp::read_now::<f32>(at as *const u8) };
        let i64v = unsafe { il2cpp::read_now::<i64>(at as *const u8) };
        steps.push(format!(
            "{{\"off\":\"{off:#x}\",\"at\":\"{at:#x}\",\"ptr\":{},\"i64\":{},\"f32\":{}}}",
            ptr.map(|p| format!("\"{p:#x}\"")).unwrap_or("null".into()),
            i64v.map(|v| v.to_string()).unwrap_or("null".into()),
            f32v.map(|v| v.to_string()).unwrap_or("null".into()),
        ));
        match ptr {
            Some(p) => cur = p,
            None => break,
        }
    }
    ok(format!("{{\"final\":\"{cur:#x}\",\"steps\":[{}]}}", steps.join(",")))
}

fn build_needle(q: &HashMap<String, String>) -> Result<Vec<u8>, String> {
    if let Some(h) = q.get("hex") {
        let clean: String = h.chars().filter(|c| !c.is_whitespace()).collect();
        if clean.len() % 2 != 0 {
            return Err("hex needs an even number of digits".into());
        }
        let mut out = Vec::with_capacity(clean.len() / 2);
        let b = clean.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let hi = hex_nibble(b[i]).ok_or("bad hex digit")?;
            let lo = hex_nibble(b[i + 1]).ok_or("bad hex digit")?;
            out.push((hi << 4) | lo);
            i += 2;
        }
        return Ok(out);
    }
    if let Some(s) = q.get("str") {
        return Ok(s.as_bytes().to_vec());
    }
    if let Some(s) = q.get("utf16") {
        return Ok(s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect());
    }
    if let Some(v) = q.get("i32").and_then(|s| parse_i64(s)) {
        return Ok((v as i32).to_le_bytes().to_vec());
    }
    if let Some(v) = q.get("u32").and_then(|s| parse_usize(s)) {
        return Ok((v as u32).to_le_bytes().to_vec());
    }
    if let Some(v) = q.get("i64").and_then(|s| parse_i64(s)) {
        return Ok(v.to_le_bytes().to_vec());
    }
    if let Some(v) = q.get("u64").and_then(|s| parse_usize(s)) {
        return Ok((v as u64).to_le_bytes().to_vec());
    }
    if let Some(v) = q.get("f32").and_then(|s| s.parse::<f32>().ok()) {
        return Ok(v.to_le_bytes().to_vec());
    }
    if let Some(v) = q.get("f64").and_then(|s| s.parse::<f64>().ok()) {
        return Ok(v.to_le_bytes().to_vec());
    }
    Err("need one of hex/str/utf16/i32/u32/i64/u64/f32/f64".into())
}

fn ep_scan(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    let needle = match build_needle(q) {
        Ok(n) => n,
        Err(e) => return err(&e),
    };

    let (start, end) = if let Some(m) = q.get("mod") {
        match unsafe { il2cpp::module_bounds(m) } {
            Some(b) => b,
            None => return err("module not loaded"),
        }
    } else {
        let start = q.get("start").and_then(|s| parse_usize(s)).unwrap_or(0);
        let end = q.get("end").and_then(|s| parse_usize(s)).unwrap_or(0x7FFF_FFFF_0000);
        (start, end)
    };
    let max_hits = q.get("max").and_then(|s| parse_usize(s)).unwrap_or(DEFAULT_SCAN_HITS);

    let hits = unsafe { il2cpp::scan(&needle, start, end, max_hits, DEFAULT_SCAN_BYTES) };
    let list: Vec<String> = hits.iter().map(|a| format!("\"{a:#x}\"")).collect();
    ok(format!(
        "{{\"needle_len\":{},\"count\":{},\"hits\":[{}]}}",
        needle.len(),
        list.len(),
        list.join(",")
    ))
}

fn ep_gameworld() -> (&'static str, &'static str, String) {
    let gw = crate::hooks::game_world();
    let alive = unsafe { runtime::is_alive(gw) };
    ok(format!("{{\"ptr\":\"{:#x}\",\"alive\":{}}}", gw.raw() as usize, alive))
}

fn ep_players(domain_raw: usize) -> (&'static str, &'static str, String) {
    let Some(rt) = runtime::get() else { return err("runtime not ready") };
    let dom = domain(domain_raw);
    let gw = crate::hooks::game_world();
    if gw.is_null() || !unsafe { runtime::is_alive(gw) } {
        return ok("{\"gameworld\":null,\"players\":[]}".to_string());
    }
    let Some(gw_cls) = (unsafe { symbols::find_class(rt, dom, eft::GAME_WORLD) }) else {
        return err("GameWorld class not found");
    };
    // First player-list field that resolves.
    let mut off = None;
    let mut fname = "";
    for name in eft::PLAYER_LIST_FIELDS {
        if let Some(f) = unsafe { rt.field(gw_cls, name) } {
            off = Some(unsafe { rt.field_offset(f) });
            fname = name;
            break;
        }
    }
    let Some(off) = off else { return err("no player-list field on GameWorld") };
    let list = Object(unsafe { runtime::read_field::<*mut c_void>(gw, off) });
    let items = unsafe { runtime::list_items(list) };
    let n = unsafe { runtime::list_count(list) };
    let mut ptrs: Vec<String> = Vec::new();
    if !items.is_null() && n <= 4096 {
        let cap = n.min(unsafe { runtime::array_len(items) });
        for i in 0..cap {
            let p = unsafe { runtime::array_get(items, i) };
            if !p.is_null() {
                ptrs.push(format!("\"{:#x}\"", p.raw() as usize));
            }
        }
    }
    ok(format!(
        "{{\"gameworld\":\"{:#x}\",\"field\":{},\"field_off\":\"{:#x}\",\"count\":{},\"players\":[{}]}}",
        gw.raw() as usize,
        jstr(fname),
        off,
        ptrs.len(),
        ptrs.join(",")
    ))
}

fn ep_chams(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    if let Some(v) = q.get("on") {
        let on = matches!(v.as_str(), "1" | "true" | "on" | "yes");
        crate::chams::set_enabled(on);
    }
    if let Some(n) = q.get("scheme").and_then(|s| s.parse::<usize>().ok()) {
        crate::chams::set_scheme(n);
    }
    if let Some(style) = q.get("style") {
        // Accept an index or a name (flat/glow/mesh).
        match style.parse::<usize>() {
            Ok(n) => crate::chams::set_style(n),
            Err(_) => match style.as_str() {
                "glow" => crate::chams::set_style(1),
                "mesh" => crate::chams::set_style(2),
                _ => crate::chams::set_style(0),
            },
        }
    }
    let (con, sch, sname, scount, style) = crate::chams::status();
    ok(format!(
        "{{\"on\":{},\"scheme\":{},\"name\":{},\"count\":{},\"style\":{}}}",
        con,
        sch,
        jstr(sname),
        scount,
        jstr(style)
    ))
}

fn ep_raid(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    if let Some(v) = q.get("on") {
        crate::world::set_enabled(matches!(v.as_str(), "1" | "true" | "on" | "yes"));
    }
    // Snapshot is published by the main-thread tick; reading it here touches no
    // Unity state, so it is safe on the HTTP thread.
    let s = crate::world::snapshot();
    let items: Vec<String> = s
        .iter()
        .map(|e| {
            format!(
                "{{\"ptr\":\"{:#x}\",\"local\":{},\"bot\":{},\"pos\":[{:.2},{:.2},{:.2}],\
                 \"screen\":[{:.1},{:.1},{:.2}],\"on_screen\":{},\"dist\":{:.1}}}",
                e.ptr, e.is_local, e.is_bot,
                e.pos[0], e.pos[1], e.pos[2],
                e.screen[0], e.screen[1], e.screen[2],
                e.on_screen, e.dist
            )
        })
        .collect();
    ok(format!(
        "{{\"enabled\":{},\"count\":{},\"players\":[{}]}}",
        crate::world::is_enabled(),
        items.len(),
        items.join(",")
    ))
}

fn ep_stamina(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    if let Some(v) = q.get("on") {
        crate::features::set_inf_stamina(matches!(v.as_str(), "1" | "true" | "on" | "yes"));
    }
    ok(format!("{{\"inf_stamina\":{}}}", crate::features::inf_stamina()))
}

fn ep_speed(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    if let Some(v) = q.get("on") {
        crate::features::set_speed_on(matches!(v.as_str(), "1" | "true" | "on" | "yes"));
    }
    if let Some(m) = q.get("mult").and_then(|s| s.parse::<f32>().ok()) {
        crate::features::set_speed_mult(m);
    }
    ok(format!(
        "{{\"on\":{},\"mult\":{:.2}}}",
        crate::features::speed_on(),
        crate::features::speed_mult()
    ))
}

fn ep_menu(q: &HashMap<String, String>) -> (&'static str, &'static str, String) {
    if let Some(v) = q.get("visible") {
        crate::menu::set_visible(matches!(v.as_str(), "1" | "true" | "on" | "yes"));
    }
    ok(format!("{{\"visible\":{}}}", crate::menu::is_visible()))
}
