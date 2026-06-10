//! Kitty graphics protocol: APC `G` parsing, image storage, placements
//! (including relative placements), deletes, animation frame storage, and
//! OK/error acknowledgements.

use std::collections::HashMap;

use crate::base64;

/// 320 MB storage quota; oldest images are evicted past this.
const QUOTA_BYTES: usize = 320 * 1024 * 1024;

/// Placeholder cell size in pixels, matching the XTWINOPS report.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// f=24: raw RGB, 3 bytes per pixel.
    Rgb,
    /// f=32: raw RGBA, 4 bytes per pixel.
    Rgba,
    /// f=100: PNG, stored compressed without decoding.
    Png,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub id: u32,
    /// Client-assigned image number (`I=`), 0 if unused.
    pub number: u32,
    pub format: ImageFormat,
    /// Pixel dimensions; 0 for PNG (not decoded).
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Insertion sequence for quota eviction order.
    pub(crate) seq: u64,
}

/// One animation frame transmitted with `a=f`. Frames are stored verbatim;
/// composition and playback rendering are left to the caller.
#[derive(Debug, Clone)]
pub struct Frame {
    /// 1-based frame number (`r=`; new frames get the next free number).
    pub number: u32,
    /// Gap before the next frame in milliseconds (`z=`).
    pub gap_ms: i32,
    /// 1-based frame composed beneath this one (`c=`), 0 = none.
    pub base_frame: u32,
    /// Composition offset of this data within the frame (`x=`, `y=`).
    pub x: u32,
    pub y: u32,
    pub format: ImageFormat,
    /// Pixel dimensions of the transmitted block; 0 for PNG.
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Animation control state set with `a=a`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnimationState {
    /// `s=`: 1 = stopped, 2 = running (waiting for frames), 3 = looping.
    pub state: u8,
    /// `v=`: loop count (0 = unset, 1 = infinite per the kitty spec).
    pub loops: u32,
    /// `c=`: 1-based current frame.
    pub current_frame: u32,
}

/// A placement of an image on the grid. `row`/`col` are absolute: relative
/// placements (`P=`/`Q=`) are resolved against the parent chain when the
/// placement is created (a parent moving later does not re-resolve).
#[derive(Debug, Clone)]
pub struct Placement {
    pub image_id: u32,
    /// Client-assigned placement id (`p=`), 0 if unused.
    pub placement_id: u32,
    /// Cursor cell at placement time (or parent cell + offsets).
    pub row: u16,
    pub col: u16,
    /// Source rectangle crop in pixels (x, y, w, h); 0 = full.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Display size in cells (c=, r=); 0 = derive from pixels.
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    /// Parent for a relative placement (`P=`/`Q=`); 0 = absolute.
    pub parent_image: u32,
    pub parent_placement: u32,
    /// Cell offsets relative to the parent (`H=`/`V=`).
    pub h_off: i32,
    pub v_off: i32,
    /// Pixel offset within the first cell (`X=`/`Y=`).
    pub cell_x: u32,
    pub cell_y: u32,
    /// `U=1`: unicode-placeholder (virtual) placement; not drawn at the
    /// cursor and never moves it.
    pub unicode: bool,
}

/// Parsed control data of one APC G escape.
#[derive(Debug, Clone, Default)]
struct Command {
    action: u8,
    quiet: u8,
    format: u32,
    transmission: u8,
    width: u32,
    height: u32,
    id: u32,
    number: u32,
    placement_id: u32,
    more: u8,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    cols: u32,
    rows: u32,
    z: i32,
    delete: u8,
    /// C=1: do not move the cursor after `a=T`.
    no_cursor_move: bool,
    /// U=1: unicode-placeholder placement.
    unicode: bool,
    parent_image: u32,
    parent_placement: u32,
    h_off: i32,
    v_off: i32,
    cell_x: u32,
    cell_y: u32,
    /// O= / S=: byte offset and size for file transmissions.
    file_offset: u64,
    file_size: u64,
}

#[derive(Debug)]
struct Pending {
    cmd: Command,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
pub(crate) struct GraphicsState {
    images: HashMap<u32, Image>,
    placements: Vec<Placement>,
    frames: HashMap<u32, Vec<Frame>>,
    animations: HashMap<u32, AnimationState>,
    pending: Option<Pending>,
    total_bytes: usize,
    next_seq: u64,
    next_id: u32,
}

const MAX_PENDING_PAYLOAD: usize = 430_000_000; // base64 of ~320 MB

/// Cursor advance after `a=T`: the placement's extent in (cols, rows).
type Advance = Option<(u32, u32)>;

impl GraphicsState {
    pub fn images(&self) -> &HashMap<u32, Image> {
        &self.images
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn frames(&self, image_id: u32) -> &[Frame] {
        self.frames.get(&image_id).map(|v| &v[..]).unwrap_or(&[])
    }

    pub fn animation(&self, image_id: u32) -> Option<AnimationState> {
        self.animations.get(&image_id).copied()
    }

    pub(crate) fn reset(&mut self) {
        *self = GraphicsState::default();
    }

    /// Handles one APC payload (already stripped of the leading `G`).
    /// Returns a response to send (if any) and the cursor advance extent
    /// for an `a=T` placement. `cursor` is the current cursor cell.
    pub(crate) fn dispatch(
        &mut self,
        data: &[u8],
        cursor: (u16, u16),
    ) -> (Option<String>, Advance) {
        let (control, payload) = split_control(data);
        let Some(cmd) = parse_command(control) else {
            return (None, None);
        };

        if let Some(mut pending) = self.pending.take() {
            // Continuation chunk of an m=1 transmission.
            if pending.payload.len() + payload.len() <= MAX_PENDING_PAYLOAD {
                pending.payload.extend_from_slice(payload);
            }
            if cmd.more == 1 {
                self.pending = Some(pending);
                return (None, None);
            }
            return self.finish_transmission(pending.cmd, &pending.payload, cursor);
        }

        match cmd.action {
            b't' | b'T' | b'q' | b'f' | 0 => {
                if cmd.more == 1 {
                    self.pending = Some(Pending {
                        cmd,
                        payload: payload.to_vec(),
                    });
                    (None, None)
                } else {
                    self.finish_transmission(cmd, payload, cursor)
                }
            }
            b'p' => self.place(&cmd, cursor),
            b'd' => {
                self.delete(&cmd, cursor);
                (None, None)
            }
            b'a' => (self.animate(&cmd), None),
            b'c' => (self.compose(&cmd), None),
            _ => (respond(&cmd, "EINVAL:unknown action"), None),
        }
    }

    fn finish_transmission(
        &mut self,
        mut cmd: Command,
        payload: &[u8],
        cursor: (u16, u16),
    ) -> (Option<String>, Advance) {
        let data = match cmd.transmission {
            0 | b'd' => match base64::decode(payload) {
                Some(d) => d,
                None => return (respond(&cmd, "EINVAL:invalid base64 payload"), None),
            },
            b'f' | b't' => match load_file(&cmd, payload) {
                Ok(d) => d,
                Err(e) => return (respond(&cmd, e), None),
            },
            // Shared memory is not available in a sandboxed/portable
            // emulator; the spec-defined error code tells the client to
            // retransmit over the escape stream.
            b's' => {
                return (
                    respond(&cmd, "EUNSUPPORTED:shared memory unavailable"),
                    None,
                )
            }
            _ => {
                return (
                    respond(&cmd, "EINVAL:unsupported transmission medium"),
                    None,
                )
            }
        };
        let format = match cmd.format {
            24 => ImageFormat::Rgb,
            0 | 32 => ImageFormat::Rgba,
            100 => ImageFormat::Png,
            _ => return (respond(&cmd, "EINVAL:unknown format"), None),
        };
        if matches!(format, ImageFormat::Rgb | ImageFormat::Rgba) {
            let bpp = if format == ImageFormat::Rgb { 3 } else { 4 };
            let expect = cmd.width as usize * cmd.height as usize * bpp;
            if cmd.width == 0 || cmd.height == 0 || data.len() != expect {
                return (
                    respond(&cmd, "EINVAL:payload size does not match dimensions"),
                    None,
                );
            }
        }
        if cmd.action == b'f' {
            return (self.store_frame(&cmd, format, data), None);
        }
        let query = cmd.action == b'q';
        if cmd.id == 0 {
            self.next_id += 1;
            cmd.id = self.next_id;
        } else {
            self.next_id = self.next_id.max(cmd.id);
        }
        let resp = respond(&cmd, "OK");
        if query {
            return (resp, None);
        }
        self.remove_image_data(cmd.id);
        self.total_bytes += data.len();
        self.next_seq += 1;
        self.images.insert(
            cmd.id,
            Image {
                id: cmd.id,
                number: cmd.number,
                format,
                width: cmd.width,
                height: cmd.height,
                data,
                seq: self.next_seq,
            },
        );
        self.enforce_quota();
        if cmd.action == b'T' {
            return match self.add_placement(&cmd, cursor) {
                Ok(adv) => (resp, adv),
                Err(e) => (respond(&cmd, e), None),
            };
        }
        (resp, None)
    }

    /// `a=f`: store one animation frame for an existing image.
    fn store_frame(&mut self, cmd: &Command, format: ImageFormat, data: Vec<u8>) -> Option<String> {
        let Some(id) = self.resolve_id(cmd) else {
            return respond(cmd, "ENOENT:no such image");
        };
        let frames = self.frames.entry(id).or_default();
        // r= edits an existing frame; otherwise a new one is appended.
        let number = if cmd.rows != 0 {
            cmd.rows
        } else {
            frames.iter().map(|f| f.number).max().unwrap_or(0) + 1
        };
        let frame = Frame {
            number,
            gap_ms: cmd.z,
            base_frame: cmd.cols,
            x: cmd.src_x,
            y: cmd.src_y,
            format,
            width: cmd.width,
            height: cmd.height,
            data,
        };
        self.total_bytes += frame.data.len();
        if let Some(existing) = frames.iter_mut().find(|f| f.number == number) {
            self.total_bytes -= existing.data.len();
            *existing = frame;
        } else {
            frames.push(frame);
        }
        self.enforce_quota();
        respond(cmd, "OK")
    }

    /// `a=a`: animation control; stores the requested play state.
    fn animate(&mut self, cmd: &Command) -> Option<String> {
        let Some(id) = self.resolve_id(cmd) else {
            return respond(cmd, "ENOENT:no such image");
        };
        let st = self.animations.entry(id).or_default();
        // In this action s/v/c are state, loop count, and current frame.
        if cmd.width != 0 {
            st.state = cmd.width as u8;
        }
        if cmd.height != 0 {
            st.loops = cmd.height;
        }
        if cmd.cols != 0 {
            st.current_frame = cmd.cols;
        }
        respond(cmd, "OK")
    }

    /// `a=c`: frame composition request. Validated and acknowledged; the
    /// actual pixel composition is left to the renderer.
    fn compose(&mut self, cmd: &Command) -> Option<String> {
        let Some(id) = self.resolve_id(cmd) else {
            return respond(cmd, "ENOENT:no such image");
        };
        let frames = self.frames.get(&id).map(|v| &v[..]).unwrap_or(&[]);
        let have = |n: u32| n == 0 || frames.iter().any(|f| f.number == n);
        if !have(cmd.rows) || !have(cmd.cols) {
            return respond(cmd, "ENOENT:no such frame");
        }
        respond(cmd, "OK")
    }

    fn enforce_quota(&mut self) {
        while self.total_bytes > QUOTA_BYTES {
            let Some(oldest) = self.images.values().min_by_key(|i| i.seq).map(|i| i.id) else {
                break;
            };
            self.remove_image_data(oldest);
            self.placements.retain(|p| p.image_id != oldest);
        }
    }

    /// Drops an image's pixel data, frames, and animation state.
    fn remove_image_data(&mut self, id: u32) {
        if let Some(img) = self.images.remove(&id) {
            self.total_bytes -= img.data.len();
        }
        for frame in self.frames.remove(&id).unwrap_or_default() {
            self.total_bytes -= frame.data.len();
        }
        self.animations.remove(&id);
    }

    fn resolve_id(&self, cmd: &Command) -> Option<u32> {
        if cmd.id != 0 {
            self.images.contains_key(&cmd.id).then_some(cmd.id)
        } else if cmd.number != 0 {
            // Most recently transmitted image with this number.
            self.images
                .values()
                .filter(|i| i.number == cmd.number)
                .max_by_key(|i| i.seq)
                .map(|i| i.id)
        } else {
            None
        }
    }

    fn place(&mut self, cmd: &Command, cursor: (u16, u16)) -> (Option<String>, Advance) {
        match self.resolve_id(cmd) {
            Some(id) => {
                let mut cmd = cmd.clone();
                cmd.id = id;
                match self.add_placement(&cmd, cursor) {
                    Ok(adv) => (respond(&cmd, "OK"), adv),
                    Err(e) => (respond(&cmd, e), None),
                }
            }
            None => (respond(cmd, "ENOENT:no such image"), None),
        }
    }

    /// Extent of a placement in cells, deriving from pixel dimensions via
    /// the placeholder cell size when c=/r= are absent.
    fn extent(&self, p: &Placement) -> (u32, u32) {
        let (mut w, mut h) = (p.src_w, p.src_h);
        if let Some(img) = self.images.get(&p.image_id) {
            if w == 0 {
                w = img.width;
            }
            if h == 0 {
                h = img.height;
            }
        }
        let cols = if p.cols != 0 {
            p.cols
        } else {
            w.div_ceil(CELL_W).max(1)
        };
        let rows = if p.rows != 0 {
            p.rows
        } else {
            h.div_ceil(CELL_H).max(1)
        };
        (cols, rows)
    }

    fn find_placement(&self, image_id: u32, placement_id: u32) -> Option<&Placement> {
        self.placements.iter().find(|p| {
            p.image_id == image_id && (placement_id == 0 || p.placement_id == placement_id)
        })
    }

    /// Creates (or replaces) a placement, resolving relative positions.
    /// Returns the cursor-advance extent, or a kitty error code.
    fn add_placement(
        &mut self,
        cmd: &Command,
        cursor: (u16, u16),
    ) -> Result<Advance, &'static str> {
        let (row, col) = if cmd.parent_image != 0 {
            // Walk the parent chain to resolve and to reject cycles.
            let mut at = (cmd.parent_image, cmd.parent_placement);
            for _ in 0..self.placements.len() + 1 {
                if at == (cmd.id, cmd.placement_id) {
                    return Err("ECYCLE:relative placement cycle");
                }
                let Some(p) = self.find_placement(at.0, at.1) else {
                    break;
                };
                if p.parent_image == 0 {
                    break;
                }
                at = (p.parent_image, p.parent_placement);
            }
            let Some(parent) = self.find_placement(cmd.parent_image, cmd.parent_placement) else {
                return Err("ENOPARENT:no such parent placement");
            };
            let row = (i32::from(parent.row) + cmd.v_off).max(0) as u16;
            let col = (i32::from(parent.col) + cmd.h_off).max(0) as u16;
            (row, col)
        } else {
            cursor
        };
        // Replace an existing placement with the same ids.
        self.placements
            .retain(|p| !(p.image_id == cmd.id && p.placement_id == cmd.placement_id));
        let placement = Placement {
            image_id: cmd.id,
            placement_id: cmd.placement_id,
            row,
            col,
            src_x: cmd.src_x,
            src_y: cmd.src_y,
            src_w: cmd.src_w,
            src_h: cmd.src_h,
            cols: cmd.cols,
            rows: cmd.rows,
            z: cmd.z,
            parent_image: cmd.parent_image,
            parent_placement: cmd.parent_placement,
            h_off: cmd.h_off,
            v_off: cmd.v_off,
            cell_x: cmd.cell_x,
            cell_y: cmd.cell_y,
            unicode: cmd.unicode,
        };
        let advance = (!cmd.no_cursor_move && !cmd.unicode && cmd.parent_image == 0)
            .then(|| self.extent(&placement));
        self.placements.push(placement);
        Ok(advance)
    }

    fn intersects(&self, p: &Placement, row: u16, col: u16) -> bool {
        if p.unicode {
            return false; // virtual placements occupy no grid cells
        }
        let (cols, rows) = self.extent(p);
        u32::from(row) >= u32::from(p.row)
            && u32::from(row) < u32::from(p.row) + rows
            && u32::from(col) >= u32::from(p.col)
            && u32::from(col) < u32::from(p.col) + cols
    }

    /// Removes placements matching `pred`; with `free`, image data of
    /// images left without placements is dropped too (uppercase forms).
    fn delete_placements<F: Fn(&GraphicsState, &Placement) -> bool>(
        &mut self,
        free: bool,
        pred: F,
    ) {
        let mut touched: Vec<u32> = Vec::new();
        let mut kept = Vec::with_capacity(self.placements.len());
        for p in std::mem::take(&mut self.placements) {
            if pred(self, &p) {
                touched.push(p.image_id);
            } else {
                kept.push(p);
            }
        }
        self.placements = kept;
        if free {
            for id in touched {
                if !self.placements.iter().any(|p| p.image_id == id) {
                    self.remove_image_data(id);
                }
            }
        }
    }

    fn delete(&mut self, cmd: &Command, cursor: (u16, u16)) {
        let free = cmd.delete.is_ascii_uppercase();
        match cmd.delete.to_ascii_lowercase() {
            0 | b'a' => {
                self.placements.clear();
                if free {
                    self.images.clear();
                    self.frames.clear();
                    self.animations.clear();
                    self.total_bytes = 0;
                }
            }
            b'i' => {
                if let Some(id) = self.resolve_id(cmd) {
                    self.placements.retain(|p| {
                        p.image_id != id
                            || (cmd.placement_id != 0 && p.placement_id != cmd.placement_id)
                    });
                    if free {
                        self.remove_image_data(id);
                    }
                }
            }
            b'n' => {
                let mut by_number = cmd.clone();
                by_number.id = 0;
                if let Some(id) = self.resolve_id(&by_number) {
                    self.placements.retain(|p| p.image_id != id);
                    if free {
                        self.remove_image_data(id);
                    }
                }
            }
            // Positional forms address cells with 1-based x=/y= keys.
            b'c' => self.delete_placements(free, |g, p| g.intersects(p, cursor.0, cursor.1)),
            b'p' => {
                let (x, y) = (cmd.src_x, cmd.src_y);
                self.delete_placements(free, move |g, p| {
                    x >= 1 && y >= 1 && g.intersects(p, clamp_cell(y - 1), clamp_cell(x - 1))
                });
            }
            b'q' => {
                let (x, y, z) = (cmd.src_x, cmd.src_y, cmd.z);
                self.delete_placements(free, move |g, p| {
                    p.z == z
                        && x >= 1
                        && y >= 1
                        && g.intersects(p, clamp_cell(y - 1), clamp_cell(x - 1))
                });
            }
            b'x' => {
                let x = cmd.src_x;
                self.delete_placements(free, move |g, p| {
                    if x < 1 || p.unicode {
                        return false;
                    }
                    let (cols, _) = g.extent(p);
                    let col = x - 1;
                    col >= u32::from(p.col) && col < u32::from(p.col) + cols
                });
            }
            b'y' => {
                let y = cmd.src_y;
                self.delete_placements(free, move |g, p| {
                    if y < 1 || p.unicode {
                        return false;
                    }
                    let (_, rows) = g.extent(p);
                    let row = y - 1;
                    row >= u32::from(p.row) && row < u32::from(p.row) + rows
                });
            }
            b'z' => {
                let z = cmd.z;
                self.delete_placements(free, move |_, p| p.z == z);
            }
            _ => {}
        }
    }
}

/// Reads a `t=f`/`t=t` payload: the base64-encoded file path, honoring the
/// `O=` offset and `S=` size keys. A `t=t` temporary file is removed after
/// reading, but only when the path carries the spec's safety marker.
fn load_file(cmd: &Command, payload: &[u8]) -> Result<Vec<u8>, &'static str> {
    let Some(path_bytes) = base64::decode(payload) else {
        return Err("EINVAL:invalid base64 payload");
    };
    let Ok(path) = String::from_utf8(path_bytes) else {
        return Err("EINVAL:invalid file path");
    };
    let Ok(mut data) = std::fs::read(&path) else {
        return Err("ENOENT:could not read file");
    };
    if cmd.transmission == b't' && path.contains("tty-graphics-protocol") {
        let _ = std::fs::remove_file(&path);
    }
    let off = (cmd.file_offset as usize).min(data.len());
    data.drain(..off);
    if cmd.file_size != 0 {
        data.truncate(cmd.file_size as usize);
    }
    if data.len() > QUOTA_BYTES {
        return Err("EFBIG:file exceeds storage quota");
    }
    Ok(data)
}

fn clamp_cell(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

/// Splits `key=value,...;payload`.
fn split_control(data: &[u8]) -> (&[u8], &[u8]) {
    match data.iter().position(|&b| b == b';') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    }
}

fn parse_command(control: &[u8]) -> Option<Command> {
    let mut cmd = Command::default();
    for pair in control.split(|&b| b == b',') {
        if pair.is_empty() {
            continue;
        }
        let eq = pair.iter().position(|&b| b == b'=')?;
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        let int = || -> i64 {
            let s = std::str::from_utf8(value).unwrap_or("0");
            s.parse().unwrap_or(0)
        };
        let ch = || -> u8 { value.first().copied().unwrap_or(0) };
        match key {
            b"a" => cmd.action = ch(),
            b"q" => cmd.quiet = int() as u8,
            b"f" => cmd.format = int() as u32,
            b"t" => cmd.transmission = ch(),
            b"s" => cmd.width = int() as u32,
            b"v" => cmd.height = int() as u32,
            b"i" => cmd.id = int() as u32,
            b"I" => cmd.number = int() as u32,
            b"p" => cmd.placement_id = int() as u32,
            b"m" => cmd.more = int() as u8,
            b"x" => cmd.src_x = int() as u32,
            b"y" => cmd.src_y = int() as u32,
            b"w" => cmd.src_w = int() as u32,
            b"h" => cmd.src_h = int() as u32,
            b"c" => cmd.cols = int() as u32,
            b"r" => cmd.rows = int() as u32,
            b"z" => cmd.z = int() as i32,
            b"d" => cmd.delete = ch(),
            b"C" => cmd.no_cursor_move = int() == 1,
            b"U" => cmd.unicode = int() == 1,
            b"P" => cmd.parent_image = int() as u32,
            b"Q" => cmd.parent_placement = int() as u32,
            b"H" => cmd.h_off = int() as i32,
            b"V" => cmd.v_off = int() as i32,
            b"X" => cmd.cell_x = int() as u32,
            b"Y" => cmd.cell_y = int() as u32,
            b"O" => cmd.file_offset = int() as u64,
            b"S" => cmd.file_size = int() as u64,
            _ => {} // unknown keys ignored
        }
    }
    Some(cmd)
}

/// Formats an acknowledgement per the kitty spec:
/// `ESC _ G i=<id> ; <message> ESC \`. Suppressed when no id/number is
/// available to address the reply, or per the `q` (quiet) key.
fn respond(cmd: &Command, message: &str) -> Option<String> {
    if cmd.id == 0 && cmd.number == 0 {
        return None;
    }
    let ok = message == "OK";
    if cmd.quiet >= 2 || (ok && cmd.quiet == 1) {
        return None;
    }
    let mut keys = String::new();
    if cmd.id != 0 {
        keys.push_str(&format!("i={}", cmd.id));
    }
    if cmd.number != 0 {
        if !keys.is_empty() {
            keys.push(',');
        }
        keys.push_str(&format!("I={}", cmd.number));
    }
    Some(format!("\x1b_G{keys};{message}\x1b\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(w: usize, h: usize) -> String {
        base64::encode(&vec![0xAAu8; w * h * 4])
    }

    fn run(g: &mut GraphicsState, s: &str) -> Option<String> {
        g.dispatch(s.as_bytes(), (0, 0)).0
    }

    #[test]
    fn transmit_and_ack() {
        let mut g = GraphicsState::default();
        let payload = rgba(2, 2);
        let resp = run(&mut g, &format!("a=t,f=32,s=2,v=2,i=7;{payload}"));
        assert_eq!(resp.unwrap(), "\x1b_Gi=7;OK\x1b\\");
        let img = &g.images()[&7];
        assert_eq!(img.format, ImageFormat::Rgba);
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.data.len(), 16);
        assert!(g.placements().is_empty());
    }

    #[test]
    fn transmit_and_place() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        g.dispatch(
            format!("a=T,f=32,s=1,v=1,i=3,p=9,c=4,r=2,z=-1;{payload}").as_bytes(),
            (5, 10),
        );
        assert_eq!(g.placements().len(), 1);
        let p = &g.placements()[0];
        assert_eq!((p.image_id, p.placement_id), (3, 9));
        assert_eq!((p.row, p.col), (5, 10));
        assert_eq!((p.cols, p.rows), (4, 2));
        assert_eq!(p.z, -1);
    }

    #[test]
    fn chunked_transmission() {
        let mut g = GraphicsState::default();
        let full = rgba(2, 1);
        let (a, b) = full.split_at(4);
        assert!(run(&mut g, &format!("a=t,f=32,s=2,v=1,i=5,m=1;{a}")).is_none());
        let resp = run(&mut g, &format!("m=0;{b}"));
        assert_eq!(resp.unwrap(), "\x1b_Gi=5;OK\x1b\\");
        assert_eq!(g.images()[&5].data.len(), 8);
    }

    #[test]
    fn size_mismatch_is_error() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = run(&mut g, &format!("a=t,f=32,s=9,v=9,i=2;{payload}"));
        assert!(resp.unwrap().contains("EINVAL"));
        assert!(g.images().is_empty());
    }

    #[test]
    fn png_stored_compressed() {
        let mut g = GraphicsState::default();
        let bytes = b"\x89PNG\r\n\x1a\nfake";
        let payload = base64::encode(bytes);
        run(&mut g, &format!("a=t,f=100,i=1;{payload}"));
        let img = &g.images()[&1];
        assert_eq!(img.format, ImageFormat::Png);
        assert_eq!(img.data, bytes);
    }

    #[test]
    fn query_does_not_store() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = run(&mut g, &format!("a=q,f=32,s=1,v=1,i=31;{payload}"));
        assert_eq!(resp.unwrap(), "\x1b_Gi=31;OK\x1b\\");
        assert!(g.images().is_empty());
    }

    #[test]
    fn quiet_suppresses_ok() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = run(&mut g, &format!("a=t,f=32,s=1,v=1,i=4,q=1;{payload}"));
        assert!(resp.is_none());
        let resp = run(&mut g, "a=t,f=32,s=1,v=1,i=4,q=1;!!!!");
        assert!(resp.unwrap().contains("EINVAL")); // q=1 still reports errors
    }

    #[test]
    fn delete_actions() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        g.dispatch(format!("a=T,f=32,s=1,v=1,i=1;{payload}").as_bytes(), (0, 0));
        g.dispatch(format!("a=T,f=32,s=1,v=1,i=2;{payload}").as_bytes(), (1, 0));
        g.dispatch(b"a=d,d=i,i=1;", (0, 0));
        assert_eq!(g.placements().len(), 1);
        assert!(g.images().contains_key(&1)); // lowercase keeps data
        g.dispatch(b"a=d,d=I,i=1;", (0, 0));
        assert!(!g.images().contains_key(&1));
        g.dispatch(b"a=d,d=A;", (0, 0));
        assert!(g.images().is_empty());
        assert!(g.placements().is_empty());
    }

    #[test]
    fn place_missing_image_is_enoent() {
        let mut g = GraphicsState::default();
        let resp = run(&mut g, "a=p,i=99;");
        assert!(resp.unwrap().contains("ENOENT"));
    }

    #[test]
    fn image_number_allocates_id() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = run(&mut g, &format!("a=t,f=32,s=1,v=1,I=42;{payload}"));
        let resp = resp.unwrap();
        assert!(resp.contains("i=1"), "{resp}");
        assert!(resp.contains("I=42"), "{resp}");
        // Placement by number resolves to the allocated id.
        g.dispatch(b"a=p,I=42;", (2, 3));
        assert_eq!(g.placements()[0].image_id, 1);
    }

    // --- cursor advancement ------------------------------------------------

    #[test]
    fn place_reports_cursor_advance() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let (_, adv) = g.dispatch(
            format!("a=T,f=32,s=1,v=1,i=1,c=3,r=2;{payload}").as_bytes(),
            (0, 0),
        );
        assert_eq!(adv, Some((3, 2)));
    }

    #[test]
    fn c1_suppresses_cursor_advance() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let (_, adv) = g.dispatch(
            format!("a=T,f=32,s=1,v=1,i=1,C=1;{payload}").as_bytes(),
            (0, 0),
        );
        assert_eq!(adv, None);
    }

    #[test]
    fn extent_derives_from_pixels() {
        let mut g = GraphicsState::default();
        let payload = base64::encode(&vec![0u8; 25 * 41 * 4]);
        let (_, adv) = g.dispatch(
            format!("a=T,f=32,s=25,v=41,i=1;{payload}").as_bytes(),
            (0, 0),
        );
        // 25px / 10 -> 3 cols, 41px / 20 -> 3 rows.
        assert_eq!(adv, Some((3, 3)));
    }

    // --- delete specifiers ---------------------------------------------------

    fn placed(g: &mut GraphicsState, id: u32, at: (u16, u16), extra: &str) {
        let payload = rgba(1, 1);
        let resp = g.dispatch(
            format!("a=T,f=32,s=1,v=1,i={id},c=2,r=2{extra};{payload}").as_bytes(),
            at,
        );
        assert!(resp.0.unwrap().contains("OK"));
    }

    #[test]
    fn delete_at_cursor_cell() {
        let mut g = GraphicsState::default();
        placed(&mut g, 1, (0, 0), "");
        placed(&mut g, 2, (5, 5), "");
        g.dispatch(b"a=d,d=c;", (1, 1)); // inside image 1 (2x2 at 0,0)
        assert_eq!(g.placements().len(), 1);
        assert_eq!(g.placements()[0].image_id, 2);
        assert!(g.images().contains_key(&1)); // lowercase keeps data
    }

    #[test]
    fn delete_at_point_uppercase_frees() {
        let mut g = GraphicsState::default();
        placed(&mut g, 1, (2, 3), "");
        g.dispatch(b"a=d,d=P,x=4,y=3;", (0, 0)); // 1-based cell (4,3)
        assert!(g.placements().is_empty());
        assert!(g.images().is_empty());
    }

    #[test]
    fn delete_by_column_row_and_z() {
        let mut g = GraphicsState::default();
        placed(&mut g, 1, (0, 0), ",z=7");
        placed(&mut g, 2, (4, 4), ",z=9");
        g.dispatch(b"a=d,d=x,x=1;", (0, 0)); // column 1 hits image 1
        assert_eq!(g.placements().len(), 1);
        placed(&mut g, 1, (0, 0), ",z=7");
        g.dispatch(b"a=d,d=y,y=5;", (0, 0)); // row 5 hits image 2
        assert_eq!(g.placements().len(), 1);
        assert_eq!(g.placements()[0].image_id, 1);
        g.dispatch(b"a=d,d=z,z=7;", (0, 0));
        assert!(g.placements().is_empty());
        assert!(!g.images().is_empty()); // lowercase keeps data
        placed(&mut g, 2, (4, 4), ",z=9");
        g.dispatch(b"a=d,d=Z,z=9;", (0, 0));
        assert!(!g.images().contains_key(&2));
    }

    #[test]
    fn delete_newest_by_number() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        run(&mut g, &format!("a=T,f=32,s=1,v=1,I=5;{payload}"));
        run(&mut g, &format!("a=T,f=32,s=1,v=1,I=5;{payload}"));
        g.dispatch(b"a=d,d=N,I=5;", (0, 0));
        assert_eq!(g.images().len(), 1);
        assert_eq!(g.placements().len(), 1);
    }

    // --- relative placements ---------------------------------------------------

    #[test]
    fn relative_placement_resolves_parent_chain() {
        let mut g = GraphicsState::default();
        placed(&mut g, 1, (3, 4), ",p=1");
        let payload = rgba(1, 1);
        let resp = run(
            &mut g,
            &format!("a=T,f=32,s=1,v=1,i=2,p=1,P=1,Q=1,H=2,V=1;{payload}"),
        );
        assert!(resp.unwrap().contains("OK"));
        let p = g.placements().iter().find(|p| p.image_id == 2).unwrap();
        assert_eq!((p.row, p.col), (4, 6));
        assert_eq!((p.parent_image, p.parent_placement), (1, 1));
        // Grandchild resolves through the chain.
        let resp = run(
            &mut g,
            &format!("a=T,f=32,s=1,v=1,i=3,p=1,P=2,Q=1,H=1,V=1;{payload}"),
        );
        assert!(resp.unwrap().contains("OK"));
        let p = g.placements().iter().find(|p| p.image_id == 3).unwrap();
        assert_eq!((p.row, p.col), (5, 7));
    }

    #[test]
    fn relative_placement_missing_parent() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = run(
            &mut g,
            &format!("a=T,f=32,s=1,v=1,i=2,p=1,P=9,Q=9;{payload}"),
        );
        assert!(resp.unwrap().contains("ENOPARENT"));
    }

    #[test]
    fn relative_placement_cycle_rejected() {
        let mut g = GraphicsState::default();
        placed(&mut g, 1, (0, 0), ",p=1");
        let payload = rgba(1, 1);
        run(
            &mut g,
            &format!("a=T,f=32,s=1,v=1,i=2,p=1,P=1,Q=1,H=1;{payload}"),
        );
        // Re-place 1 relative to 2: 2's chain leads back to 1.
        let resp = run(&mut g, "a=p,i=1,p=1,P=2,Q=1;");
        assert!(resp.unwrap().contains("ECYCLE"));
    }

    #[test]
    fn unicode_placeholder_flag() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let (resp, adv) = g.dispatch(
            format!("a=T,f=32,s=1,v=1,i=1,U=1;{payload}").as_bytes(),
            (0, 0),
        );
        assert!(resp.unwrap().contains("OK"));
        assert_eq!(adv, None); // virtual placements never move the cursor
        assert!(g.placements()[0].unicode);
    }

    // --- animation -----------------------------------------------------------

    #[test]
    fn frame_transmission_and_storage() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        run(&mut g, &format!("a=t,f=32,s=1,v=1,i=6;{payload}"));
        let resp = run(&mut g, &format!("a=f,f=32,s=1,v=1,i=6,z=80;{payload}"));
        assert_eq!(resp.unwrap(), "\x1b_Gi=6;OK\x1b\\");
        let resp = run(
            &mut g,
            &format!("a=f,f=32,s=1,v=1,i=6,r=2,c=1,x=3,y=4,z=120;{payload}"),
        );
        assert!(resp.unwrap().contains("OK"));
        let frames = g.frames(6);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].number, 1);
        assert_eq!(frames[0].gap_ms, 80);
        assert_eq!(frames[1].number, 2);
        assert_eq!(frames[1].base_frame, 1);
        assert_eq!((frames[1].x, frames[1].y), (3, 4));
        assert_eq!(frames[1].gap_ms, 120);
    }

    #[test]
    fn frame_for_missing_image_is_enoent() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = run(&mut g, &format!("a=f,f=32,s=1,v=1,i=66;{payload}"));
        assert!(resp.unwrap().contains("ENOENT"));
    }

    #[test]
    fn animation_control_stores_state() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        run(&mut g, &format!("a=t,f=32,s=1,v=1,i=6;{payload}"));
        let resp = run(&mut g, "a=a,i=6,s=3,v=2;");
        assert!(resp.unwrap().contains("OK"));
        let st = g.animation(6).unwrap();
        assert_eq!(st.state, 3);
        assert_eq!(st.loops, 2);
        let resp = run(&mut g, "a=a,i=6,c=2;");
        assert!(resp.unwrap().contains("OK"));
        assert_eq!(g.animation(6).unwrap().current_frame, 2);
    }

    #[test]
    fn compose_validates_frames() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        run(&mut g, &format!("a=t,f=32,s=1,v=1,i=6;{payload}"));
        run(&mut g, &format!("a=f,f=32,s=1,v=1,i=6;{payload}"));
        run(&mut g, &format!("a=f,f=32,s=1,v=1,i=6;{payload}"));
        assert!(run(&mut g, "a=c,i=6,r=1,c=2;").unwrap().contains("OK"));
        assert!(run(&mut g, "a=c,i=6,r=9;").unwrap().contains("ENOENT"));
    }

    #[test]
    fn frame_delete_with_image() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        run(&mut g, &format!("a=t,f=32,s=1,v=1,i=6;{payload}"));
        run(&mut g, &format!("a=f,f=32,s=1,v=1,i=6;{payload}"));
        g.dispatch(b"a=d,d=I,i=6;", (0, 0));
        assert!(g.frames(6).is_empty());
        assert_eq!(g.total_bytes, 0);
    }

    // --- file / shared-memory transmission --------------------------------------

    #[test]
    fn file_transmission_loads_pixels() {
        let mut g = GraphicsState::default();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("posh-term-gfx-{}.rgba", std::process::id()));
        std::fs::write(&path, vec![0x55u8; 4]).unwrap();
        let payload = base64::encode(path.to_str().unwrap().as_bytes());
        let resp = run(&mut g, &format!("a=t,t=f,f=32,s=1,v=1,i=8;{payload}"));
        assert_eq!(resp.unwrap(), "\x1b_Gi=8;OK\x1b\\");
        assert_eq!(g.images()[&8].data, vec![0x55u8; 4]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn file_transmission_offset_and_size() {
        let mut g = GraphicsState::default();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("posh-term-gfx-os-{}.rgba", std::process::id()));
        std::fs::write(&path, (0u8..12).collect::<Vec<_>>()).unwrap();
        let payload = base64::encode(path.to_str().unwrap().as_bytes());
        let resp = run(
            &mut g,
            &format!("a=t,t=f,f=32,s=1,v=1,i=8,O=4,S=4;{payload}"),
        );
        assert!(resp.unwrap().contains("OK"));
        assert_eq!(g.images()[&8].data, vec![4, 5, 6, 7]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tempfile_transmission_deletes_marked_file() {
        let mut g = GraphicsState::default();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tty-graphics-protocol-{}.rgba", std::process::id()));
        std::fs::write(&path, vec![1u8; 4]).unwrap();
        let payload = base64::encode(path.to_str().unwrap().as_bytes());
        let resp = run(&mut g, &format!("a=t,t=t,f=32,s=1,v=1,i=8;{payload}"));
        assert!(resp.unwrap().contains("OK"));
        assert!(!path.exists());
    }

    #[test]
    fn missing_file_is_enoent() {
        let mut g = GraphicsState::default();
        let payload = base64::encode(b"/no/such/file/here.rgba");
        let resp = run(&mut g, &format!("a=t,t=f,f=32,s=1,v=1,i=8;{payload}"));
        assert!(resp.unwrap().contains("ENOENT"));
    }

    #[test]
    fn shared_memory_is_unsupported() {
        let mut g = GraphicsState::default();
        let payload = base64::encode(b"/posh-shm");
        let resp = run(&mut g, &format!("a=t,t=s,f=32,s=1,v=1,i=8;{payload}"));
        assert!(resp.unwrap().contains("EUNSUPPORTED"));
    }
}
