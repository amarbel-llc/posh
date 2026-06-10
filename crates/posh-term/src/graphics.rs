//! Kitty graphics protocol: APC `G` parsing, image storage, placements,
//! and OK/error acknowledgements.

use std::collections::HashMap;

use crate::base64;

/// 320 MB storage quota; oldest images are evicted past this.
const QUOTA_BYTES: usize = 320 * 1024 * 1024;

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

/// A placement of an image on the grid.
#[derive(Debug, Clone)]
pub struct Placement {
    pub image_id: u32,
    /// Client-assigned placement id (`p=`), 0 if unused.
    pub placement_id: u32,
    /// Cursor cell at placement time.
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
    pending: Option<Pending>,
    total_bytes: usize,
    next_seq: u64,
    next_id: u32,
}

const MAX_PENDING_PAYLOAD: usize = 430_000_000; // base64 of ~320 MB

impl GraphicsState {
    pub fn images(&self) -> &HashMap<u32, Image> {
        &self.images
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub(crate) fn reset(&mut self) {
        *self = GraphicsState::default();
    }

    /// Handles one APC payload (already stripped of the leading `G`).
    /// Returns a response to send, if any. `cursor` is the current cursor
    /// cell used for placements.
    pub(crate) fn dispatch(&mut self, data: &[u8], cursor: (u16, u16)) -> Option<String> {
        let (control, payload) = split_control(data);
        let cmd = parse_command(control)?;

        if let Some(mut pending) = self.pending.take() {
            // Continuation chunk of an m=1 transmission.
            if pending.payload.len() + payload.len() <= MAX_PENDING_PAYLOAD {
                pending.payload.extend_from_slice(payload);
            }
            if cmd.more == 1 {
                self.pending = Some(pending);
                return None;
            }
            return self.finish_transmission(pending.cmd, &pending.payload, cursor);
        }

        match cmd.action {
            b't' | b'T' | b'q' | 0 => {
                if cmd.more == 1 {
                    self.pending = Some(Pending {
                        cmd,
                        payload: payload.to_vec(),
                    });
                    None
                } else {
                    self.finish_transmission(cmd, payload, cursor)
                }
            }
            b'p' => self.place(&cmd, cursor),
            b'd' => {
                self.delete(&cmd);
                None
            }
            _ => respond(&cmd, "EINVAL:unknown action"),
        }
    }

    fn finish_transmission(
        &mut self,
        mut cmd: Command,
        payload: &[u8],
        cursor: (u16, u16),
    ) -> Option<String> {
        if cmd.transmission != b'd' && cmd.transmission != 0 {
            return respond(&cmd, "EINVAL:unsupported transmission medium");
        }
        let Some(data) = base64::decode(payload) else {
            return respond(&cmd, "EINVAL:invalid base64 payload");
        };
        let format = match cmd.format {
            24 => ImageFormat::Rgb,
            0 | 32 => ImageFormat::Rgba,
            100 => ImageFormat::Png,
            _ => return respond(&cmd, "EINVAL:unknown format"),
        };
        if matches!(format, ImageFormat::Rgb | ImageFormat::Rgba) {
            let bpp = if format == ImageFormat::Rgb { 3 } else { 4 };
            let expect = cmd.width as usize * cmd.height as usize * bpp;
            if cmd.width == 0 || cmd.height == 0 || data.len() != expect {
                return respond(&cmd, "EINVAL:payload size does not match dimensions");
            }
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
            return resp;
        }
        if let Some(old) = self.images.remove(&cmd.id) {
            self.total_bytes -= old.data.len();
        }
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
            self.add_placement(&cmd, cursor);
        }
        resp
    }

    fn enforce_quota(&mut self) {
        while self.total_bytes > QUOTA_BYTES {
            let Some(oldest) = self.images.values().min_by_key(|i| i.seq).map(|i| i.id) else {
                break;
            };
            if let Some(img) = self.images.remove(&oldest) {
                self.total_bytes -= img.data.len();
            }
            self.placements.retain(|p| p.image_id != oldest);
        }
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

    fn place(&mut self, cmd: &Command, cursor: (u16, u16)) -> Option<String> {
        match self.resolve_id(cmd) {
            Some(id) => {
                let mut cmd = cmd.clone();
                cmd.id = id;
                self.add_placement(&cmd, cursor);
                respond(&cmd, "OK")
            }
            None => respond(cmd, "ENOENT:no such image"),
        }
    }

    fn add_placement(&mut self, cmd: &Command, cursor: (u16, u16)) {
        // Replace an existing placement with the same ids.
        self.placements
            .retain(|p| !(p.image_id == cmd.id && p.placement_id == cmd.placement_id));
        self.placements.push(Placement {
            image_id: cmd.id,
            placement_id: cmd.placement_id,
            row: cursor.0,
            col: cursor.1,
            src_x: cmd.src_x,
            src_y: cmd.src_y,
            src_w: cmd.src_w,
            src_h: cmd.src_h,
            cols: cmd.cols,
            rows: cmd.rows,
            z: cmd.z,
        });
    }

    fn delete(&mut self, cmd: &Command) {
        match cmd.delete {
            0 | b'a' => self.placements.clear(),
            b'A' => {
                self.placements.clear();
                self.images.clear();
                self.total_bytes = 0;
            }
            b'i' | b'I' => {
                if let Some(id) = self.resolve_id(cmd) {
                    self.placements.retain(|p| {
                        p.image_id != id
                            || (cmd.placement_id != 0 && p.placement_id != cmd.placement_id)
                    });
                    if cmd.delete == b'I' {
                        if let Some(img) = self.images.remove(&id) {
                            self.total_bytes -= img.data.len();
                        }
                    }
                }
            }
            b'n' | b'N' => {
                let mut by_number = cmd.clone();
                by_number.id = 0;
                if let Some(id) = self.resolve_id(&by_number) {
                    self.placements.retain(|p| p.image_id != id);
                    if cmd.delete == b'N' {
                        if let Some(img) = self.images.remove(&id) {
                            self.total_bytes -= img.data.len();
                        }
                    }
                }
            }
            // Point/range/column/row deletes: parsed but treated as no-ops.
            _ => {}
        }
    }
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
            _ => {} // unknown keys ignored (X, Y, C, U, S, O, ...)
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

    #[test]
    fn transmit_and_ack() {
        let mut g = GraphicsState::default();
        let payload = rgba(2, 2);
        let resp = g.dispatch(format!("a=t,f=32,s=2,v=2,i=7;{payload}").as_bytes(), (0, 0));
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
        assert!(g
            .dispatch(format!("a=t,f=32,s=2,v=1,i=5,m=1;{a}").as_bytes(), (0, 0))
            .is_none());
        let resp = g.dispatch(format!("m=0;{b}").as_bytes(), (0, 0));
        assert_eq!(resp.unwrap(), "\x1b_Gi=5;OK\x1b\\");
        assert_eq!(g.images()[&5].data.len(), 8);
    }

    #[test]
    fn size_mismatch_is_error() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = g.dispatch(format!("a=t,f=32,s=9,v=9,i=2;{payload}").as_bytes(), (0, 0));
        assert!(resp.unwrap().contains("EINVAL"));
        assert!(g.images().is_empty());
    }

    #[test]
    fn png_stored_compressed() {
        let mut g = GraphicsState::default();
        let bytes = b"\x89PNG\r\n\x1a\nfake";
        let payload = base64::encode(bytes);
        g.dispatch(format!("a=t,f=100,i=1;{payload}").as_bytes(), (0, 0));
        let img = &g.images()[&1];
        assert_eq!(img.format, ImageFormat::Png);
        assert_eq!(img.data, bytes);
    }

    #[test]
    fn query_does_not_store() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = g.dispatch(
            format!("a=q,f=32,s=1,v=1,i=31;{payload}").as_bytes(),
            (0, 0),
        );
        assert_eq!(resp.unwrap(), "\x1b_Gi=31;OK\x1b\\");
        assert!(g.images().is_empty());
    }

    #[test]
    fn quiet_suppresses_ok() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = g.dispatch(
            format!("a=t,f=32,s=1,v=1,i=4,q=1;{payload}").as_bytes(),
            (0, 0),
        );
        assert!(resp.is_none());
        let resp = g.dispatch("a=t,f=32,s=1,v=1,i=4,q=1;!!!!".as_bytes(), (0, 0));
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
        let resp = g.dispatch(b"a=p,i=99;", (0, 0));
        assert!(resp.unwrap().contains("ENOENT"));
    }

    #[test]
    fn image_number_allocates_id() {
        let mut g = GraphicsState::default();
        let payload = rgba(1, 1);
        let resp = g.dispatch(
            format!("a=t,f=32,s=1,v=1,I=42;{payload}").as_bytes(),
            (0, 0),
        );
        let resp = resp.unwrap();
        assert!(resp.contains("i=1"), "{resp}");
        assert!(resp.contains("I=42"), "{resp}");
        // Placement by number resolves to the allocated id.
        g.dispatch(b"a=p,I=42;", (2, 3));
        assert_eq!(g.placements()[0].image_id, 1);
    }
}
