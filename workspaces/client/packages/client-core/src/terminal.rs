//! Terminal state and ordered input live in Rust. React only displays the grid.
use crate::{
    backend::Backend,
    file_save::uuid,
    protocol::{Request, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    num::NonZeroU16,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

type Command = (Request, oneshot::Sender<Response>);
// Initial terminal grid dimensions in rows and columns; both are nonzero.
const INITIAL_ROWS: NonZeroU16 = NonZeroU16::new(24).unwrap();
const INITIAL_COLS: NonZeroU16 = NonZeroU16::new(100).unwrap();
struct Entry {
    input: mpsc::Sender<Command>,
    value: watch::Receiver<Value>,
    task: JoinHandle<()>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone, Default)]
pub struct Terminals(Arc<Mutex<HashMap<String, Entry>>>);
impl Terminals {
    // Called in transport arrival order, before concurrent response tasks start.
    pub fn queue(
        &self,
        backend: &Result<Backend, String>,
        request: Request,
    ) -> oneshot::Receiver<Response> {
        let (reply, receive) = oneshot::channel();
        let id = request.id.clone();
        let immediate = |result: Result<Value, String>| match result {
            Ok(v) => Response::success(id.clone(), v),
            Err(e) => Response::failure(id.clone(), "terminal", e),
        };
        let mut entries = self.0.lock().unwrap();
        if request.method == "terminal.open" {
            let result = (|| {
                if entries.len() >= 4 {
                    return Err("Close another terminal view first.".into());
                }
                let backend = backend.as_ref().map_err(Clone::clone)?.clone();
                let terminal = terminal_id(&request.params["id"])?;
                let handle = uuid()?;
                let (input, commands) = mpsc::channel(32);
                let (changed, value) = watch::channel(Value::Null);
                let task = tokio::spawn(run(backend, terminal, commands, changed));
                entries.insert(handle.clone(), Entry { input, value, task });
                Ok(json!({"handle":handle}))
            })();
            let _ = reply.send(immediate(result));
        } else {
            let handle = request.params["handle"].as_str().unwrap_or("");
            if request.method == "terminal.release" {
                entries.remove(handle);
                let _ = reply.send(immediate(Ok(json!({"closed":true}))));
            } else if let Some(entry) = entries.get(handle) {
                if let Err(e) = entry.input.try_send((request, reply)) {
                    let (_, reply) = e.into_inner();
                    let _ = reply.send(immediate(Err(
                        "The terminal command queue is unavailable. This input was not sent."
                            .into(),
                    )));
                }
            } else {
                let _ = reply.send(immediate(Err("This terminal view is closed.".into())));
            }
        }
        receive
    }
    pub fn watch(&self, handle: &str) -> Result<watch::Receiver<Value>, String> {
        self.0
            .lock()
            .unwrap()
            .get(handle)
            .map(|e| e.value.clone())
            .ok_or("This terminal view is closed.".into())
    }
}
fn terminal_id(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
        .map(str::to_owned)
        .ok_or("Invalid terminal identity.".into())
}
async fn post(backend: &Backend, action: &str, body: Value) -> Result<Value, String> {
    let response = backend.dispatch(Request {id:"terminal-http".into(),method:"api.request".into(),
        params:json!({"path":format!("/api/terminals/{action}"),"body":body,"timeoutMs":10000})}).await;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or("The terminal response is missing.".into())
    })
}
async fn send(backend: &Backend, id: &str, text: String) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > 65536 {
        return Err("Terminal input exceeds 64 KiB.".into());
    }
    let request_id = uuid()?;
    // This mutation has one identity and one attempt. No automatic retry.
    let receipt = post(
        backend,
        "input",
        json!({"id":id,"text":text,"request_id":request_id}),
    )
    .await?;
    if receipt["ok"] != true || receipt["delivery"] != "sent" {
        return Err(
            "Terminal input delivery is uncertain. Check the terminal before sending it again."
                .into(),
        );
    }
    Ok(())
}
#[derive(Default)]
struct Replies(String);
impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i1.is_some() || i2.is_some() || self.0.len() > 4096 {
            return;
        }
        let p = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (c, p) {
            ('n', 5) => self.0.push_str("\x1b[0n"),
            ('n', 6) => {
                let (r, c) = screen.cursor_position();
                self.0.push_str(&format!("\x1b[{};{}R", r + 1, c + 1));
            }
            ('c', 0) => self.0.push_str("\x1b[?1;2c"),
            _ => {}
        }
    }
}
struct Screen {
    parser: vt100::Parser<Replies>,
    offset: u64,
    loaded: bool,
    truncated: bool,
    status: String,
    failure: String,
    input_failure: String,
}
impl Screen {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                INITIAL_ROWS,
                INITIAL_COLS,
                5000,
                Replies::default(),
            ),
            offset: 0,
            loaded: false,
            truncated: false,
            status: "loading".into(),
            failure: String::new(),
            input_failure: String::new(),
        }
    }
    fn update(&mut self, output: Value) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Output {
            text: String,
            offset: u64,
            truncated: bool,
            status: String,
            error: Option<String>,
        }
        let output: Output = serde_json::from_value(output)
            .map_err(|_| "The server returned invalid terminal output.")?;
        let count = output.text.chars().count() as u64;
        if output.offset < self.offset
            || output.offset < count
            || (!output.truncated && output.offset - self.offset != count)
        {
            return Err(
                "The terminal output cursor changed unexpectedly. Reconnect to read it again."
                    .into(),
            );
        }
        if output.truncated {
            let (rows, cols) = self.parser.screen().size();
            self.parser = vt100::Parser::new_with_callbacks(rows, cols, 5000, Replies::default());
            self.truncated = true;
        }
        self.parser.process(output.text.as_bytes());
        let replies = std::mem::take(&mut self.parser.callbacks_mut().0);
        let live = self.loaded && !output.truncated;
        self.loaded = true;
        self.offset = output.offset;
        self.status = output.status;
        self.failure = output.error.unwrap_or_default();
        // Retained output can contain old queries. Never replay their replies.
        Ok(if live { replies } else { String::new() })
    }
    fn value(&self) -> Value {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (rows, cols) = (rows.get(), cols.get());
        let cursor = screen.cursor_position();
        let lines:Vec<Value>=(0..rows).map(|r| {
            let mut runs:Vec<Value>=Vec::new();
            for c in 0..cols {
                let cell=screen.cell(r,c).unwrap(); if cell.is_wide_continuation(){continue;}
                let mut fg=color(cell.fgcolor(),"#dadde5"); let mut bg=color(cell.bgcolor(),"#141519");
                if cell.inverse(){std::mem::swap(&mut fg,&mut bg);}
                let caret=screen.scrollback()==0 && !screen.hide_cursor() && cursor==(r,c);
                let style=json!({"color":fg,"backgroundColor":bg,"fontWeight":if cell.bold(){700}else{400},
                    "fontStyle":if cell.italic(){"italic"}else{"normal"},"textDecoration":if cell.underline(){"underline"}else{"none"},
                    "opacity":if cell.dim(){0.65}else{1.0}});
                let text=if cell.contents().is_empty(){" "}else{cell.contents()}; let width=if cell.is_wide(){2}else{1};
                if let Some(last)=runs.last_mut().filter(|last|last["style"]==style && last["cursor"]==caret) {
                    last["text"]=json!(format!("{}{}",last["text"].as_str().unwrap(),text));
                    last["columns"]=json!(last["columns"].as_u64().unwrap()+width);
                } else {runs.push(json!({"text":text,"columns":width,"style":style,"cursor":caret}));}
            }
            json!(runs)
        }).collect();
        json!({"lines":lines,"rows":rows,"cols":cols,"status":self.status,"failure":self.failure,
            "inputFailure":self.input_failure,"truncated":self.truncated,"scrollback":screen.scrollback(),
            "alternate":screen.alternate_screen(),"mouse":screen.mouse_protocol_mode()!=vt100::MouseProtocolMode::None})
    }
}
fn color(value: vt100::Color, default: &str) -> String {
    const ANSI: [&str; 16] = [
        "#1d1f21", "#cc6666", "#b5bd68", "#f0c674", "#81a2be", "#b294bb", "#8abeb7", "#c5c8c6",
        "#666666", "#d54e53", "#b9ca4a", "#e7c547", "#7aa6da", "#c397d8", "#70c0b1", "#eaeaea",
    ];
    match value {
        vt100::Color::Default => default.into(),
        vt100::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        vt100::Color::Idx(n) if n < 16 => ANSI[n as usize].into(),
        vt100::Color::Idx(n) if n >= 232 => {
            let c = 8 + (n - 232) * 10;
            format!("#{c:02x}{c:02x}{c:02x}")
        }
        vt100::Color::Idx(n) => {
            let n = n - 16;
            let v = [0, 95, 135, 175, 215, 255];
            format!(
                "#{:02x}{:02x}{:02x}",
                v[(n / 36) as usize],
                v[(n / 6 % 6) as usize],
                v[(n % 6) as usize]
            )
        }
    }
}
fn running(status: &str) -> bool {
    status == "running"
}
fn input(screen: &vt100::Screen, params: &Value) -> Result<String, String> {
    let kind = params["kind"].as_str().unwrap_or("");
    if kind == "text" || kind == "paste" {
        let text = params["text"]
            .as_str()
            .filter(|s| s.len() <= 65520)
            .ok_or("Terminal input exceeds 64 KiB.")?;
        let text = if kind == "paste" {
            text.replace("\r\n", "\r").replace('\n', "\r")
        } else {
            text.into()
        };
        return Ok(if kind == "paste" && screen.bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        });
    }
    if kind == "mouse" {
        return mouse(screen, params);
    }
    if kind != "key" {
        return Err("Unknown terminal input type.".into());
    }
    let key = params["key"].as_str().ok_or("Missing terminal key.")?;
    let shift = params["shift"] == true;
    let alt = params["alt"] == true;
    let ctrl = params["ctrl"] == true;
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let cursor = match key {
        "ArrowUp" => Some('A'),
        "ArrowDown" => Some('B'),
        "ArrowRight" => Some('C'),
        "ArrowLeft" => Some('D'),
        "Home" => Some('H'),
        "End" => Some('F'),
        _ => None,
    };
    if let Some(c) = cursor {
        return Ok(if modifier > 1 {
            format!("\x1b[1;{modifier}{c}")
        } else if screen.application_cursor() {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        });
    }
    let special = match key {
        "Enter" => "\r",
        "Backspace" => {
            if ctrl {
                "\x08"
            } else {
                "\x7f"
            }
        }
        "Tab" => {
            if shift {
                "\x1b[Z"
            } else {
                "\t"
            }
        }
        "Escape" => "\x1b",
        _ => "",
    };
    if !special.is_empty() {
        return Ok(format!("{}{}", if alt { "\x1b" } else { "" }, special));
    }
    let code = match key {
        "Insert" => 2,
        "Delete" => 3,
        "PageUp" => 5,
        "PageDown" => 6,
        "F5" => 15,
        "F6" => 17,
        "F7" => 18,
        "F8" => 19,
        "F9" => 20,
        "F10" => 21,
        "F11" => 23,
        "F12" => 24,
        _ => 0,
    };
    if code != 0 {
        return Ok(if modifier > 1 {
            format!("\x1b[{code};{modifier}~")
        } else {
            format!("\x1b[{code}~")
        });
    }
    if let Some(c) = match key {
        "F1" => Some('P'),
        "F2" => Some('Q'),
        "F3" => Some('R'),
        "F4" => Some('S'),
        _ => None,
    } {
        return Ok(if modifier > 1 {
            format!("\x1b[1;{modifier}{c}")
        } else {
            format!("\x1bO{c}")
        });
    }
    if ctrl && key.len() == 1 && key.is_ascii() {
        let c = key.as_bytes()[0].to_ascii_uppercase();
        let c = match c {
            b'?' => 127,
            b' '..=b'_' => c & 31,
            _ => return Err("Unsupported control key.".into()),
        };
        return Ok(format!("{}{}", if alt { "\x1b" } else { "" }, c as char));
    }
    if alt && key.chars().count() == 1 {
        return Ok(format!("\x1b{key}"));
    }
    Err("Unsupported terminal key.".into())
}
fn mouse(screen: &vt100::Screen, params: &Value) -> Result<String, String> {
    use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};
    let mode = screen.mouse_protocol_mode();
    if mode == Mode::None {
        return Ok(String::new());
    }
    let (rows, cols) = screen.size();
    let row = params["row"]
        .as_u64()
        .filter(|v| *v < u64::from(rows.get()))
        .ok_or("Invalid mouse row.")?
        + 1;
    let col = params["col"]
        .as_u64()
        .filter(|v| *v < u64::from(cols.get()))
        .ok_or("Invalid mouse column.")?
        + 1;
    let action = params["action"].as_str().unwrap_or("");
    let button = params["button"]
        .as_u64()
        .filter(|v| *v <= 5)
        .ok_or("Invalid mouse button.")?;
    if action == "release" && mode == Mode::Press {
        return Ok(String::new());
    }
    if action == "move"
        && (mode == Mode::Press
            || mode == Mode::PressRelease
            || (mode == Mode::ButtonMotion && button == 3))
    {
        return Ok(String::new());
    }
    if !matches!(action, "press" | "release" | "move") {
        return Err("Invalid mouse action.".into());
    }
    let mut code = if button >= 4 { button + 60 } else { button };
    if action == "move" {
        code += 32;
    }
    if params["shift"] == true {
        code += 4;
    }
    if params["alt"] == true {
        code += 8;
    }
    if params["ctrl"] == true {
        code += 16;
    }
    match screen.mouse_protocol_encoding() {
        Encoding::Sgr => Ok(format!(
            "\x1b[<{code};{col};{row}{}",
            if action == "release" { 'm' } else { 'M' }
        )),
        encoding => {
            if action == "release" {
                code = 3;
            }
            // The server's input endpoint carries UTF-8 text. Do not corrupt
            // a legacy single-byte coordinate above its ASCII range.
            if encoding == Encoding::Default && (row > 95 || col > 95) {
                return Err("This application uses legacy mouse coordinates outside the text input range. Use its keyboard controls.".into());
            }
            Ok(format!(
                "\x1b[M{}{}{}",
                char::from_u32((code + 32) as u32).unwrap(),
                char::from_u32((col + 32) as u32).unwrap(),
                char::from_u32((row + 32) as u32).unwrap()
            ))
        }
    }
}
async fn run(
    backend: Backend,
    id: String,
    mut commands: mpsc::Receiver<Command>,
    changed: watch::Sender<Value>,
) {
    let mut screen = Screen::new();
    let mut interval = tokio::time::interval(Duration::from_millis(350));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = Value::Null;
    loop {
        tokio::select! {
            command=commands.recv()=>{
                let Some((request,reply))=command else {break;};
                let result=async {
                    match request.method.as_str() {
                        "terminal.input"=>{
                            if !screen.input_failure.is_empty(){return Err(screen.input_failure.clone());}
                            if !running(&screen.status){return Err("This terminal is not running.".into());}
                            let text=input(screen.parser.screen(),&request.params)?;
                            if let Err(e)=send(&backend,&id,text).await {screen.input_failure=format!("{e} Input paused. Reconnect before you continue.");return Err(screen.input_failure.clone());}
                            screen.parser.screen_mut().set_scrollback(0);
                            Ok(json!({"sent":true}))
                        },
                        "terminal.resize"=>{
                            let cols=request.params["cols"].as_u64().filter(|n|(2..=240).contains(n)).and_then(|n|NonZeroU16::new(n as u16)).ok_or("Invalid terminal width.")?;
                            let rows=request.params["rows"].as_u64().filter(|n|(2..=120).contains(n)).and_then(|n|NonZeroU16::new(n as u16)).ok_or("Invalid terminal height.")?;
                            if screen.parser.screen().size()!=(rows,cols) {
                                if running(&screen.status) {post(&backend,"resize",json!({"id":id,"cols":cols,"rows":rows})).await?;}
                                screen.parser.screen_mut().set_size(rows,cols);
                            }
                            Ok(json!({"resized":true}))
                        },
                        "terminal.scroll"=>{
                            let delta=request.params["delta"].as_i64().filter(|v|v.unsigned_abs()<=5000).ok_or("Invalid scroll distance.")?;
                            let offset=(screen.parser.screen().scrollback()as i64+delta).clamp(0,5000)as usize;
                            screen.parser.screen_mut().set_scrollback(offset);Ok(json!({"scrolled":true}))
                        },
                        _=>Err("Unknown terminal command.".into())
                    }
                }.await;
                let response=match result {Ok(v)=>Response::success(request.id,v),Err(e)=>Response::failure(request.id,"terminal",e)};
                let _=reply.send(response);
            },
            _=interval.tick()=>{
                let result=async {
                    let output=backend.read_bounded(&format!("/api/terminals/output?id={id}&offset={}",screen.offset),4*1024*1024).await?;
                    let replies=screen.update(output)?;
                    if !replies.is_empty() && running(&screen.status) && screen.input_failure.is_empty()
                        && let Err(e)=send(&backend,&id,replies).await{screen.input_failure=format!("{e} Input paused. Reconnect before you continue.");}
                    Ok::<_,String>(())
                }.await;
                if let Err(e)=result{screen.failure=e;}
            }
        }
        let value = screen.value();
        if value != previous {
            changed.send_replace(value.clone());
            previous = value;
        }
    }
}

pub async fn history(backend: &Backend, value: &Value) -> Result<(String, Arc<Vec<u8>>), String> {
    let id = terminal_id(&value["id"])?;
    let mut offset = 0u64;
    let mut end = None;
    let mut bytes = Vec::new();
    for _ in 0..512 {
        let limit = end
            .map(|end: u64| end.saturating_sub(offset).min(65536))
            .unwrap_or(65536)
            .max(1);
        let part = backend
            .read_bounded(
                &format!("/api/terminals/output?id={id}&history=1&offset={offset}&limit={limit}"),
                2 * 1024 * 1024,
            )
            .await?;
        let text = part["text"]
            .as_str()
            .ok_or("Invalid saved terminal output.")?;
        let next = part["offset"]
            .as_u64()
            .ok_or("Invalid saved terminal cursor.")?;
        let available = part["availableOffset"]
            .as_u64()
            .ok_or("Invalid saved terminal size.")?;
        let more = part["hasMore"]
            .as_bool()
            .ok_or("Invalid saved terminal continuation.")?;
        if part["truncated"] != false {
            return Err("Earlier output is unavailable. The saved output is incomplete.".into());
        }
        let end = *end.get_or_insert(available);
        if next < offset
            || next - offset != text.chars().count() as u64
            || next > end
            || (more && next == offset)
            || available < end
        {
            return Err("Saved terminal output changed during the download.".into());
        }
        if bytes.len() + text.len() > 32 * 1024 * 1024 {
            return Err("Saved terminal output exceeds 32 MiB.".into());
        }
        bytes.extend_from_slice(text.as_bytes());
        offset = next;
        if offset == end {
            return Ok((format!("terminal-{id}.txt"), Arc::new(bytes)));
        }
        if !more {
            return Err("Saved terminal output is incomplete.".into());
        }
    }
    Err("Saved terminal output exceeds 512 pages.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(screen: &mut Screen, text: &str, truncated: bool) -> String {
        let offset = screen.offset + text.chars().count() as u64;
        screen.update(json!({"text":text,"offset":offset,"truncated":truncated,"status":"running","error":null})).unwrap()
    }
    #[test]
    fn unicode_color_cursor_and_split_sequences_are_native_state() {
        let mut s = Screen::new();
        output(&mut s, "\x1b[3", false);
        output(&mut s, "1mЖ🦀\x1b[0m\r\nNext", false);
        let value = s.value();
        assert_eq!(value["lines"][0][0]["text"], "Ж🦀");
        assert_eq!(value["lines"][0][0]["columns"], 3);
        assert_eq!(value["lines"][0][0]["style"]["color"], "#cc6666");
        assert_eq!(s.parser.screen().cursor_position(), (1, 4));
        output(&mut s, "\rDone\x1b[K", false);
        assert!(s.parser.screen().contents().contains("Done"));
        assert!(!s.parser.screen().contents().contains("Next"));
        output(&mut s, "\x1b[?1049hALT", false);
        assert!(s.parser.screen().alternate_screen());
        output(&mut s, "\x1b[?1049l", false);
        assert!(s.parser.screen().contents().contains("Done"));
    }
    #[test]
    fn resized_grid_keeps_cursor_output_and_truncation_dimensions() {
        let mut s = Screen::new();
        assert_eq!(s.value()["rows"], 24);
        assert_eq!(s.value()["cols"], 100);
        let rows = std::num::NonZeroU16::new(3).unwrap();
        let cols = std::num::NonZeroU16::new(8).unwrap();
        s.parser.screen_mut().set_size(rows, cols);
        output(&mut s, "one\r\ntwo\r\nthree", false);
        s.parser
            .screen_mut()
            .set_size(std::num::NonZeroU16::new(2).unwrap(), cols);
        assert_eq!(s.parser.screen().contents(), "two\nthree");
        assert_eq!(s.parser.screen().cursor_position(), (1, 5));
        let value = s.value();
        assert_eq!(value["rows"], 2);
        assert_eq!(value["cols"], 8);
        assert_eq!(value["lines"].as_array().unwrap().len(), 2);
        s.parser.screen_mut().set_size(rows, cols);
        assert_eq!(s.parser.screen().contents(), "one\ntwo\nthree");
        output(&mut s, "new\x1b[6n", true);
        assert_eq!(s.value()["rows"], 3);
        assert_eq!(s.value()["cols"], 8);
        assert_eq!(s.parser.screen().contents(), "new");
        assert!(s.parser.callbacks().0.is_empty());
    }
    #[test]
    fn old_queries_never_write_and_invalid_cursors_do_not_consume_output() {
        let mut s = Screen::new();
        assert_eq!(output(&mut s, "old\x1b[6n", false), "");
        assert_eq!(output(&mut s, "\x1b[6n", false), "\x1b[1;4R");
        assert_eq!(output(&mut s, "new\x1b[6n", true), "");
        assert!(s.truncated);
        let prior = s.parser.screen().contents();
        assert!(
            s.update(json!({"text":"wrong","offset":0,"truncated":false,"status":"running"}))
                .is_err()
        );
        assert_eq!(s.parser.screen().contents(), prior);
    }
    #[test]
    fn keys_paste_and_scrollback_follow_terminal_modes() {
        let mut s = Screen::new();
        assert_eq!(
            input(s.parser.screen(), &json!({"kind":"key","key":"ArrowUp"})).unwrap(),
            "\x1b[A"
        );
        output(&mut s, "\x1b[?1h\x1b[?2004h", false);
        assert_eq!(
            input(s.parser.screen(), &json!({"kind":"key","key":"ArrowUp"})).unwrap(),
            "\x1bOA"
        );
        assert_eq!(
            input(
                s.parser.screen(),
                &json!({"kind":"key","key":"ArrowLeft","ctrl":true})
            )
            .unwrap(),
            "\x1b[1;5D"
        );
        assert_eq!(
            input(
                s.parser.screen(),
                &json!({"kind":"key","key":"c","ctrl":true})
            )
            .unwrap(),
            "\x03"
        );
        assert_eq!(
            input(
                s.parser.screen(),
                &json!({"kind":"key","key":"?","ctrl":true})
            )
            .unwrap(),
            "\x7f"
        );
        assert_eq!(
            input(s.parser.screen(), &json!({"kind":"paste","text":"Ж🦀"})).unwrap(),
            "\x1b[200~Ж🦀\x1b[201~"
        );
        for n in 0..60 {
            output(&mut s, &format!("{n}\r\n"), false);
        }
        s.parser.screen_mut().set_scrollback(5000);
        assert!(s.parser.screen().scrollback() > 0);
        s.parser.screen_mut().set_scrollback(0);
        assert!(s.parser.screen().contents().contains("59"));
        assert!(
            input(
                s.parser.screen(),
                &json!({"kind":"text","text":"x".repeat(65537)})
            )
            .is_err()
        );
    }
    async fn read(stream: &mut tokio::net::TcpStream) -> (String, Value) {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        let header = loop {
            let mut b = [0; 1];
            stream.read_exact(&mut b).await.unwrap();
            bytes.push(b[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break String::from_utf8(bytes.clone()).unwrap();
            }
        };
        let size = header
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|v| v.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = vec![0; size];
        stream.read_exact(&mut body).await.unwrap();
        (
            header,
            if size > 0 {
                serde_json::from_slice(&body).unwrap()
            } else {
                Value::Null
            },
        )
    }
    async fn respond(stream: &mut tokio::net::TcpStream, value: Value) {
        use tokio::io::AsyncWriteExt;
        let body = value.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
    #[tokio::test]
    async fn lost_input_reply_blocks_queued_input_and_release_stops_the_view() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend =
            Ok(Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap());
        let (sent, mut inputs) = mpsc::channel(8);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, body) = read(&mut stream).await;
                if head.starts_with("GET /api/session ") {
                    respond(&mut stream, json!({"token":"test"})).await;
                } else if head.starts_with("GET /api/terminals/output") {
                    respond(&mut stream,json!({"text":"","offset":0,"truncated":false,"status":"running","error":null})).await;
                } else {
                    sent.send(body).await.unwrap();
                    drop(stream);
                }
            }
        });
        let terminals = Terminals::default();
        let opened = terminals
            .queue(
                &backend,
                Request {
                    id: "open".into(),
                    method: "terminal.open".into(),
                    params: json!({"id":"shell"}),
                },
            )
            .await
            .unwrap();
        let handle = opened.result.unwrap()["handle"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut value = terminals.watch(&handle).unwrap();
        tokio::time::timeout(Duration::from_secs(2), value.changed())
            .await
            .unwrap()
            .unwrap();
        let first = terminals.queue(
            &backend,
            Request {
                id: "first".into(),
                method: "terminal.input".into(),
                params: json!({"handle":handle,"kind":"text","text":"first"}),
            },
        );
        let second = terminals.queue(
            &backend,
            Request {
                id: "second".into(),
                method: "terminal.input".into(),
                params: json!({"handle":handle,"kind":"text","text":"second"}),
            },
        );
        assert!(
            first
                .await
                .unwrap()
                .error
                .unwrap()
                .message
                .contains("Input paused")
        );
        assert!(
            second
                .await
                .unwrap()
                .error
                .unwrap()
                .message
                .contains("Input paused")
        );
        let submitted = inputs.recv().await.unwrap();
        assert_eq!(submitted["text"], "first");
        assert_eq!(submitted["request_id"].as_str().unwrap().len(), 36);
        assert_eq!(inputs.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        terminals
            .queue(
                &backend,
                Request {
                    id: "release".into(),
                    method: "terminal.release".into(),
                    params: json!({"handle":handle}),
                },
            )
            .await
            .unwrap();
        assert!(terminals.watch(&handle).is_err());
        server.abort();
    }
    #[tokio::test]
    async fn history_preserves_unicode_and_rejects_missing_prefix() {
        for truncated in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend =
                Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, _) = read(&mut stream).await;
                assert!(head.contains("history=1&offset=0&limit=65536"));
                respond(&mut stream,json!({"text":"Ж🦀\r\n","offset":4,"availableOffset":4,"hasMore":false,"truncated":truncated})).await;
            });
            let result = history(&backend, &json!({"id":"shell"})).await;
            if truncated {
                assert!(result.unwrap_err().contains("incomplete"));
            } else {
                assert_eq!(result.unwrap().1.as_slice(), "Ж🦀\r\n".as_bytes());
            }
            server.await.unwrap();
        }
    }
}
