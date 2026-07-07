// gen-xwire - emits src/carrotconx/wire.rs. every x11 message carrot
// speaks is described here exactly once and codecs generate from it; the
// committed output is pinned to this source by hash, like the shaders.
//
// layouts follow the core protocol encoding: requests are
// [major, detail, len_units u16, fields...], replies are
// [1, detail, seq u16, extra_len u32, fields...] with a 32 byte floor,
// events are a fixed 32 byte frame [code, detail, seq u16, fields...].

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

// a fixed field after the 4-byte request header
#[derive(Copy, Clone)]
enum F {
    U8(&'static str),
    U16(&'static str),
    I16(&'static str),
    U32(&'static str),
    Pad(usize),
}

struct Req {
    fname: &'static str,
    major: u8,
    // Some(name) puts a u8 arg in the detail byte, None pads it
    detail: Option<&'static str>,
    fields: &'static [F],
    // appends a (bit, value) list behind a u32 mask written first
    value_mask: bool,
}

const SIMPLE_REQS: &[Req] = &[
    Req { fname: "create_window", major: 1, detail: Some("depth"), value_mask: true, fields: &[
        F::U32("wid"), F::U32("parent"), F::I16("x"), F::I16("y"), F::U16("width"),
        F::U16("height"), F::U16("border_width"), F::U16("class"), F::U32("visual"),
    ]},
    Req { fname: "change_window_attributes", major: 2, detail: None, value_mask: true, fields: &[
        F::U32("window"),
    ]},
    Req { fname: "map_window", major: 8, detail: None, value_mask: false, fields: &[
        F::U32("window"),
    ]},
    Req { fname: "get_geometry", major: 14, detail: None, value_mask: false, fields: &[
        F::U32("drawable"),
    ]},
    Req { fname: "get_atom_name", major: 17, detail: None, value_mask: false, fields: &[
        F::U32("atom"),
    ]},
    Req { fname: "get_property", major: 20, detail: Some("delete"), value_mask: false, fields: &[
        F::U32("window"), F::U32("property"), F::U32("ty"), F::U32("long_offset"),
        F::U32("long_length"),
    ]},
    Req { fname: "set_selection_owner", major: 22, detail: None, value_mask: false, fields: &[
        F::U32("owner"), F::U32("selection"), F::U32("time"),
    ]},
    Req { fname: "convert_selection", major: 24, detail: None, value_mask: false, fields: &[
        F::U32("requestor"), F::U32("selection"), F::U32("target"), F::U32("property"),
        F::U32("time"),
    ]},
    Req { fname: "set_input_focus", major: 42, detail: Some("revert_to"), value_mask: false, fields: &[
        F::U32("focus"), F::U32("time"),
    ]},
    Req { fname: "get_input_focus", major: 43, detail: None, value_mask: false, fields: &[] },
    Req { fname: "create_pixmap", major: 53, detail: Some("depth"), value_mask: false, fields: &[
        F::U32("pid"), F::U32("drawable"), F::U16("width"), F::U16("height"),
    ]},
    Req { fname: "free_pixmap", major: 54, detail: None, value_mask: false, fields: &[
        F::U32("pixmap"),
    ]},
    Req { fname: "create_gc", major: 55, detail: None, value_mask: true, fields: &[
        F::U32("cid"), F::U32("drawable"),
    ]},
    Req { fname: "free_gc", major: 60, detail: None, value_mask: false, fields: &[
        F::U32("gc"),
    ]},
    Req { fname: "create_cursor", major: 93, detail: None, value_mask: false, fields: &[
        F::U32("cid"), F::U32("source"), F::U32("mask"), F::U16("fore_red"),
        F::U16("fore_green"), F::U16("fore_blue"), F::U16("back_red"), F::U16("back_green"),
        F::U16("back_blue"), F::U16("x"), F::U16("y"),
    ]},
    Req { fname: "list_extensions", major: 99, detail: None, value_mask: false, fields: &[] },
    Req { fname: "kill_client", major: 113, detail: None, value_mask: false, fields: &[
        F::U32("resource"),
    ]},
];

// extension requests: the detail byte carries the minor opcode and the
// major arrives at runtime from QueryExtension
struct ExtReq {
    fname: &'static str,
    minor: u8,
    fields: &'static [F],
    value_mask: bool,
}

const EXT_REQS: &[ExtReq] = &[
    ExtReq { fname: "composite_redirect_subwindows", minor: 2, value_mask: false, fields: &[
        F::U32("window"), F::U8("update"), F::Pad(3),
    ]},
    ExtReq { fname: "xfixes_query_version", minor: 0, value_mask: false, fields: &[
        F::U32("client_major"), F::U32("client_minor"),
    ]},
    ExtReq { fname: "xfixes_select_selection_input", minor: 2, value_mask: false, fields: &[
        F::U32("window"), F::U32("selection"), F::U32("event_mask"),
    ]},
    ExtReq { fname: "render_query_pict_formats", minor: 1, value_mask: false, fields: &[] },
    ExtReq { fname: "render_create_picture", minor: 4, value_mask: true, fields: &[
        F::U32("pid"), F::U32("drawable"), F::U32("format"),
    ]},
    ExtReq { fname: "render_create_cursor", minor: 27, value_mask: false, fields: &[
        F::U32("cid"), F::U32("source"), F::U16("x"), F::U16("y"),
    ]},
];

// events: fixed offsets inside the 32 byte frame, detail byte included
struct Ev {
    variant: &'static str,
    code: u8,
    fields: &'static [(usize, &'static str, &'static str)], // offset, name, ty
}

const EVENTS: &[Ev] = &[
    Ev { variant: "CreateNotify", code: 16, fields: &[
        (4, "parent", "u32"), (8, "window", "u32"), (12, "x", "i16"), (14, "y", "i16"),
        (16, "width", "u16"), (18, "height", "u16"), (20, "border_width", "u16"),
        (22, "override_redirect", "bool"),
    ]},
    Ev { variant: "DestroyNotify", code: 17, fields: &[
        (4, "event", "u32"), (8, "window", "u32"),
    ]},
    Ev { variant: "UnmapNotify", code: 18, fields: &[
        (4, "event", "u32"), (8, "window", "u32"), (12, "from_configure", "bool"),
    ]},
    Ev { variant: "MapNotify", code: 19, fields: &[
        (4, "event", "u32"), (8, "window", "u32"), (12, "override_redirect", "bool"),
    ]},
    Ev { variant: "MapRequest", code: 20, fields: &[
        (4, "parent", "u32"), (8, "window", "u32"),
    ]},
    Ev { variant: "ConfigureNotify", code: 22, fields: &[
        (4, "event", "u32"), (8, "window", "u32"), (12, "above_sibling", "u32"),
        (16, "x", "i16"), (18, "y", "i16"), (20, "width", "u16"), (22, "height", "u16"),
        (24, "border_width", "u16"), (26, "override_redirect", "bool"),
    ]},
    Ev { variant: "ConfigureRequest", code: 23, fields: &[
        (1, "stack_mode", "u8"), (4, "parent", "u32"), (8, "window", "u32"),
        (12, "sibling", "u32"), (16, "x", "i16"), (18, "y", "i16"), (20, "width", "u16"),
        (22, "height", "u16"), (24, "border_width", "u16"), (26, "value_mask", "u16"),
    ]},
    Ev { variant: "PropertyNotify", code: 28, fields: &[
        (4, "window", "u32"), (8, "atom", "u32"), (12, "time", "u32"), (16, "state", "u8"),
    ]},
    Ev { variant: "FocusIn", code: 9, fields: &[
        (1, "detail", "u8"), (4, "event", "u32"), (8, "mode", "u8"),
    ]},
    Ev { variant: "SelectionRequest", code: 30, fields: &[
        (4, "time", "u32"), (8, "owner", "u32"), (12, "requestor", "u32"),
        (16, "selection", "u32"), (20, "target", "u32"), (24, "property", "u32"),
    ]},
    Ev { variant: "SelectionNotify", code: 31, fields: &[
        (4, "time", "u32"), (8, "requestor", "u32"), (12, "selection", "u32"),
        (16, "target", "u32"), (20, "property", "u32"),
    ]},
];

// xfixes SelectionNotify rides at first_event + 0 and is matched at runtime
const XFIXES_SELECTION_NOTIFY: Ev = Ev {
    variant: "XfixesSelectionNotify",
    code: 0,
    fields: &[
        (1, "subtype", "u8"), (4, "window", "u32"), (8, "owner", "u32"),
        (12, "selection", "u32"), (16, "timestamp", "u32"), (20, "selection_timestamp", "u32"),
    ],
};

const ERROR_NAMES: &[(u8, &str)] = &[
    (1, "Request"), (2, "Value"), (3, "Window"), (4, "Pixmap"), (5, "Atom"),
    (6, "Cursor"), (7, "Font"), (8, "Match"), (9, "Drawable"), (10, "Access"),
    (11, "Alloc"), (12, "Colormap"), (13, "GContext"), (14, "IDChoice"),
    (15, "Name"), (16, "Length"), (17, "Implementation"),
];

fn field_args(fields: &[F]) -> String {
    let mut out = String::new();
    for f in fields {
        match f {
            F::U8(n) => write!(out, ", {n}: u8").unwrap(),
            F::U16(n) => write!(out, ", {n}: u16").unwrap(),
            F::I16(n) => write!(out, ", {n}: i16").unwrap(),
            F::U32(n) => write!(out, ", {n}: u32").unwrap(),
            F::Pad(_) => {}
        }
    }
    out
}

fn field_pushes(fields: &[F]) -> String {
    let mut out = String::new();
    for f in fields {
        match f {
            F::U8(n) => writeln!(out, "    b.push({n});").unwrap(),
            F::U16(n) => writeln!(out, "    b.extend_from_slice(&{n}.to_ne_bytes());").unwrap(),
            F::I16(n) => writeln!(out, "    b.extend_from_slice(&{n}.to_ne_bytes());").unwrap(),
            F::U32(n) => writeln!(out, "    b.extend_from_slice(&{n}.to_ne_bytes());").unwrap(),
            F::Pad(n) => writeln!(out, "    b.extend_from_slice(&[0u8; {n}]);").unwrap(),
        }
    }
    out
}

fn main() {
    let gen_src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"),
    )
    .expect("read own source");
    let gen_hash: String = Sha256::digest(gen_src.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let mut w = String::new();
    w.push_str(
        "// GENERATED by tools/gen-xwire - do not edit\n\
         //\n\
         // x11 wire codecs, native endian: the setup handshake declares host\n\
         // byte order, so to_ne_bytes is correct by construction.\n\
         //\n\
         // the surface is generated ahead of use, so dead-code lints are noise\n\
         #![allow(dead_code)]\n\n",
    );

    // -- shared helpers --
    w.push_str(r#"fn begin(b: &mut Vec<u8>, major: u8, detail: u8) -> usize {
    let start = b.len();
    b.push(major);
    b.push(detail);
    b.extend_from_slice(&0u16.to_ne_bytes());
    start
}

fn finish(b: &mut Vec<u8>, start: usize) {
    while (b.len() - start) % 4 != 0 {
        b.push(0);
    }
    let units = ((b.len() - start) / 4) as u16;
    b[start + 2..start + 4].copy_from_slice(&units.to_ne_bytes());
}

// x value-lists: ascending bit order, one u32 per set bit
fn push_values(b: &mut Vec<u8>, values: &[(u8, u32)]) -> u32 {
    let mut sorted: Vec<(u8, u32)> = values.to_vec();
    sorted.sort_by_key(|v| v.0);
    let mut mask = 0u32;
    for (bit, _) in &sorted {
        mask |= 1 << bit;
    }
    for (_, v) in &sorted {
        b.extend_from_slice(&v.to_ne_bytes());
    }
    mask
}

fn u16at(f: &[u8], o: usize) -> u16 {
    u16::from_ne_bytes([f[o], f[o + 1]])
}

fn i16at(f: &[u8], o: usize) -> i16 {
    i16::from_ne_bytes([f[o], f[o + 1]])
}

fn u32at(f: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes([f[o], f[o + 1], f[o + 2], f[o + 3]])
}

"#);

    // -- simple core requests --
    w.push_str("// -- core requests --\n\n");
    for r in SIMPLE_REQS {
        let detail_expr = match r.detail {
            Some(name) => name.to_string(),
            None => "0".to_string(),
        };
        let detail_arg = match r.detail {
            Some(name) => format!(", {name}: u8"),
            None => String::new(),
        };
        let mask_arg = if r.value_mask { ", values: &[(u8, u32)]" } else { "" };
        writeln!(
            w,
            "pub fn {}(b: &mut Vec<u8>{}{}{}) {{",
            r.fname,
            detail_arg,
            field_args(r.fields),
            mask_arg
        )
        .unwrap();
        writeln!(w, "    let s = begin(b, {}, {});", r.major, detail_expr).unwrap();
        w.push_str(&field_pushes(r.fields));
        if r.value_mask {
            w.push_str(
                "    let mp = b.len();\n    b.extend_from_slice(&[0u8; 4]);\n    let mask = push_values(b, values);\n    b[mp..mp + 4].copy_from_slice(&mask.to_ne_bytes());\n",
            );
        }
        w.push_str("    finish(b, s);\n}\n\n");
    }

    // the irregulars, spelled out
    w.push_str(r#"// configure window carries a u16 mask with 2 bytes of pad
pub fn configure_window(b: &mut Vec<u8>, window: u32, values: &[(u8, u32)]) {
    let s = begin(b, 12, 0);
    b.extend_from_slice(&window.to_ne_bytes());
    let mp = b.len();
    b.extend_from_slice(&[0u8; 4]);
    let mask = push_values(b, values) as u16;
    b[mp..mp + 2].copy_from_slice(&mask.to_ne_bytes());
    finish(b, s);
}

pub fn intern_atom(b: &mut Vec<u8>, only_if_exists: bool, name: &[u8]) {
    let s = begin(b, 16, only_if_exists as u8);
    b.extend_from_slice(&(name.len() as u16).to_ne_bytes());
    b.extend_from_slice(&[0u8; 2]);
    b.extend_from_slice(name);
    finish(b, s);
}

pub fn query_extension(b: &mut Vec<u8>, name: &[u8]) {
    let s = begin(b, 98, 0);
    b.extend_from_slice(&(name.len() as u16).to_ne_bytes());
    b.extend_from_slice(&[0u8; 2]);
    b.extend_from_slice(name);
    finish(b, s);
}

// data_len is in format-sized units; data is raw bytes
pub fn change_property(
    b: &mut Vec<u8>,
    mode: u8,
    window: u32,
    property: u32,
    ty: u32,
    format: u8,
    data: &[u8],
) {
    let s = begin(b, 18, mode);
    b.extend_from_slice(&window.to_ne_bytes());
    b.extend_from_slice(&property.to_ne_bytes());
    b.extend_from_slice(&ty.to_ne_bytes());
    b.push(format);
    b.extend_from_slice(&[0u8; 3]);
    let units = data.len() as u32 / (format as u32 / 8).max(1);
    b.extend_from_slice(&units.to_ne_bytes());
    b.extend_from_slice(data);
    finish(b, s);
}

pub fn put_image(
    b: &mut Vec<u8>,
    format: u8,
    drawable: u32,
    gc: u32,
    width: u16,
    height: u16,
    dst_x: i16,
    dst_y: i16,
    depth: u8,
    data: &[u8],
) {
    let s = begin(b, 72, format);
    b.extend_from_slice(&drawable.to_ne_bytes());
    b.extend_from_slice(&gc.to_ne_bytes());
    b.extend_from_slice(&width.to_ne_bytes());
    b.extend_from_slice(&height.to_ne_bytes());
    b.extend_from_slice(&dst_x.to_ne_bytes());
    b.extend_from_slice(&dst_y.to_ne_bytes());
    b.push(0); // left_pad
    b.push(depth);
    b.extend_from_slice(&[0u8; 2]);
    b.extend_from_slice(data);
    finish(b, s);
}

pub fn send_event(b: &mut Vec<u8>, propagate: bool, destination: u32, event_mask: u32, event: &[u8; 32]) {
    let s = begin(b, 25, propagate as u8);
    b.extend_from_slice(&destination.to_ne_bytes());
    b.extend_from_slice(&event_mask.to_ne_bytes());
    b.extend_from_slice(event);
    finish(b, s);
}

// -- extension requests --

"#);
    for r in EXT_REQS {
        let mask_arg = if r.value_mask { ", values: &[(u8, u32)]" } else { "" };
        writeln!(
            w,
            "pub fn {}(b: &mut Vec<u8>, major: u8{}{}) {{",
            r.fname,
            field_args(r.fields),
            mask_arg
        )
        .unwrap();
        writeln!(w, "    let s = begin(b, major, {});", r.minor).unwrap();
        w.push_str(&field_pushes(r.fields));
        if r.value_mask {
            w.push_str(
                "    let mp = b.len();\n    b.extend_from_slice(&[0u8; 4]);\n    let mask = push_values(b, values);\n    b[mp..mp + 4].copy_from_slice(&mask.to_ne_bytes());\n",
            );
        }
        w.push_str("    finish(b, s);\n}\n\n");
    }

    w.push_str(r#"pub fn res_query_client_ids(b: &mut Vec<u8>, major: u8, specs: &[(u32, u32)]) {
    let s = begin(b, major, 4);
    b.extend_from_slice(&(specs.len() as u32).to_ne_bytes());
    for (client, mask) in specs {
        b.extend_from_slice(&client.to_ne_bytes());
        b.extend_from_slice(&mask.to_ne_bytes());
    }
    finish(b, s);
}

// -- replies --
// input is the whole frame: 32 byte header plus extra_len * 4 bytes

pub fn reply_extra_len(header: &[u8]) -> usize {
    u32at(header, 4) as usize * 4
}

pub struct GetInputFocusReply {
    pub revert_to: u8,
    pub focus: u32,
}

pub fn parse_get_input_focus(f: &[u8]) -> Option<GetInputFocusReply> {
    if f.len() < 32 {
        return None;
    }
    Some(GetInputFocusReply { revert_to: f[1], focus: u32at(f, 8) })
}

pub fn parse_intern_atom(f: &[u8]) -> Option<u32> {
    if f.len() < 32 {
        return None;
    }
    Some(u32at(f, 8))
}

pub fn parse_get_atom_name(f: &[u8]) -> Option<String> {
    if f.len() < 32 {
        return None;
    }
    let len = u16at(f, 8) as usize;
    let name = f.get(32..32 + len)?;
    Some(String::from_utf8_lossy(name).into_owned())
}

pub struct GetGeometryReply {
    pub depth: u8,
    pub root: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
}

pub fn parse_get_geometry(f: &[u8]) -> Option<GetGeometryReply> {
    if f.len() < 32 {
        return None;
    }
    Some(GetGeometryReply {
        depth: f[1],
        root: u32at(f, 8),
        x: i16at(f, 12),
        y: i16at(f, 14),
        width: u16at(f, 16),
        height: u16at(f, 18),
        border_width: u16at(f, 20),
    })
}

pub struct GetPropertyReply {
    pub format: u8,
    pub ty: u32,
    pub bytes_after: u32,
    pub data: Vec<u8>,
}

pub fn parse_get_property(f: &[u8]) -> Option<GetPropertyReply> {
    if f.len() < 32 {
        return None;
    }
    let format = f[1];
    let value_len = u32at(f, 16) as usize;
    let bytes = value_len * (format as usize / 8).max(if format == 0 { 0 } else { 1 });
    let data = f.get(32..32 + bytes)?.to_vec();
    Some(GetPropertyReply { format, ty: u32at(f, 8), bytes_after: u32at(f, 12), data })
}

pub struct QueryExtensionReply {
    pub present: bool,
    pub major_opcode: u8,
    pub first_event: u8,
    pub first_error: u8,
}

pub fn parse_query_extension(f: &[u8]) -> Option<QueryExtensionReply> {
    if f.len() < 32 {
        return None;
    }
    Some(QueryExtensionReply {
        present: f[8] != 0,
        major_opcode: f[9],
        first_event: f[10],
        first_error: f[11],
    })
}

pub fn parse_list_extensions(f: &[u8]) -> Option<Vec<String>> {
    if f.len() < 32 {
        return None;
    }
    let count = f[1] as usize;
    let mut names = Vec::with_capacity(count);
    let mut o = 32;
    for _ in 0..count {
        let len = *f.get(o)? as usize;
        let name = f.get(o + 1..o + 1 + len)?;
        names.push(String::from_utf8_lossy(name).into_owned());
        o += 1 + len;
    }
    Some(names)
}

pub struct XfixesVersionReply {
    pub major: u32,
    pub minor: u32,
}

pub fn parse_xfixes_query_version(f: &[u8]) -> Option<XfixesVersionReply> {
    if f.len() < 32 {
        return None;
    }
    Some(XfixesVersionReply { major: u32at(f, 8), minor: u32at(f, 12) })
}

pub struct PictFormat {
    pub id: u32,
    pub ty: u8,
    pub depth: u8,
    pub alpha_shift: u16,
    pub alpha_mask: u16,
}

// only the format table; the per-screen tail isn't needed to pick argb32
pub fn parse_render_query_pict_formats(f: &[u8]) -> Option<Vec<PictFormat>> {
    if f.len() < 32 {
        return None;
    }
    let count = u32at(f, 8) as usize;
    let mut out = Vec::with_capacity(count);
    let mut o = 32;
    for _ in 0..count {
        let rec = f.get(o..o + 28)?;
        out.push(PictFormat {
            id: u32at(rec, 0),
            ty: rec[4],
            depth: rec[5],
            alpha_shift: u16at(rec, 20),
            alpha_mask: u16at(rec, 22),
        });
        o += 28;
    }
    Some(out)
}

pub struct ClientIdValue {
    pub client: u32,
    pub mask: u32,
    pub values: Vec<u32>,
}

pub fn parse_res_query_client_ids(f: &[u8]) -> Option<Vec<ClientIdValue>> {
    if f.len() < 32 {
        return None;
    }
    let count = u32at(f, 8) as usize;
    let mut out = Vec::with_capacity(count);
    let mut o = 32;
    for _ in 0..count {
        let head = f.get(o..o + 12)?;
        let len_bytes = u32at(head, 8) as usize;
        let mut values = Vec::with_capacity(len_bytes / 4);
        let body = f.get(o + 12..o + 12 + len_bytes)?;
        for c in body.chunks_exact(4) {
            values.push(u32::from_ne_bytes([c[0], c[1], c[2], c[3]]));
        }
        out.push(ClientIdValue { client: u32at(head, 0), mask: u32at(head, 4), values });
        o += 12 + len_bytes;
    }
    Some(out)
}

// -- errors --

pub struct XError {
    pub code: u8,
    pub sequence: u16,
    pub bad_value: u32,
    pub minor: u16,
    pub major: u8,
}

pub fn parse_error(f: &[u8]) -> Option<XError> {
    if f.len() < 32 || f[0] != 0 {
        return None;
    }
    Some(XError {
        code: f[1],
        sequence: u16at(f, 2),
        bad_value: u32at(f, 4),
        minor: u16at(f, 8),
        major: f[10],
    })
}

"#);

    // error names
    w.push_str("pub fn error_name(code: u8) -> &'static str {\n    match code {\n");
    for (code, name) in ERROR_NAMES {
        writeln!(w, "        {code} => \"{name}\",").unwrap();
    }
    w.push_str("        _ => \"Unknown\",\n    }\n}\n\n");

    // -- events --
    w.push_str("// -- events --\n\n#[derive(Debug)]\npub enum XEvent {\n");
    for ev in EVENTS.iter().chain(std::iter::once(&XFIXES_SELECTION_NOTIFY)) {
        writeln!(w, "    {} {{", ev.variant).unwrap();
        for (_, name, ty) in ev.fields {
            writeln!(w, "        {name}: {ty},").unwrap();
        }
        w.push_str("    },\n");
    }
    w.push_str("    ClientMessage {\n        format: u8,\n        window: u32,\n        ty: u32,\n        data: [u32; 5],\n    },\n");
    w.push_str("}\n\n");

    w.push_str(
        "// the top bit marks send_event frames and is ignored here\n\
         pub fn parse_event(f: &[u8], xfixes_first_event: u8) -> Option<XEvent> {\n    \
         if f.len() < 32 {\n        return None;\n    }\n    \
         let code = f[0] & 0x7f;\n    \
         if xfixes_first_event != 0 && code == xfixes_first_event {\n",
    );
    w.push_str("        return ");
    w.push_str(event_arm(&XFIXES_SELECTION_NOTIFY, 2).trim_start());
    w.push_str(";\n");
    w.push_str("    }\n    match code {\n");
    w.push_str(
        "        33 => Some(XEvent::ClientMessage {\n            format: f[1],\n            window: u32at(f, 4),\n            ty: u32at(f, 8),\n            data: [\n                u32at(f, 12),\n                u32at(f, 16),\n                u32at(f, 20),\n                u32at(f, 24),\n                u32at(f, 28),\n            ],\n        }),\n",
    );
    for ev in EVENTS {
        writeln!(w, "        {} => {{", ev.code).unwrap();
        w.push_str(&event_arm(ev, 3));
        w.push_str("        }\n");
    }
    w.push_str("        _ => None,\n    }\n}\n\n");

    // event encoders for send_event payloads
    w.push_str(r#"pub fn encode_client_message(window: u32, ty: u32, format: u8, data: &[u32; 5]) -> [u8; 32] {
    let mut f = [0u8; 32];
    f[0] = 33;
    f[1] = format;
    f[4..8].copy_from_slice(&window.to_ne_bytes());
    f[8..12].copy_from_slice(&ty.to_ne_bytes());
    for (i, d) in data.iter().enumerate() {
        f[12 + i * 4..16 + i * 4].copy_from_slice(&d.to_ne_bytes());
    }
    f
}

// synthetic answer for a ConfigureRequest we won't grant; event=window
pub fn encode_configure_notify(window: u32, x: i16, y: i16, w: u16, h: u16, border: u16) -> [u8; 32] {
    let mut f = [0u8; 32];
    f[0] = 22;
    f[4..8].copy_from_slice(&window.to_ne_bytes());
    f[8..12].copy_from_slice(&window.to_ne_bytes());
    // above_sibling stays None
    f[16..18].copy_from_slice(&x.to_ne_bytes());
    f[18..20].copy_from_slice(&y.to_ne_bytes());
    f[20..22].copy_from_slice(&w.to_ne_bytes());
    f[22..24].copy_from_slice(&h.to_ne_bytes());
    f[24..26].copy_from_slice(&border.to_ne_bytes());
    f
}

pub fn encode_selection_notify(
    time: u32,
    requestor: u32,
    selection: u32,
    target: u32,
    property: u32,
) -> [u8; 32] {
    let mut f = [0u8; 32];
    f[0] = 31;
    f[4..8].copy_from_slice(&time.to_ne_bytes());
    f[8..12].copy_from_slice(&requestor.to_ne_bytes());
    f[12..16].copy_from_slice(&selection.to_ne_bytes());
    f[16..20].copy_from_slice(&target.to_ne_bytes());
    f[20..24].copy_from_slice(&property.to_ne_bytes());
    f
}

// -- the setup handshake --

pub fn encode_setup_request(b: &mut Vec<u8>, auth_name: &[u8], auth_data: &[u8]) {
    b.push(if cfg!(target_endian = "little") { 0x6c } else { 0x42 });
    b.push(0);
    b.extend_from_slice(&11u16.to_ne_bytes());
    b.extend_from_slice(&0u16.to_ne_bytes());
    b.extend_from_slice(&(auth_name.len() as u16).to_ne_bytes());
    b.extend_from_slice(&(auth_data.len() as u16).to_ne_bytes());
    b.extend_from_slice(&[0u8; 2]);
    b.extend_from_slice(auth_name);
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.extend_from_slice(auth_data);
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

// the first 8 bytes say how much more to read
pub fn setup_reply_len(prefix: &[u8]) -> Option<usize> {
    if prefix.len() < 8 {
        return None;
    }
    Some(u16at(prefix, 6) as usize * 4)
}

pub struct SetupScreen {
    pub root: u32,
    pub root_visual: u32,
    pub root_depth: u8,
    pub width: u16,
    pub height: u16,
}

pub struct Setup {
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub screens: Vec<SetupScreen>,
}

// f is the full reply including the 8 byte prefix; success must be 1
pub fn parse_setup(f: &[u8]) -> Option<Setup> {
    if f.len() < 40 || f[0] != 1 {
        return None;
    }
    let base = u32at(f, 12);
    let mask = u32at(f, 16);
    let vendor_len = u16at(f, 24) as usize;
    let n_screens = f[28] as usize;
    let n_formats = f[29] as usize;
    let mut o = 40 + vendor_len.div_ceil(4) * 4 + n_formats * 8;
    let mut screens = Vec::with_capacity(n_screens);
    for _ in 0..n_screens {
        let s = f.get(o..o + 40)?;
        screens.push(SetupScreen {
            root: u32at(s, 0),
            root_visual: u32at(s, 32),
            root_depth: s[38],
            width: u16at(s, 20),
            height: u16at(s, 22),
        });
        let n_depths = s[39] as usize;
        o += 40;
        for _ in 0..n_depths {
            let d = f.get(o..o + 8)?;
            let n_visuals = u16at(d, 2) as usize;
            o += 8 + n_visuals * 24;
        }
    }
    Some(Setup { resource_id_base: base, resource_id_mask: mask, screens })
}
"#);

    // freshness test, shaders-style
    write!(
        w,
        r#"
#[cfg(test)]
mod freshness {{
    use sha2::{{Digest, Sha256}};

    // pins tools/gen-xwire/src/main.rs as of the last regeneration
    const GEN_SRC_HASH: &str = "{gen_hash}";
    const REGEN: &str =
        "x11 wire codecs out of date - rerun: cargo run --manifest-path tools/gen-xwire/Cargo.toml";

    #[test]
    fn generator_matches_committed_codecs() {{
        let gen_src = include_str!("../../tools/gen-xwire/src/main.rs");
        let hash: String = Sha256::digest(gen_src.as_bytes())
            .iter()
            .map(|b| format!("{{b:02x}}"))
            .collect();
        assert_eq!(GEN_SRC_HASH, hash, "{{REGEN}}");
    }}
}}
"#
    )
    .unwrap();

    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../src/carrotconx/wire.rs");
    std::fs::write(&out, &w).unwrap();
    println!("wrote wire.rs ({} lines)", w.lines().count());
}

fn event_arm(ev: &Ev, indent: usize) -> String {
    let pad = "    ".repeat(indent);
    let mut out = String::new();
    writeln!(out, "{pad}Some(XEvent::{} {{", ev.variant).unwrap();
    for (off, name, ty) in ev.fields {
        let read = match *ty {
            "u8" => format!("f[{off}]"),
            "bool" => format!("f[{off}] != 0"),
            "u16" => format!("u16at(f, {off})"),
            "i16" => format!("i16at(f, {off})"),
            "u32" => format!("u32at(f, {off})"),
            other => panic!("unhandled event field type {other}"),
        };
        writeln!(out, "{pad}    {name}: {read},").unwrap();
    }
    writeln!(out, "{pad}}})").unwrap();
    out
}
