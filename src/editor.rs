//! Neovim, embedded: `nvim --embed` over msgpack-RPC, drawn by the window.
//!
//! The user's own Neovim — their `init.lua`, plugins and colour scheme — but
//! with the window drawing the grid rather than a terminal emulator, so the
//! font, the cursor and the keyboard are the app's. A session is one draft:
//! a temporary file Neovim edits, which is what keeps the draft if Neovim
//! dies, and the RPC channel the window reads the buffer through.
//!
//! The signature is not in that file. Neovim shows it as virtual lines it
//! cannot edit, and the session puts it back into the body whenever the body
//! is read, so a plugin that trims whitespace cannot reach its `-- `.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nvim_rs::{Handler, Neovim, UiAttachOptions, compat::tokio::Compat, create::tokio as create};
use rmpv::Value;
use tokio::process::{ChildStdin, Command};

/// What the window hears from a session.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "events", rename_all = "lowercase")]
pub enum EditorEvent {
    /// The `redraw` events up to and including a `flush`, as Neovim sent
    /// them: each is `[name, args…]`, see `:help ui-linegrid`.
    Redraw(Vec<serde_json::Value>),
    /// The draft was written with `:w`.
    Written,
    /// Neovim has gone — `:q`, `:x`, or a crash.
    Exited,
}

/// Where a session's events go.
pub type EditorSink = Arc<dyn Fn(EditorEvent) + Send + Sync>;

type Nvim = Neovim<Compat<ChildStdin>>;

/// The notification the `BufWritePost` autocommand sends.
const WRITTEN: &str = "orb_written";

/// How long a call to Neovim may take. A Neovim at a prompt — "Press
/// ENTER", `input()`, a `:!` command — answers nothing until the prompt is
/// done, and a save or a close must not wait for that.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// A call to Neovim, bounded by [`CALL_TIMEOUT`].
async fn bounded<T, E: std::fmt::Display>(
    call: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, EditorError> {
    match tokio::time::timeout(CALL_TIMEOUT, call).await {
        Ok(result) => result.map_err(call_error),
        Err(_) => Err(EditorError::Busy),
    }
}

/// The signature a session shows below the text rather than in it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
    lines: Vec<String>,
    /// The line of the file it goes above when the file was last written;
    /// `None` for after the last line, wherever that is now.
    at: Option<usize>,
}

#[derive(Clone)]
struct Ui {
    sink: EditorSink,
    /// Redraw events since the last flush: one batch per screen update
    /// rather than one message per cell.
    pending: Arc<Mutex<Vec<serde_json::Value>>>,
    signature: Arc<Mutex<Option<Signature>>>,
}

#[async_trait]
impl Handler for Ui {
    type Writer = Compat<ChildStdin>;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _neovim: Nvim) {
        match name.as_str() {
            "redraw" => {
                for event in args {
                    let flush = matches!(
                        &event,
                        Value::Array(items) if items.first().and_then(Value::as_str) == Some("flush")
                    );
                    let batch = {
                        let mut pending = self.pending.lock().unwrap();
                        pending.push(to_json(event));
                        flush.then(|| std::mem::take(&mut *pending))
                    };
                    if let Some(batch) = batch {
                        (self.sink)(EditorEvent::Redraw(batch));
                    }
                }
            }
            WRITTEN => {
                // Where the signature stood in what was written, for reading
                // the draft from the file once Neovim is gone.
                if let Some(signature) = self.signature.lock().unwrap().as_mut() {
                    signature.at = args
                        .first()
                        .and_then(Value::as_u64)
                        .and_then(|at| usize::try_from(at).ok());
                }
                (self.sink)(EditorEvent::Written)
            }
            _ => {}
        }
    }
}

/// msgpack as JSON for the window. Strings that are not UTF-8 are read
/// lossily, and the extension types (buffer and window handles) become
/// null: the window draws one grid and never names a buffer.
fn to_json(value: Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(b),
        Value::Integer(i) => i
            .as_i64()
            .map(J::from)
            .or_else(|| i.as_u64().map(J::from))
            .unwrap_or(J::Null),
        Value::F32(f) => serde_json::Number::from_f64(f64::from(f)).map_or(J::Null, J::Number),
        Value::F64(f) => serde_json::Number::from_f64(f).map_or(J::Null, J::Number),
        Value::String(s) => J::String(s.into_str().unwrap_or_default()),
        Value::Binary(bytes) => J::String(String::from_utf8_lossy(&bytes).into_owned()),
        Value::Array(items) => J::Array(items.into_iter().map(to_json).collect()),
        Value::Map(pairs) => J::Object(
            pairs
                .into_iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::String(s) => s.into_str().unwrap_or_default(),
                        other => other.to_string(),
                    };
                    (key, to_json(v))
                })
                .collect(),
        ),
        Value::Ext(..) => J::Null,
    }
}

struct Session {
    /// The window the editor belongs to, so it can be stopped with it.
    owner: String,
    nvim: Nvim,
    child: Mutex<Option<tokio::process::Child>>,
    path: PathBuf,
    signature: Arc<Mutex<Option<Signature>>>,
}

/// The open editors, by the id the window gave each.
#[derive(Default)]
pub struct Editors {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum EditorError {
    #[error("could not start Neovim: {0}")]
    Start(String),
    #[error("no editor is open with id {0}")]
    Unknown(String),
    #[error("Neovim did not answer: {0}")]
    Call(String),
    #[error("Neovim is waiting at a prompt")]
    Busy,
}

fn call_error(e: impl std::fmt::Display) -> EditorError {
    EditorError::Call(e.to_string())
}

/// Lua that finds the draft's buffer by its file, whatever else is open.
const FIND_BUFFER: &str = r#"
local function draft_buffer(path)
  local want = vim.fn.resolve(path)
  for _, b in ipairs(vim.api.nvim_list_bufs()) do
    local name = vim.api.nvim_buf_get_name(b)
    if name ~= "" and vim.fn.resolve(vim.fn.fnamemodify(name, ":p")) == want then
      return b
    end
  end
end
"#;

/// Lua that shows the signature below the draft, in `__orb_draft`: the
/// draft's buffer once it is read, the signature's lines and the line they go
/// above (`nil` for after the last), and the mark that shows them.
///
/// A mark above a line moves with the text around it; one after the last
/// line is moved back to the end whenever the text changes, or a line added
/// at the end would push the signature into the middle.
const SIGNATURE: &str = r#"
local ns = vim.api.nvim_create_namespace("orb_signature")
local draft = { path = path }
_G.__orb_draft = draft

function draft.place()
  local b = draft.buf
  if not b or not vim.api.nvim_buf_is_valid(b) then return end
  vim.api.nvim_set_hl(0, "OrbSignature", { link = "Comment", default = true })
  vim.api.nvim_buf_clear_namespace(b, ns, 0, -1)
  draft.mark, draft.anchored = nil, false
  local s = draft.signature
  if not s then return end
  local virt = {}
  for _, line in ipairs(s.lines) do table.insert(virt, { { line, "OrbSignature" } }) end
  local count = vim.api.nvim_buf_line_count(b)
  if s.at and s.at < count then
    draft.anchored = true
    draft.mark = vim.api.nvim_buf_set_extmark(b, ns, s.at, 0, { virt_lines = virt, virt_lines_above = true })
  else
    draft.mark = vim.api.nvim_buf_set_extmark(b, ns, count - 1, 0, { virt_lines = virt })
  end
end

function draft.anchor()
  if not (draft.anchored and draft.mark and draft.buf) then return nil end
  return vim.api.nvim_buf_get_extmark_by_id(draft.buf, ns, draft.mark, {})[1]
end

local group = vim.api.nvim_create_augroup("orb_draft", { clear = true })
vim.api.nvim_create_autocmd("BufReadPost", {
  group = group,
  callback = function(args)
    if draft_buffer(path) ~= args.buf then return end
    draft.buf = args.buf
    draft.place()
  end,
})
vim.api.nvim_create_autocmd({ "TextChanged", "TextChangedI", "TextChangedP" }, {
  group = group,
  callback = function(args)
    if args.buf == draft.buf and draft.signature and not draft.anchored then draft.place() end
  end,
})
vim.api.nvim_create_autocmd("ColorScheme", {
  group = group,
  callback = function()
    vim.api.nvim_set_hl(0, "OrbSignature", { link = "Comment", default = true })
  end,
})
"#;

/// Lua that keeps the placeholders of the templates put into the draft, in
/// `__orb_draft.marks`: each an extmark over its text, so it moves with the
/// text around it, and the text Orb last wrote there. A placeholder whose
/// text was edited by hand since is left to the hand that edited it.
///
/// Runs after [`SIGNATURE`], with `draft` and `group` in scope; every
/// function takes the draft's file, as the buffer is found by it.
const PLACEHOLDERS: &str = r#"
local pns = vim.api.nvim_create_namespace("orb_placeholders")
draft.marks = {}
local function placeholder_highlights()
  vim.api.nvim_set_hl(0, "OrbPlaceholder", { link = "DiagnosticWarn", default = true })
  vim.api.nvim_set_hl(0, "OrbPlaceholderFilled", { link = "Visual", default = true })
end
placeholder_highlights()
vim.api.nvim_create_autocmd("ColorScheme", { group = group, callback = placeholder_highlights })

local function set_mark(b, row, col, end_col, filled, id)
  return vim.api.nvim_buf_set_extmark(b, pns, row, col, {
    id = id, end_row = row, end_col = end_col, priority = 200,
    hl_group = filled and "OrbPlaceholderFilled" or "OrbPlaceholder",
    right_gravity = false, end_right_gravity = true,
  })
end

-- The draft's window, when one shows it.
local function draft_window(b)
  local win = vim.fn.bufwinid(b)
  return win ~= -1 and win or nil
end

function draft.insert(path, lines, marks)
  local b = draft_buffer(path)
  if not b then return false end
  local win = draft_window(b)
  local row = win and (vim.api.nvim_win_get_cursor(win)[1] - 1) or 0
  local current = vim.api.nvim_buf_get_lines(b, row, row + 1, false)[1] or ""
  local first = row
  if current:match("^%s*$") then
    vim.api.nvim_buf_set_lines(b, row, row + 1, false, lines)
  else
    first = row + 1
    vim.api.nvim_buf_set_lines(b, first, first, false, lines)
  end
  for _, m in ipairs(marks) do
    local id = set_mark(b, first + m.line, m.start, m.finish, false)
    draft.marks[m.name] = draft.marks[m.name] or {}
    table.insert(draft.marks[m.name], { id = id, text = "{" .. m.name .. "}" })
  end
  if win then vim.api.nvim_win_set_cursor(win, { first + 1, 0 }) end
  return true
end

function draft.fill(path, name, value)
  local b = draft_buffer(path)
  if not b then return end
  local text = value ~= "" and value or ("{" .. name .. "}")
  for _, m in ipairs(draft.marks[name] or {}) do
    local pos = vim.api.nvim_buf_get_extmark_by_id(b, pns, m.id, { details = true })
    if pos[1] and pos[3].end_row == pos[1] then
      local now = vim.api.nvim_buf_get_text(b, pos[1], pos[2], pos[1], pos[3].end_col, {})
      if table.concat(now, "\n") == m.text then
        vim.api.nvim_buf_set_text(b, pos[1], pos[2], pos[1], pos[3].end_col, { text })
        set_mark(b, pos[1], pos[2], pos[2] + #text, value ~= "", m.id)
        m.text = text
      end
    end
  end
end

function draft.go(path, name)
  local b = draft_buffer(path)
  local win = b and draft_window(b)
  if not win then return false end
  for _, m in ipairs(draft.marks[name] or {}) do
    local pos = vim.api.nvim_buf_get_extmark_by_id(b, pns, m.id, {})
    if pos[1] then
      vim.api.nvim_set_current_win(win)
      vim.api.nvim_win_set_cursor(win, { pos[1] + 1, pos[2] })
      return true
    end
  end
  return false
end
"#;

/// The signature as `__orb_draft` holds it.
fn signature_value(signature: Option<&Signature>) -> Value {
    let Some(signature) = signature else {
        return Value::Nil;
    };
    let lines = signature
        .lines
        .iter()
        .map(|line| Value::from(line.as_str()))
        .collect();
    let mut map = vec![("lines".into(), Value::Array(lines))];
    // Left out rather than nil: a nil in a table reaches Lua as `vim.NIL`,
    // which is true.
    if let Some(at) = signature.at {
        map.push(("at".into(), Value::from(at as u64)));
    }
    Value::Map(map)
}

/// `lines` without `signature`'s lines, and the line of what is left that
/// they stood above — `None` when nothing is left below them. `None` overall
/// when the body does not carry the signature as it is written: one that was
/// edited stays in the text, where it can be edited further.
fn take_signature(lines: &[String], signature: &[String]) -> Option<(Vec<String>, Option<usize>)> {
    if signature.is_empty() || signature.len() > lines.len() {
        return None;
    }
    let at = lines
        .windows(signature.len())
        .position(|window| window == signature)?;
    let mut rest = lines[..at].to_vec();
    rest.extend_from_slice(&lines[at + signature.len()..]);
    let at = (at < rest.len()).then_some(at);
    Some((rest, at))
}

/// `lines` with the signature put back above line `at`, or after the last.
fn put_signature(mut lines: Vec<String>, signature: &Signature) -> Vec<String> {
    let at = signature.at.unwrap_or(lines.len()).min(lines.len());
    lines.splice(at..at, signature.lines.iter().cloned());
    lines
}

/// A body as lines, without the newline it ends in.
fn body_lines(body: &str) -> Vec<String> {
    body.strip_suffix('\n')
        .unwrap_or(body)
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// `body` with `signature` (its text, as a draft carries it) taken out: the
/// text for Neovim, and the signature to show below it.
fn split_body(body: &str, signature: Option<&str>) -> (String, Option<Signature>) {
    let taken = signature.map(body_lines).and_then(|signature| {
        take_signature(&body_lines(body), &signature).map(|taken| (taken, signature))
    });
    match taken {
        Some(((rest, at), lines)) => {
            let mut text = rest.join("\n");
            if !rest.is_empty() {
                text.push('\n');
            }
            (text, Some(Signature { lines, at }))
        }
        None => (body.to_string(), None),
    }
}

/// Introduce the window, watch for `:w`, show the signature, and attach the
/// grid.
async fn attach(
    nvim: &Nvim,
    path: &std::path::Path,
    signature: Option<&Signature>,
    cols: u32,
    rows: u32,
) -> Result<(), EditorError> {
    let version: Vec<(Value, Value)> = vec![("major".into(), 0.into()), ("minor".into(), 1.into())];
    bounded(nvim.set_client_info("orb", version, "ui", vec![], vec![])).await?;
    // Before the attach, which is when an embedded Neovim reads the user's
    // configuration: a configuration that stops at a prompt then cannot keep
    // the autocommand from being registered. The buffer is found by its file
    // when the write happens.
    let code = format!(
        r#"{FIND_BUFFER}
        local path, signature = ...
        {SIGNATURE}
        {PLACEHOLDERS}
        draft.signature = signature ~= vim.NIL and signature or nil
        vim.api.nvim_create_autocmd("BufWritePost", {{
          group = group,
          callback = function(args)
            if draft_buffer(path) == args.buf then
              local at = draft.anchor()
              if draft.signature then draft.signature.at = at end
              vim.rpcnotify(0, "{WRITTEN}", at == nil and vim.NIL or at)
            end
          end,
        }})"#
    );
    let args = vec![
        path.to_string_lossy().as_ref().into(),
        signature_value(signature),
    ];
    bounded(nvim.exec_lua(&code, args)).await?;
    let mut options = UiAttachOptions::new();
    options.set_rgb(true);
    options.set_linegrid_external(true);
    let (cols, rows) = (i64::from(cols.max(1)), i64::from(rows.max(1)));
    match bounded(nvim.ui_attach(cols, rows, &options)).await {
        // At a prompt from the user's configuration: the grid is attached and
        // showing it, and the user answers it like any other.
        Ok(()) | Err(EditorError::Busy) => {}
        Err(e) => return Err(e),
    }
    // The draft's file may have been read before the autocommand above could
    // see it; showing the signature twice does no harm.
    let code = format!(
        r#"{FIND_BUFFER}
        local b = draft_buffer(...)
        if b and vim.api.nvim_buf_is_loaded(b) then
          __orb_draft.buf = b
          __orb_draft.place()
        end"#
    );
    let _ = bounded(nvim.exec_lua(&code, vec![path.to_string_lossy().as_ref().into()])).await;
    Ok(())
}

impl Editors {
    fn session(&self, id: &str) -> Result<Arc<Session>, EditorError> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| EditorError::Unknown(id.to_string()))
    }

    /// Start Neovim on `body` in a grid of `cols` × `rows` for the window
    /// `owner`, sending what it draws to `sink`. `signature` is the text of
    /// the identity's signature, which is shown below the draft rather than
    /// in it when the body carries it unchanged. Must run inside the tokio
    /// runtime.
    #[allow(clippy::too_many_arguments)]
    pub async fn open(
        &self,
        id: &str,
        owner: &str,
        body: &str,
        signature: Option<&str>,
        cols: u32,
        rows: u32,
        sink: EditorSink,
    ) -> Result<(), EditorError> {
        let start = |e: &dyn std::fmt::Display| EditorError::Start(e.to_string());
        // `.eml` so Neovim picks the `mail` filetype and the user's settings
        // for mail apply — not those for prose, which tend to strip the
        // trailing spaces that format=flowed and `-- ` depend on. The id is
        // the window's, so only its safe characters reach the file name.
        let safe: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(40)
            .collect();
        let path = crate::files::scratch_dir()
            .map_err(|e| start(&e))?
            .join(format!("draft-{safe}.eml"));
        let (text, signature) = split_body(body, signature);
        crate::files::write_private(&path, text.as_bytes()).map_err(|e| start(&e))?;
        let shown = signature.clone();
        let signature = Arc::new(Mutex::new(signature));

        let ui = Ui {
            sink: sink.clone(),
            pending: Arc::default(),
            signature: signature.clone(),
        };
        let mut command = Command::new("nvim");
        command.arg("--embed").arg(&path).kill_on_drop(true);
        let (nvim, io, child) = match create::new_child_cmd(&mut command, ui).await {
            Ok(spawned) => spawned,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(start(&e));
            }
        };

        // Registered before anything can wait on Neovim, so a session stuck
        // at a prompt can still be typed into, read from and closed.
        self.sessions.lock().unwrap().insert(
            id.to_string(),
            Arc::new(Session {
                owner: owner.to_string(),
                nvim: nvim.clone(),
                child: Mutex::new(Some(child)),
                path: path.clone(),
                signature,
            }),
        );

        if let Err(e) = attach(&nvim, &path, shown.as_ref(), cols, rows).await {
            let _ = self.close(id).await;
            return Err(e);
        }

        // The I/O loop ends when Neovim closes its end, which is when it has
        // gone; the window then puts something else in its place. Started
        // only once the session is up, so a start that failed is reported as
        // a failure and not as a quit.
        tokio::spawn(async move {
            let _ = io.await;
            sink(EditorEvent::Exited);
        });
        Ok(())
    }

    /// Keys, in Neovim's notation (`<C-w>`, `<lt>` for `<`).
    pub async fn input(&self, id: &str, keys: &str) -> Result<(), EditorError> {
        let session = self.session(id)?;
        bounded(session.nvim.input(keys)).await?;
        Ok(())
    }

    /// Text pasted in one piece, as the system pastes it.
    pub async fn paste(&self, id: &str, text: &str) -> Result<(), EditorError> {
        let session = self.session(id)?;
        bounded(session.nvim.paste(text, true, -1)).await?;
        Ok(())
    }

    /// A mouse event on the grid; see `:help nvim_input_mouse()`.
    pub async fn mouse(
        &self,
        id: &str,
        button: &str,
        action: &str,
        modifier: &str,
        row: u32,
        col: u32,
    ) -> Result<(), EditorError> {
        let session = self.session(id)?;
        let (row, col) = (i64::from(row), i64::from(col));
        bounded(
            session
                .nvim
                .input_mouse(button, action, modifier, 0, row, col),
        )
        .await
    }

    /// The window gained or lost the keyboard, for `FocusGained` and
    /// `FocusLost`.
    pub async fn focus(&self, id: &str, gained: bool) -> Result<(), EditorError> {
        let session = self.session(id)?;
        bounded(session.nvim.ui_set_focus(gained)).await
    }

    /// The window is now `cols` × `rows` cells.
    pub async fn resize(&self, id: &str, cols: u32, rows: u32) -> Result<(), EditorError> {
        let session = self.session(id)?;
        let (cols, rows) = (i64::from(cols.max(1)), i64::from(rows.max(1)));
        bounded(session.nvim.ui_try_resize(cols, rows)).await
    }

    /// The draft as it stands in the editor, written or not, with its
    /// signature and ending in a newline as the file would. When Neovim is
    /// gone, or busy at a prompt, what it last wrote.
    pub async fn buffer(&self, id: &str) -> Result<String, EditorError> {
        let session = self.session(id)?;
        let code = format!(
            r#"{FIND_BUFFER}
            local b = draft_buffer(...)
            if not b then return vim.NIL end
            local at = _G.__orb_draft and __orb_draft.anchor()
            return {{ vim.api.nvim_buf_get_lines(b, 0, -1, false), at == nil and vim.NIL or at }}"#
        );
        let path = session.path.to_string_lossy().as_ref().into();
        let signature = session.signature.lock().unwrap().clone();
        let (lines, signature) = match bounded(session.nvim.exec_lua(&code, vec![path])).await {
            Ok(Value::Array(read)) if matches!(read.first(), Some(Value::Array(_))) => {
                let lines = read[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|line| {
                        String::from_utf8_lossy(line.as_slice().unwrap_or_default()).into_owned()
                    })
                    .collect();
                let at = read
                    .get(1)
                    .and_then(Value::as_u64)
                    .and_then(|at| usize::try_from(at).ok());
                (lines, signature.map(|s| Signature { at, ..s }))
            }
            // The buffer was wiped, Neovim is at a prompt, or it is on its
            // way out: the file is the draft, and the signature stands where
            // it stood when the file was written. Neovim writes an `.eml`
            // with CRLF line endings.
            _ => {
                let file = std::fs::read_to_string(&session.path)
                    .unwrap_or_default()
                    .replace("\r\n", "\n");
                (body_lines(&file), signature)
            }
        };
        let lines = match &signature {
            Some(signature) => put_signature(lines, signature),
            None => lines,
        };
        let mut text = lines.join("\n");
        text.push('\n');
        Ok(text)
    }

    /// Replace the draft in the editor, as one change, and write it.
    /// `signature` is shown below it as [`Editors::open`] does.
    pub async fn set_buffer(
        &self,
        id: &str,
        body: &str,
        signature: Option<&str>,
    ) -> Result<(), EditorError> {
        let session = self.session(id)?;
        let (text, signature) = split_body(body, signature);
        let shown = signature_value(signature.as_ref());
        *session.signature.lock().unwrap() = signature;
        let lines: Vec<Value> = body_lines(&text).into_iter().map(Value::from).collect();
        let code = format!(
            r#"{FIND_BUFFER}
            local path, lines, signature = ...
            local b = draft_buffer(path)
            if not b then return end
            vim.api.nvim_buf_set_lines(b, 0, -1, false, lines)
            if _G.__orb_draft then
              __orb_draft.buf = b
              __orb_draft.signature = signature ~= vim.NIL and signature or nil
              __orb_draft.place()
            end
            vim.api.nvim_buf_call(b, function() vim.cmd("silent update") end)"#
        );
        let path = session.path.to_string_lossy().as_ref().into();
        bounded(
            session
                .nvim
                .exec_lua(&code, vec![path, Value::Array(lines), shown]),
        )
        .await?;
        Ok(())
    }

    /// Run `__orb_draft.<function>(path, args…)` in a session.
    async fn draft_call(
        &self,
        id: &str,
        function: &str,
        args: Vec<Value>,
    ) -> Result<Value, EditorError> {
        let session = self.session(id)?;
        let code = format!(
            r#"if not _G.__orb_draft then return vim.NIL end
            return __orb_draft.{function}(...)"#
        );
        let mut all = vec![session.path.to_string_lossy().as_ref().into()];
        all.extend(args);
        bounded(session.nvim.exec_lua(&code, all)).await
    }

    /// Put a template's text into the draft where the cursor is — in place of
    /// the cursor's line when it is blank, below it otherwise — as one
    /// change, and keep its placeholders for [`Editors::fill_placeholder`].
    /// False when the draft's buffer is gone.
    pub async fn insert_template(
        &self,
        id: &str,
        text: &crate::templates::Expanded,
    ) -> Result<bool, EditorError> {
        let lines = text.lines.iter().map(|l| Value::from(l.as_str())).collect();
        let marks = text
            .marks
            .iter()
            .map(|m| {
                Value::Map(vec![
                    ("name".into(), m.name.as_str().into()),
                    ("line".into(), (m.line as u64).into()),
                    ("start".into(), (m.start as u64).into()),
                    ("finish".into(), (m.end as u64).into()),
                ])
            })
            .collect();
        let done = self
            .draft_call(id, "insert", vec![Value::Array(lines), Value::Array(marks)])
            .await?;
        Ok(done.as_bool().unwrap_or(false))
    }

    /// Write `value` wherever the placeholder `name` stands, or the
    /// placeholder itself again when `value` is empty.
    pub async fn fill_placeholder(
        &self,
        id: &str,
        name: &str,
        value: &str,
    ) -> Result<(), EditorError> {
        let value: String = value.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.draft_call(id, "fill", vec![name.into(), value.as_str().into()])
            .await
            .map(|_| ())
    }

    /// Put the cursor where the placeholder `name` first stands; false when
    /// it stands nowhere.
    pub async fn goto_placeholder(&self, id: &str, name: &str) -> Result<bool, EditorError> {
        let done = self.draft_call(id, "go", vec![name.into()]).await?;
        Ok(done.as_bool().unwrap_or(false))
    }

    /// Open `path` in a tab of its own, beside the draft.
    pub async fn edit_file(&self, id: &str, path: &str) -> Result<(), EditorError> {
        let session = self.session(id)?;
        let code = r#"vim.cmd("tabedit " .. vim.fn.fnameescape(select(1, ...)))"#;
        bounded(session.nvim.exec_lua(code, vec![path.into()]))
            .await
            .map(|_| ())
    }

    /// Stop Neovim and remove its file. The draft has been read by now.
    pub async fn close(&self, id: &str) -> Result<(), EditorError> {
        let Some(session) = self.sessions.lock().unwrap().remove(id) else {
            return Ok(());
        };
        let child = session.child.lock().unwrap().take();
        if let Some(mut child) = child {
            // Asked to quit first, so Neovim removes its swap file; a kill
            // leaves one behind for every draft ever written. Killed only if
            // it does not go — a Neovim at a prompt answers nothing.
            let wait = std::time::Duration::from_millis(1500);
            let quit = session.nvim.command("qall!");
            let _ = tokio::time::timeout(wait, quit).await;
            if tokio::time::timeout(wait, child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(wait, child.wait()).await;
            }
        }
        let _ = std::fs::remove_file(&session.path);
        Ok(())
    }

    /// Stop every editor of a window, for when it goes away without saying
    /// so — a reload, or its web content dying.
    pub async fn close_owned_by(&self, owner: &str) {
        let ids: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| session.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = self.close(&id).await;
        }
    }

    /// Stop every editor, for when the app goes away.
    pub async fn close_all(&self) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.close(&id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNATURE: &str = "-- \nSam\nExample";

    fn round_trip(body: &str) -> (String, Option<Signature>, String) {
        let (text, signature) = split_body(body, Some(SIGNATURE));
        let back = match &signature {
            Some(signature) => put_signature(body_lines(&text), signature),
            None => body_lines(&text),
        };
        (text, signature, back.join("\n") + "\n")
    }

    #[test]
    fn a_signature_at_the_end_follows_the_end() {
        let body = "\nThanks!\n\n-- \nSam\nExample\n";
        let (text, signature, back) = round_trip(body);
        assert_eq!(text, "\nThanks!\n\n");
        assert_eq!(signature.unwrap().at, None);
        assert_eq!(back, body);

        let lines = body_lines("Thanks!\n\nAnd more.\n");
        let signature = Signature {
            lines: body_lines(SIGNATURE),
            at: None,
        };
        assert_eq!(
            put_signature(lines, &signature).join("\n"),
            "Thanks!\n\nAnd more.\n-- \nSam\nExample"
        );
    }

    #[test]
    fn a_signature_above_the_quote_stays_above_it() {
        let body = "\n\n\n-- \nSam\nExample\n\nOn Monday, Thomas wrote:\n> hello\n";
        let (text, signature, back) = round_trip(body);
        assert_eq!(text, "\n\n\n\nOn Monday, Thomas wrote:\n> hello\n");
        assert_eq!(signature.unwrap().at, Some(3));
        assert_eq!(back, body);
    }

    #[test]
    fn an_edited_signature_stays_in_the_text() {
        let body = "Thanks!\n\n-- \nSam\nExample AG\n";
        let (text, signature, _) = round_trip(body);
        assert_eq!(text, body);
        assert_eq!(signature, None);

        let (text, signature) = split_body(body, None);
        assert_eq!((text.as_str(), signature), (body, None));
    }

    #[test]
    fn a_signature_where_the_text_has_shrunk_goes_at_the_end() {
        let signature = Signature {
            lines: body_lines(SIGNATURE),
            at: Some(12),
        };
        assert_eq!(
            put_signature(body_lines("Thanks!\n"), &signature).join("\n"),
            "Thanks!\n-- \nSam\nExample"
        );
    }

    #[test]
    fn msgpack_reads_as_json() {
        let value = Value::Array(vec![
            "grid_line".into(),
            Value::Array(vec![1.into(), 0.into(), 3.into()]),
            Value::Map(vec![("bold".into(), true.into())]),
            Value::Ext(1, vec![0]),
            Value::Binary(b"ok".to_vec()),
        ]);
        assert_eq!(
            to_json(value),
            serde_json::json!(["grid_line", [1, 0, 3], {"bold": true}, null, "ok"])
        );
    }
}
